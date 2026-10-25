//! Storage format comparison for the SimGDB index (docs/benchmarks/storage.md).
//!
//! Generates a deterministic synthetic trace, stores the same tables as
//! (a) Arrow IPC files read through mmap, (b) Parquet (zstd) loaded into memory,
//! (c) SQLite with B-tree indexes, then times the queries the engine needs:
//! point queries (registers / memory word at an index) and scans.
//!
//! Usage: storage-formats [instructions] [out_dir]

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::sync::Arc;
use std::time::Instant;

use arrow_array::types::{ArrowPrimitiveType, UInt8Type, UInt32Type, UInt64Type};
use arrow_array::{
    Array, ArrayRef, PrimitiveArray, RecordBatch, UInt8Array, UInt32Array, UInt64Array,
};
use arrow_buffer::Buffer;
use arrow_ipc::convert::try_fb_to_schema;
use arrow_ipc::reader::{FileDecoder, read_footer_length};
use arrow_ipc::root_as_footer;
use arrow_ipc::writer::FileWriter;
use arrow_schema::{Field, Schema};

const CHECKPOINT: u64 = 4096;
const BATCH: usize = 1 << 20;
const NREG: usize = 17; // r0..r15, xpsr

// ---------------------------------------------------------------- data

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

#[derive(Default)]
struct Trace {
    // instructions (index implicit)
    pc: Vec<u32>,
    time: Vec<u64>,
    flags: Vec<u8>,
    opcode: Vec<u32>,
    // register writes
    rw_idx: Vec<u64>,
    rw_reg: Vec<u8>,
    rw_val: Vec<u32>,
    // memory accesses
    m_idx: Vec<u64>,
    m_addr: Vec<u32>,
    m_size: Vec<u8>,
    m_write: Vec<u8>,
    m_val: Vec<u64>,
    // checkpoints: registers before instruction cp*CHECKPOINT, and reg-write row to replay from
    cp_regs: Vec<Vec<u32>>, // NREG columns
    cp_rw_start: Vec<u64>,
    // inverse indexes (sorted)
    pi_pc: Vec<u32>,
    pi_idx: Vec<u64>,
    wi_granule: Vec<u32>,
    wi_idx: Vec<u64>,
    wi_row: Vec<u64>,
}

fn generate(n: u64) -> Trace {
    let mut r = Rng(0x9E37_79B9_7F4A_7C15);
    let mut t = Trace {
        cp_regs: vec![Vec::new(); NREG],
        ..Default::default()
    };
    let blocks: Vec<u32> = (0..50_000u32).map(|b| 0x0000_1000 + b * 24).collect();
    let mut regs = [0u32; NREG];
    let mut block = 0usize;
    let mut slot = 0u32;
    for i in 0..n {
        if i % CHECKPOINT == 0 {
            for (k, col) in t.cp_regs.iter_mut().enumerate() {
                col.push(regs[k]);
            }
            t.cp_rw_start.push(t.rw_idx.len() as u64);
        }
        if slot == 6 {
            // Skewed jumps: most go to a small hot set (loops), the rest anywhere.
            block = if r.below(10) < 8 {
                r.below(64) as usize
            } else {
                r.below(blocks.len() as u64) as usize
            };
            slot = 0;
        }
        let pc = blocks[block] + slot * 4;
        slot += 1;
        t.pc.push(pc);
        t.time.push(1000 + i * 10);
        t.flags.push(if r.below(50) == 0 { 1 } else { 0 });
        t.opcode.push(r.next() as u32);
        let writes = match r.below(20) {
            0..=4 => 0,
            5..=15 => 1,
            _ => 2,
        };
        for _ in 0..writes {
            let reg = if r.below(4) == 0 {
                16
            } else {
                r.below(13) as usize
            };
            let val = if r.below(8) == 0 { 0 } else { r.next() as u32 };
            regs[reg] = val;
            t.rw_idx.push(i);
            t.rw_reg.push(reg as u8);
            t.rw_val.push(val);
        }
        if r.below(10) < 3 {
            let write = r.below(3) == 0;
            let addr = match r.below(10) {
                0..=5 => 0x2000_8000 - (r.below(4096) as u32 & !3),
                6..=8 => 0x2000_0000 + (r.below(16384) as u32 & !3),
                _ => 0x4000_0000 + (r.below(4096) as u32 & !3),
            };
            let size = [4u8, 4, 4, 2, 1, 8][r.below(6) as usize];
            t.m_idx.push(i);
            t.m_addr.push(addr);
            t.m_size.push(size);
            t.m_write.push(write as u8);
            t.m_val.push(r.next());
        }
    }
    // pc -> indexes
    let mut order: Vec<u32> = (0..n as u32).collect();
    order.sort_by_key(|&i| (t.pc[i as usize], i));
    t.pi_pc = order.iter().map(|&i| t.pc[i as usize]).collect();
    t.pi_idx = order.iter().map(|&i| i as u64).collect();
    // 4-byte granule -> write rows
    let mut w: Vec<(u32, u64, u64)> = Vec::new();
    for row in 0..t.m_idx.len() {
        if t.m_write[row] == 1 {
            let a = t.m_addr[row];
            let end = a + t.m_size[row] as u32;
            let mut g = a & !3;
            while g < end {
                w.push((g, t.m_idx[row], row as u64));
                g += 4;
            }
        }
    }
    w.sort_unstable();
    t.wi_granule = w.iter().map(|x| x.0).collect();
    t.wi_idx = w.iter().map(|x| x.1).collect();
    t.wi_row = w.iter().map(|x| x.2).collect();
    t
}

