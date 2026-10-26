//! The MCP tools through a real MCP client over an in-memory stream: every
//! tool answers, answers equal the engine's (the front end adds no logic),
//! a few are checked against the independent query answers
//! (tests/queries/expected), errors come back as tool errors the client
//! can act on, and calls are logged.

use std::path::PathBuf;
use std::sync::Arc;

use rmcp::model::{CallToolRequestParams, ClientConfig};
use rmcp::{ClientHandler, ServiceExt};
use serde_json::{Value, json};
use simgdb_engine::Engine;
use simgdb_mcp::SimGdbMcp;
use simgdb_store::annotations::Annotations;
use simgdb_store::{IndexOptions, build};

#[derive(Clone, Default)]
struct Client;

impl ClientHandler for Client {
    fn get_info(&self) -> ClientConfig {
        ClientConfig::default()
    }
}

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests")
}

#[tokio::test(flavor = "multi_thread")]
async fn tools_answer_like_the_engine() {
    let dir = tempfile::tempdir().unwrap();
    let isa = simgdb_isa::by_name("arm-m").unwrap();
    build(
        isa.as_ref(),
        &IndexOptions {
            trace: root().join("firmware/traces/state_m4.m4rtl.log.gz"),
            elf: Some(root().join("firmware/state_m4.elf")),
            out: dir.path().to_path_buf(),
            ..Default::default()
        },
    )
    .unwrap();
    let engine = Arc::new(Engine::open(dir.path()).unwrap());
    let notes = Annotations::open(&dir.path().join("annotations.sqlite")).unwrap();
    let server = SimGdbMcp::new(engine.clone(), Some(notes));
    let (server_io, client_io) = tokio::io::duplex(1 << 20);
    tokio::spawn(async move { server.serve(server_io).await.unwrap().waiting().await });
    let client = Client.serve(client_io).await.unwrap();

    let tools = client.list_all_tools().await.unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
    for want in [
        "trace_info",
        "locate",
        "registers",
        "read_memory",
        "read_variable",
        "list_variables",
        "find_executions",
        "call_tree",
        "exceptions",
        "last_write",
        "writes_to",
        "find_first",
        "diff_state",
        "bookmark_add",
        "bookmark_list",
        "register_history",
    ] {
        assert!(names.contains(&want), "{want} missing from {names:?}");
    }
    for t in &tools {
        assert!(t.description.as_ref().is_some_and(|d| d.len() > 40), "{} needs a description", t.name);
    }

    let call = |name: &'static str, args: Value| {
        let client = &client;
        async move {
            let r = client
                .call_tool(CallToolRequestParams::new(name).with_arguments(args.as_object().unwrap().clone()))
                .await
                .unwrap();
            let text = r.content.first().and_then(|c| c.as_text()).map(|t| t.text.clone()).unwrap_or_default();
            (r.is_error.unwrap_or(false), text)
        }
    };
    let ok = |(err, text): (bool, String)| -> Value {
        assert!(!err, "tool error: {text}");
        serde_json::from_str(&text).unwrap()
    };
    let expected: Value =
        serde_json::from_str(&std::fs::read_to_string(root().join("queries/expected/state_m4.json")).unwrap()).unwrap();

    let info = ok(call("trace_info", json!({})).await);
    assert_eq!(info["info"]["instructions"], 4748);
    assert_eq!(info["exceptions"]["SysTick"].as_u64().unwrap() + info["exceptions"]["SVCall"].as_u64().unwrap(), 53);

    // Same answers as the engine.
    let loc = ok(call("locate", json!({"at": 1080})).await);
    let want = engine.locate(1080).unwrap();
    assert_eq!(loc["pc"]["value"], format!("{:#x}", want.pc.value.unwrap()));
    assert_eq!(loc["call_stack"].as_array().unwrap().len(), want.call_stack.len());
    assert!(loc["where"].as_str().unwrap().starts_with("SysTick_Handler (state.c:"), "{loc}");
    assert_eq!(loc["call_stack"][1]["at"].as_str().unwrap().split(' ').next(), Some("build_list"));
    let full = ok(call("locate", json!({"at": 1080, "detail": true})).await);
    assert_eq!(full["call_stack"][1]["pc_from"], want.call_stack[1].pc_from);
    assert_eq!(full["call_stack"][1]["pc"], format!("{:#x}", want.call_stack[1].pc.unwrap()));
    let regs = ok(call("registers", json!({"at": "0x438", "names": ["SP", "pc"]})).await);
    assert_eq!(regs["point"]["index"], 0x438);
    assert_eq!(regs["registers"].as_array().unwrap().len(), 2);
    let pc = regs["registers"].as_array().unwrap().iter().find(|r| r["name"] == "pc").unwrap().clone();
    assert_eq!(pc["hex"], format!("{:#x}", engine.pc(0x438).unwrap().value.unwrap()));
    let mem = ok(call("read_memory", json!({"var": "ticks", "at": 3000})).await);
    let raw = engine.read_variable("ticks", 3000).unwrap();
    assert_eq!(mem["hex"], raw.hex);
    assert_eq!(mem["variable"], "ticks");
    assert!(mem.get("bytes").is_none() && mem["sources"][0]["len"] == 4, "{mem}");
    let mem = ok(call("read_memory", json!({"var": "ticks", "at": 3000, "detail": true})).await);
    assert_eq!(mem["bytes"].as_array().unwrap().len(), 4);
    let r4 = ok(call("register_history", json!({"name": "r4", "limit": 3})).await);
    assert_eq!(r4["items"].as_array().unwrap().len(), 3);
    assert!(r4["items"][0]["hex"].as_str().unwrap().starts_with("0x"));
    let var = ok(call("read_variable", json!({"path": "pool[3].len", "time": 9001})).await);
    assert_eq!(var["type"], "uint16_t");
    let vars = ok(call("list_variables", json!({"filter": "ring"})).await);
    assert_eq!(vars["total"], 1);

    // Against the independent answers.
    let mix = ok(call("find_executions", json!({"target": "mix", "limit": 3})).await);
    assert_eq!(mix["entries"]["total"], expected["calls"]["mix"]["calls"]);
    assert_eq!(mix["entries"]["items"].as_array().unwrap().len(), 3);
    let line = ok(call("find_executions", json!({"target": "state.c:111", "limit": 1000})).await);
    assert_eq!(line["entries"]["items"], expected["line_visits"]["state.c:111"]);
    let exc = ok(call("exceptions", json!({"limit": 2, "cursor": 1})).await);
    assert_eq!(exc["exceptions"]["items"][0]["entered_at"], expected["exceptions"][1]["entry"]);
    let svc = ok(call("exceptions", json!({"kind": "SVCall", "limit": 1})).await);
    let first_svc = expected["exceptions"].as_array().unwrap().iter().find(|x| x["number"] == 11).unwrap().clone();
    assert_eq!(svc["exceptions"]["items"][0]["entered_at"], first_svc["entry"]);
    assert_eq!(svc["exceptions"]["items"][0]["taken_after"]["function"], "work");
    assert_eq!(
        svc["exceptions"]["items"][0]["interrupted_pc"],
        format!("{:#x}", first_svc["interrupted_pc"].as_u64().unwrap())
    );
    let tree = ok(call("call_tree", json!({"depth": 2})).await);
    let work = tree["functions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["function"] == "work" && f["kind"] == "call")
        .unwrap()
        .clone();
    assert!(tree["functions"].as_array().unwrap().iter().any(|f| f["kind"] == "exception"));
    assert_eq!(work["inclusive_instructions"], expected["calls"]["work"]["inclusive"]);
    let lw = ok(call("last_write", json!({"var": "ticks", "at": 3000})).await);
    assert_eq!(lw["writes"].as_array().unwrap().len(), 1);
    assert_eq!(lw["writes"][0]["function"], "SysTick_Handler");
    let w = &expected["writes_to"][0];
    let writes = ok(call(
        "writes_to",
        json!({"var": "ring", "from": w["from"], "to": w["to"].as_u64().unwrap() - 1, "limit": 1000}),
    )
    .await);
    assert_eq!(writes["writes"]["total"].as_u64().unwrap() as usize, w["writes"].as_array().unwrap().len());
    let f = &expected["find_first"][8];
    let found = ok(call(
        "find_first",
        json!({"condition": f["condition"], "from": f["from"], "to": f["to"],
                                             "direction": f["direction"]}),
    )
    .await);
    assert_eq!(found["found"]["index"], f["found"]);
    assert!(found["source"]["function"].is_string());
    let svc3 = ok(call("find_first", json!({"condition": "var(\"svc_calls\") == 3"})).await);
    assert_eq!(svc3["previous"]["holds"], false);
    assert_eq!(svc3["previous"]["function"], "SVC_Handler");
    assert_eq!(svc3["undecided_ranges"][0][0], 0);
    let d = &expected["diff"][0];
    let diff = ok(call("diff_state", json!({"a": d["a"], "b": d["b"]})).await);
    assert!(diff["memory"]["total"].as_u64().unwrap() > 0);
    let changed: Vec<&str> = diff["variables"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|v| v["changed"].as_u64().unwrap() > 0)
        .map(|v| v["variable"].as_str().unwrap())
        .collect();
    let q: Value =
        serde_json::from_str(&std::fs::read_to_string(root().join("ai_eval/questions.json")).unwrap()).unwrap();
    let want: Vec<String> =
        q["questions"].as_array().unwrap().iter().find(|x| x["id"] == "diff_vars").unwrap()["accept"]
            .as_array()
            .unwrap()
            .iter()
            .map(|g| g[0].as_str().unwrap().to_string())
            .collect();
    let ticks = diff["variables"].as_array().unwrap().iter().find(|v| v["variable"] == "ticks").unwrap().clone();
    assert!(ticks["b"].as_u64().unwrap() > ticks["a"].as_u64().unwrap(), "{ticks}");
    let only = ok(call("diff_state", json!({"a": d["a"], "b": d["b"], "variables_only": true})).await);
    assert!(only.get("memory").is_none());
    let mut got: Vec<String> = changed.iter().map(|s| s.to_string()).collect();
    got.sort();
    assert_eq!(got, want);
    assert!(
        diff["variables"].as_array().unwrap().iter().any(|v| v["paths"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p.as_str().unwrap().starts_with("pool[")))
    );

    // Bookmarks round trip and are shared through the database.
    ok(call("bookmark_add", json!({"at": 1080, "note": "first SysTick"})).await);
    let list = ok(call("bookmark_list", json!({})).await);
    assert_eq!(list["bookmarks"][0]["note"], "first SysTick");
    assert_eq!(list["bookmarks"][0]["author"], "ai");

    // Errors are tool errors with a usable message.
    for (name, args, needle) in [
        ("find_first", json!({"condition": "reg(\"r99\") == 1"}), "r99"),
        ("locate", json!({}), "give `at`"),
        ("locate", json!({"at": 999999}), "outside"),
        ("read_variable", json!({"path": "nope", "at": 1}), "nope"),
        ("read_memory", json!({"addr": "0xzz", "at": 1}), "not a number"),
        ("find_first", json!({"condition": "pc == 1", "direction": "sideways"}), "direction"),
    ] {
        let (err, text) = call(name, args).await;
        assert!(err && text.contains(needle), "{name}: {text}");
    }

    // Every call was logged.
    let log = Annotations::open(&dir.path().join("annotations.sqlite")).unwrap().calls().unwrap();
    assert!(log.len() >= 20, "{} calls logged", log.len());
    assert!(log.iter().any(|(tool, _, ok, _)| tool == "find_first" && !ok));
    client.cancel().await.unwrap();
}
