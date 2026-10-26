#!/usr/bin/env python3
"""Minimal MCP client over stdio, for trying the SimGDB tools by hand or from
an agent that cannot attach MCP servers itself.

  mcp_call.py INDEX                     list tools with descriptions and parameters
  mcp_call.py INDEX TOOL '{"at": 100}'   call a tool, print its JSON answer

Environment: SIMGDB (default: simgdb on PATH).
"""
import json
import os
import signal
import subprocess
import sys


def main():
    signal.signal(signal.SIGPIPE, signal.SIG_DFL)
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    index = sys.argv[1]
    proc = subprocess.Popen([os.environ.get("SIMGDB", "simgdb"), "mcp", index], stdin=subprocess.PIPE,
                            stdout=subprocess.PIPE, universal_newlines=True)

    def send(msg):
        proc.stdin.write(json.dumps(msg) + "\n")
        proc.stdin.flush()

    def recv(want_id):
        for line in proc.stdout:
            msg = json.loads(line)
            if msg.get("id") == want_id:
                return msg
        sys.exit("server closed")

    send({"jsonrpc": "2.0", "id": 1, "method": "initialize",
          "params": {"protocolVersion": "2025-06-18", "capabilities": {},
                     "clientInfo": {"name": "mcp_call", "version": "1"}}})
    init = recv(1)
    send({"jsonrpc": "2.0", "method": "notifications/initialized"})
    if len(sys.argv) == 2:
        print(init["result"].get("instructions", ""))
        send({"jsonrpc": "2.0", "id": 2, "method": "tools/list"})
        for t in recv(2)["result"]["tools"]:
            props = t.get("inputSchema", {}).get("properties", {})
            print("\n{}: {}".format(t["name"], t.get("description", "")))
            for name, p in props.items():
                print("    {}: {}".format(name, p.get("description", "")))
    else:
        args = json.loads(sys.argv[3]) if len(sys.argv) > 3 else {}
        send({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": sys.argv[2], "arguments": args}})
        res = recv(2)
        if "error" in res:
            print("protocol error:", res["error"])
        else:
            r = res["result"]
            if r.get("isError"):
                print("TOOL ERROR:", end=" ")
            for c in r.get("content", []):
                print(c.get("text", ""))
    proc.stdin.close()
    proc.wait(timeout=10)


if __name__ == "__main__":
    main()