// ---------------------------------------------------------------- tables as arrow

fn u8a(v: &[u8]) -> ArrayRef {
    Arc::new(UInt8Array::from(v.to_vec()))
}
fn u32a(v: &[u32]) -> ArrayRef {
    Arc::new(UInt32Array::from(v.to_vec()))
}
fn u64a(v: &[u64]) -> ArrayRef {
    Arc::new(UInt64Array::from(v.to_vec()))
}

type Table = (&'static str, Vec<(String, ArrayRef)>);

fn tables(t: &Trace) -> Vec<Table> {
    let mut cp: Vec<(String, ArrayRef)> = vec![("rw_start".into(), u64a(&t.cp_rw_start))];
    for (k, col) in t.cp_regs.iter().enumerate() {
        cp.push((format!("r{k}"), u32a(col)));
    }
    vec![
        (
            "instructions",
            vec![
                ("pc".into(), u32a(&t.pc)),
                ("time".into(), u64a(&t.time)),
                ("flags".into(), u8a(&t.flags)),
                ("opcode".into(), u32a(&t.opcode)),
            ],
        ),
        (
            "reg_writes",
            vec![
                ("idx".into(), u64a(&t.rw_idx)),
                ("reg".into(), u8a(&t.rw_reg)),
                ("value".into(), u32a(&t.rw_val)),
            ],
        ),
        (
            "mem_accesses",
            vec![
                ("idx".into(), u64a(&t.m_idx)),
                ("addr".into(), u32a(&t.m_addr)),
                ("size".into(), u8a(&t.m_size)),
                ("write".into(), u8a(&t.m_write)),
                ("value".into(), u64a(&t.m_val)),
            ],
        ),
        ("checkpoints", cp),
        (
            "pc_index",
            vec![
                ("pc".into(), u32a(&t.pi_pc)),
                ("idx".into(), u64a(&t.pi_idx)),
            ],
        ),
        (
            "write_index",
            vec![
                ("granule".into(), u32a(&t.wi_granule)),
                ("idx".into(), u64a(&t.wi_idx)),
                ("row".into(), u64a(&t.wi_row)),
            ],
        ),
    ]
}

fn batches(cols: &[(String, ArrayRef)]) -> (Arc<Schema>, Vec<RecordBatch>) {
    let schema = Arc::new(Schema::new(
        cols.iter()
            .map(|(n, a)| Field::new(n, a.data_type().clone(), false))
            .collect::<Vec<_>>(),
    ));
    let len = cols[0].1.len();
    let mut out = Vec::new();
    let mut start = 0;
    while start < len {
        let n = BATCH.min(len - start);
        let arrays = cols.iter().map(|(_, a)| a.slice(start, n)).collect();
        out.push(RecordBatch::try_new(schema.clone(), arrays).unwrap());
        start += n;
    }
    (schema, out)
}

// ---------------------------------------------------------------- chunked column access

/// One column over fixed-size batches: O(1) row access, sorted-range search.
struct Col<T: ArrowPrimitiveType> {
    chunks: Vec<PrimitiveArray<T>>,
}
impl<T: ArrowPrimitiveType> Col<T>
where
    T::Native: Ord,
{
    fn from(batches: &[RecordBatch], name: &str) -> Self {
        Col {
            chunks: batches
                .iter()
                .map(|b| {
                    b.column_by_name(name)
                        .unwrap()
                        .as_any()
                        .downcast_ref::<PrimitiveArray<T>>()
                        .unwrap()
                        .clone()
                })
                .collect(),
        }
    }
    fn len(&self) -> usize {
        self.chunks.iter().map(|c| c.len()).sum()
    }
    fn get(&self, row: usize) -> T::Native {
        self.chunks[row / BATCH].values()[row % BATCH]
    }
    /// First row in [lo, hi) for which `pred` is false (pred monotone true→false).
    fn partition(&self, mut lo: usize, mut hi: usize, pred: impl Fn(usize) -> bool) -> usize {
        while lo < hi {
            let mid = (lo + hi) / 2;
            if pred(mid) { lo = mid + 1 } else { hi = mid }
        }
        lo
    }
}

struct Loaded {
    pc: Col<UInt32Type>,
    rw_idx: Col<UInt64Type>,
    rw_reg: Col<UInt8Type>,
    rw_val: Col<UInt32Type>,
    m_addr: Col<UInt32Type>,
    m_write: Col<UInt8Type>,
    m_val: Col<UInt64Type>,
    cp_start: Col<UInt64Type>,
    cp_regs: Vec<Col<UInt32Type>>,
    pi_pc: Col<UInt32Type>,
    pi_idx: Col<UInt64Type>,
    wi_granule: Col<UInt32Type>,
    wi_idx: Col<UInt64Type>,
    wi_row: Col<UInt64Type>,
}

fn load(get: impl Fn(&str) -> Vec<RecordBatch>) -> Loaded {
    let i = get("instructions");
    let rw = get("reg_writes");
    let m = get("mem_accesses");
    let cp = get("checkpoints");
    let pi = get("pc_index");
    let wi = get("write_index");
    Loaded {
        pc: Col::from(&i, "pc"),
        rw_idx: Col::from(&rw, "idx"),
        rw_reg: Col::from(&rw, "reg"),
        rw_val: Col::from(&rw, "value"),
        m_addr: Col::from(&m, "addr"),
        m_write: Col::from(&m, "write"),
        m_val: Col::from(&m, "value"),
        cp_start: Col::from(&cp, "rw_start"),
        cp_regs: (0..NREG)
            .map(|k| Col::from(&cp, &format!("r{k}")))
            .collect(),
        pi_pc: Col::from(&pi, "pc"),
        pi_idx: Col::from(&pi, "idx"),
        wi_granule: Col::from(&wi, "granule"),
        wi_idx: Col::from(&wi, "idx"),
        wi_row: Col::from(&wi, "row"),
    }
}

impl Loaded {
    fn regs_at(&self, i: u64) -> [u32; NREG] {
        let c = (i / CHECKPOINT) as usize;
        let mut regs = [0u32; NREG];
        for (k, r) in regs.iter_mut().enumerate() {
            *r = self.cp_regs[k].get(c);
        }
        let mut row = self.cp_start.get(c) as usize;
        let n = self.rw_idx.len();
        while row < n && self.rw_idx.get(row) < i {
            regs[self.rw_reg.get(row) as usize] = self.rw_val.get(row);
            row += 1;
        }
        regs
    }
    /// Value of the last write covering `granule` before index `i`.
    fn last_write(&self, granule: u32, i: u64) -> Option<u64> {
        let n = self.wi_granule.len();
        let end = self.wi_granule.partition(0, n, |r| {
            let g = self.wi_granule.get(r);
            g < granule || (g == granule && self.wi_idx.get(r) < i)
        });
        if end > 0 && self.wi_granule.get(end - 1) == granule {
            Some(self.m_val.get(self.wi_row.get(end - 1) as usize))
        } else {
            None
        }
    }
    fn executions(&self, pc: u32) -> (usize, Option<u64>) {
        let n = self.pi_pc.len();
        let lo = self.pi_pc.partition(0, n, |r| self.pi_pc.get(r) < pc);
        let hi = self.pi_pc.partition(lo, n, |r| self.pi_pc.get(r) <= pc);
        (
            hi - lo,
            if hi > lo {
                Some(self.pi_idx.get(lo))
            } else {
                None
            },
        )
    }
    fn scan_reg_value(&self, reg: u8, value: u32) -> (usize, Option<u64>) {
        let mut count = 0;
        let mut first = None;
        for (c, (regs, vals)) in self
            .rw_reg
            .chunks
            .iter()
            .zip(&self.rw_val.chunks)
            .enumerate()
        {
            let (regs, vals) = (regs.values(), vals.values());
            for j in 0..regs.len() {
                if regs[j] == reg && vals[j] == value {
                    count += 1;
                    if first.is_none() {
                        first = Some(self.rw_idx.get(c * BATCH + j));
                    }
                }
            }
        }
        (count, first)
    }
    fn scan_writes_in(&self, lo: u32, hi: u32) -> usize {
        let mut count = 0;
        for (a, w) in self.m_addr.chunks.iter().zip(&self.m_write.chunks) {
            count += a
                .values()
                .iter()
                .zip(w.values())
                .filter(|(a, w)| **w == 1 && **a >= lo && **a < hi)
                .count();
        }
        count
    }
    fn scan_pc_column(&self, pc: u32) -> usize {
        self.pc
            .chunks
            .iter()
            .map(|c| c.values().iter().filter(|&&p| p == pc).count())
            .sum()
    }
}

// ---------------------------------------------------------------- arrow ipc

fn write_ipc(dir: &Path, ts: &[Table]) {
    for (name, cols) in ts {
        let (schema, bs) = batches(cols);
        let mut w = FileWriter::try_new(
            File::create(dir.join(format!("{name}.arrow"))).unwrap(),
            &schema,
        )
        .unwrap();
        for b in &bs {
            w.write(b).unwrap();
        }
        w.finish().unwrap();
    }
}

fn read_ipc_mmap(path: &Path) -> Vec<RecordBatch> {
    let file = File::open(path).unwrap();
    let mmap = unsafe { memmap2::Mmap::map(&file).unwrap() };
    let len = mmap.len();
    let ptr = NonNull::new(mmap.as_ptr() as *mut u8).unwrap();
    let buffer = unsafe { Buffer::from_custom_allocation(ptr, len, Arc::new(mmap)) };
    let trailer = len - 10;
    let footer_len = read_footer_length(buffer[trailer..].try_into().unwrap()).unwrap();
    let footer = root_as_footer(&buffer[trailer - footer_len..trailer]).unwrap();
    let schema = Arc::new(try_fb_to_schema(footer.schema().unwrap()).unwrap());
    let decoder = FileDecoder::new(schema, footer.version());
    footer
        .recordBatches()
        .unwrap()
        .iter()
        .map(|block| {
            let n = block.bodyLength() as usize + block.metaDataLength() as usize;
            let data = buffer.slice_with_length(block.offset() as usize, n);
            decoder.read_record_batch(block, &data).unwrap().unwrap()
        })
        .collect()
}

// ---------------------------------------------------------------- parquet

fn write_parquet(dir: &Path, ts: &[Table]) {
    use parquet::arrow::ArrowWriter;
    use parquet::basic::{Compression, ZstdLevel};
    use parquet::file::properties::WriterProperties;
    let props = WriterProperties::builder()
        .set_compression(Compression::ZSTD(ZstdLevel::try_new(3).unwrap()))
        .set_max_row_group_row_count(Some(BATCH))
        .build();
    for (name, cols) in ts {
        let (schema, bs) = batches(cols);
        let file = File::create(dir.join(format!("{name}.parquet"))).unwrap();
        let mut w = ArrowWriter::try_new(file, schema, Some(props.clone())).unwrap();
        for b in &bs {
            w.write(b).unwrap();
        }
        w.close().unwrap();
    }
}

fn read_parquet(path: &Path) -> Vec<RecordBatch> {
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
    let r = ParquetRecordBatchReaderBuilder::try_new(File::open(path).unwrap())
        .unwrap()
        .with_batch_size(BATCH)
        .build()
        .unwrap();
    r.map(|b| b.unwrap()).collect()
}

// ---------------------------------------------------------------- sqlite

fn write_sqlite(path: &Path, t: &Trace) {
    let _ = fs::remove_file(path);
    let mut db = rusqlite::Connection::open(path).unwrap();
    db.execute_batch(
        "PRAGMA journal_mode=OFF; PRAGMA synchronous=OFF; PRAGMA page_size=4096;
         CREATE TABLE instructions(idx INTEGER PRIMARY KEY, pc INTEGER, time INTEGER, flags INTEGER, opcode INTEGER);
         CREATE TABLE reg_writes(idx INTEGER, reg INTEGER, value INTEGER);
         CREATE TABLE mem_accesses(row INTEGER PRIMARY KEY, idx INTEGER, addr INTEGER, size INTEGER, write INTEGER, value INTEGER);
         CREATE TABLE checkpoints(cp INTEGER PRIMARY KEY, regs BLOB);
         CREATE TABLE write_index(granule INTEGER, idx INTEGER, row INTEGER);",
    )
    .unwrap();
    let tx = db.transaction().unwrap();
    {
        let mut s = tx
            .prepare("INSERT INTO instructions VALUES(?,?,?,?,?)")
            .unwrap();
        for i in 0..t.pc.len() {
            s.execute((i as i64, t.pc[i], t.time[i] as i64, t.flags[i], t.opcode[i]))
                .unwrap();
        }
        let mut s = tx.prepare("INSERT INTO reg_writes VALUES(?,?,?)").unwrap();
        for i in 0..t.rw_idx.len() {
            s.execute((t.rw_idx[i] as i64, t.rw_reg[i], t.rw_val[i]))
                .unwrap();
        }
        let mut s = tx
            .prepare("INSERT INTO mem_accesses VALUES(?,?,?,?,?,?)")
            .unwrap();
        for i in 0..t.m_idx.len() {
            s.execute((
                i as i64,
                t.m_idx[i] as i64,
                t.m_addr[i],
                t.m_size[i],
                t.m_write[i],
                t.m_val[i] as i64,
            ))
            .unwrap();
        }
        let mut s = tx.prepare("INSERT INTO checkpoints VALUES(?,?)").unwrap();
        for c in 0..t.cp_rw_start.len() {
            let blob: Vec<u8> = (0..NREG)
                .flat_map(|k| t.cp_regs[k][c].to_le_bytes())
                .collect();
            s.execute((c as i64, blob)).unwrap();
        }
        let mut s = tx.prepare("INSERT INTO write_index VALUES(?,?,?)").unwrap();
        for i in 0..t.wi_granule.len() {
            s.execute((t.wi_granule[i], t.wi_idx[i] as i64, t.wi_row[i] as i64))
                .unwrap();
        }
    }
    tx.commit().unwrap();
    db.execute_batch(
        "CREATE INDEX i_pc ON instructions(pc, idx);
         CREATE INDEX i_rw ON reg_writes(idx);
         CREATE INDEX i_wi ON write_index(granule, idx);",
    )
    .unwrap();
}

struct Sql(rusqlite::Connection);
impl Sql {
    fn regs_at(&self, i: u64) -> [u32; NREG] {
        let c = i / CHECKPOINT;
        let blob: Vec<u8> = self
            .0
            .prepare_cached("SELECT regs FROM checkpoints WHERE cp=?")
            .unwrap()
            .query_row([c as i64], |r| r.get(0))
            .unwrap();
        let mut regs = [0u32; NREG];
        for k in 0..NREG {
            regs[k] = u32::from_le_bytes(blob[k * 4..k * 4 + 4].try_into().unwrap());
        }
        let mut s = self
            .0
            .prepare_cached(
                "SELECT reg, value FROM reg_writes WHERE idx>=? AND idx<? ORDER BY rowid",
            )
            .unwrap();
        let mut rows = s.query(((c * CHECKPOINT) as i64, i as i64)).unwrap();
        while let Some(r) = rows.next().unwrap() {
            let reg: u8 = r.get(0).unwrap();
            regs[reg as usize] = r.get(1).unwrap();
        }
        regs
    }
    fn last_write(&self, granule: u32, i: u64) -> Option<u64> {
        self.0.prepare_cached("SELECT m.value FROM mem_accesses m WHERE m.row = (SELECT row FROM write_index WHERE granule=? AND idx<? ORDER BY idx DESC, row DESC LIMIT 1)")
            .unwrap().query_row((granule, i as i64), |r| r.get::<_, i64>(0)).ok().map(|v| v as u64)
    }
    fn executions(&self, pc: u32) -> (usize, Option<u64>) {
        let n: i64 = self
            .0
            .prepare_cached("SELECT count(*) FROM instructions WHERE pc=?")
            .unwrap()
            .query_row([pc], |r| r.get(0))
            .unwrap();
        let first: Option<i64> = self
            .0
            .prepare_cached("SELECT min(idx) FROM instructions WHERE pc=?")
            .unwrap()
            .query_row([pc], |r| r.get(0))
            .unwrap();
        (n as usize, first.map(|v| v as u64))
    }
    fn scan_reg_value(&self, reg: u8, value: u32) -> (usize, Option<u64>) {
        let n: i64 = self
            .0
            .prepare_cached("SELECT count(*) FROM reg_writes WHERE reg=? AND value=?")
            .unwrap()
            .query_row((reg, value), |r| r.get(0))
            .unwrap();
        let first: Option<i64> = self
            .0
            .prepare_cached("SELECT min(idx) FROM reg_writes WHERE reg=? AND value=?")
            .unwrap()
            .query_row((reg, value), |r| r.get(0))
            .unwrap();
        (n as usize, first.map(|v| v as u64))
    }
    fn scan_writes_in(&self, lo: u32, hi: u32) -> usize {
        let n: i64 = self
            .0
            .prepare_cached(
                "SELECT count(*) FROM mem_accesses WHERE write=1 AND addr>=? AND addr<?",
            )
            .unwrap()
            .query_row((lo, hi), |r| r.get(0))
            .unwrap();
        n as usize
    }
    fn scan_pc_column(&self, pc: u32) -> usize {
        // NOT INDEXED forces the full scan an unindexed column would need.
        let n: i64 = self
            .0
            .prepare_cached("SELECT count(*) FROM instructions NOT INDEXED WHERE pc=?")
            .unwrap()
            .query_row([pc], |r| r.get(0))
            .unwrap();
        n as usize
    }
}

// ---------------------------------------------------------------- measurement

fn dir_size(paths: &[PathBuf]) -> u64 {
    paths
        .iter()
        .map(|p| fs::metadata(p).map(|m| m.len()).unwrap_or(0))
        .sum()
}

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}

struct Queries {
    points: Vec<u64>,
    granules: Vec<(u32, u64)>,
    pcs: Vec<u32>,
}

trait Engine {
    fn regs_at(&self, i: u64) -> [u32; NREG];
    fn last_write(&self, g: u32, i: u64) -> Option<u64>;
    fn executions(&self, pc: u32) -> (usize, Option<u64>);
    fn scan_reg_value(&self, reg: u8, v: u32) -> (usize, Option<u64>);
    fn scan_writes_in(&self, lo: u32, hi: u32) -> usize;
    fn scan_pc_column(&self, pc: u32) -> usize;
}
macro_rules! engine {
    ($t:ty) => {
        impl Engine for $t {
            fn regs_at(&self, i: u64) -> [u32; NREG] {
                <$t>::regs_at(self, i)
            }
            fn last_write(&self, g: u32, i: u64) -> Option<u64> {
                <$t>::last_write(self, g, i)
            }
            fn executions(&self, pc: u32) -> (usize, Option<u64>) {
                <$t>::executions(self, pc)
            }
            fn scan_reg_value(&self, reg: u8, v: u32) -> (usize, Option<u64>) {
                <$t>::scan_reg_value(self, reg, v)
            }
            fn scan_writes_in(&self, lo: u32, hi: u32) -> usize {
                <$t>::scan_writes_in(self, lo, hi)
            }
            fn scan_pc_column(&self, pc: u32) -> usize {
                <$t>::scan_pc_column(self, pc)
            }
        }
    };
}
engine!(Loaded);
engine!(Sql);

/// Returns (per-query median ms for each query kind) and a checksum to compare engines.
fn run(e: &dyn Engine, q: &Queries) -> (Vec<f64>, u64) {
    let mut sum = 0u64;
    let median = |mut v: Vec<f64>| {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        v[v.len() / 2]
    };
    let mut res = Vec::new();
    let mut v = Vec::new();
    for &i in &q.points {
        let t = Instant::now();
        let r = e.regs_at(i);
        v.push(ms(t));
        sum = sum.wrapping_add(r.iter().map(|&x| x as u64).sum::<u64>());
    }
    let first_point = v[0];
    res.push(median(v));
    let mut v = Vec::new();
    for &(g, i) in &q.granules {
        let t = Instant::now();
        let r = e.last_write(g, i);
        v.push(ms(t));
        sum = sum.wrapping_add(r.unwrap_or(7));
    }
    res.push(median(v));
    let mut v = Vec::new();
    for &pc in &q.pcs {
        let t = Instant::now();
        let (n, f) = e.executions(pc);
        v.push(ms(t));
        sum = sum.wrapping_add(n as u64 + f.unwrap_or(3));
    }
    res.push(median(v));
    let mut v = Vec::new();
    for k in 0..3u32 {
        let t = Instant::now();
        let (n, f) = e.scan_reg_value(0, k);
        v.push(ms(t));
        sum = sum.wrapping_add(n as u64 + f.unwrap_or(5));
    }
    res.push(median(v));
    let mut v = Vec::new();
    for k in 0..3u32 {
        let t = Instant::now();
        let n = e.scan_writes_in(0x2000_0000 + k * 0x1000, 0x2000_0400 + k * 0x1000);
        v.push(ms(t));
        sum = sum.wrapping_add(n as u64);
    }
    res.push(median(v));
    let mut v = Vec::new();
    for &pc in q.pcs.iter().take(3) {
        let t = Instant::now();
        let n = e.scan_pc_column(pc);
        v.push(ms(t));
        sum = sum.wrapping_add(n as u64);
    }
    let first_scan = v[0];
    res.push(median(v));
    res.push(first_point);
    res.push(first_scan);
    (res, sum)
}

fn drop_caches() {
    if std::env::var_os("COLD").is_some() {
        let _ = std::process::Command::new("sync").status();
        fs::write("/proc/sys/vm/drop_caches", "3")
            .expect("COLD needs permission to drop the page cache");
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let n: u64 = args
        .get(1)
        .map(|s| s.parse().unwrap())
        .unwrap_or(10_000_000);
    let out = PathBuf::from(args.get(2).cloned().unwrap_or_else(|| "bench-out".into()));
    fs::create_dir_all(&out).unwrap();

    let t = Instant::now();
    let trace = generate(n);
    println!(
        "generated {n} instructions, {} reg writes, {} mem accesses, {} write-index rows in {:.0} ms",
        trace.rw_idx.len(),
        trace.m_idx.len(),
        trace.wi_granule.len(),
        ms(t)
    );

    let mut r = Rng(42);
    let q = Queries {
        points: (0..2000).map(|_| r.below(n)).collect(),
        granules: (0..2000)
            .map(|_| {
                let row = r.below(trace.m_idx.len() as u64) as usize;
                (trace.m_addr[row] & !3, r.below(n))
            })
            .collect(),
        pcs: (0..200).map(|_| trace.pc[r.below(n) as usize]).collect(),
    };
    let ts = tables(&trace);
    let names: Vec<&str> = ts.iter().map(|t| t.0).collect();

    let mut rows = Vec::new();

    // Arrow IPC
    let d = out.join("arrow");
    fs::create_dir_all(&d).unwrap();
    let t = Instant::now();
    write_ipc(&d, &ts);
    let build = ms(t);
    let files: Vec<PathBuf> = names.iter().map(|n| d.join(format!("{n}.arrow"))).collect();
    drop_caches();
    let t = Instant::now();
    let a = load(|name| read_ipc_mmap(&d.join(format!("{name}.arrow"))));
    let open = ms(t);
    let (qa, ca) = run(&a, &q);
    rows.push(("Arrow IPC (mmap)", build, dir_size(&files), open, qa, ca));
    drop(a);

    // Parquet
    let d = out.join("parquet");
    fs::create_dir_all(&d).unwrap();
    let t = Instant::now();
    write_parquet(&d, &ts);
    let build = ms(t);
    let files: Vec<PathBuf> = names
        .iter()
        .map(|n| d.join(format!("{n}.parquet")))
        .collect();
    drop_caches();
    let t = Instant::now();
    let p = load(|name| read_parquet(&d.join(format!("{name}.parquet"))));
    let open = ms(t);
    let (qp, cp) = run(&p, &q);
    rows.push((
        "Parquet zstd (load to memory)",
        build,
        dir_size(&files),
        open,
        qp,
        cp,
    ));
    drop(p);

    // SQLite
    let path = out.join("index.sqlite");
    let t = Instant::now();
    write_sqlite(&path, &trace);
    let build = ms(t);
    drop_caches();
    let t = Instant::now();
    let s = Sql(rusqlite::Connection::open_with_flags(
        &path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap());
    let open = ms(t);
    let (qs, cs) = run(&s, &q);
    rows.push((
        "SQLite (B-tree indexes)",
        build,
        dir_size(&[path]),
        open,
        qs,
        cs,
    ));

    println!();
    println!(
        "| format | build (s) | size (MB) | open (ms) | registers @ i | memory @ i | executions(pc) | scan r0==v | scan writes in range | scan pc column | first registers @ i | first scan pc column | checksum |"
    );
    println!("|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|");
    for (name, build, size, open, q, c) in &rows {
        println!(
            "| {name} | {:.1} | {:.0} | {:.1} | {:.3} | {:.3} | {:.3} | {:.0} | {:.0} | {:.0} | {:.3} | {:.0} | {c:x} |",
            build / 1000.0,
            *size as f64 / 1e6,
            open,
            q[0],
            q[1],
            q[2],
            q[3],
            q[4],
            q[5],
            q[6],
            q[7]
        );
    }
    println!();
    println!(
        "Query columns are median milliseconds per query (point queries over 2000 random points, \
              executions over 200 PCs, scans over 3 runs). The two `first` columns are the first query \
              after opening (with COLD=1 the page cache is dropped before each format is opened)."
    );
}
