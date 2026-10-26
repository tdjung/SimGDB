//! External sort of fixed-size records for the index tables that must be
//! sorted (memory index by address, PC index by PC): records are buffered up
//! to a memory cap, sorted, and spilled to run files; the runs are merged at
//! the end. Memory stays bounded however long the trace is.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use crate::StoreError;

pub struct ExtSorter<const N: usize> {
    buf: Vec<[u64; N]>,
    cap: usize,
    dir: PathBuf,
    name: &'static str,
    runs: Vec<PathBuf>,
}

impl<const N: usize> ExtSorter<N> {
    /// `cap`: records held in memory before a run is spilled to `dir`.
    pub fn new(dir: &Path, name: &'static str, cap: usize) -> Self {
        ExtSorter { buf: Vec::new(), cap: cap.max(1), dir: dir.to_path_buf(), name, runs: Vec::new() }
    }

    #[inline]
    pub fn push(&mut self, r: [u64; N]) -> Result<(), StoreError> {
        self.buf.push(r);
        if self.buf.len() >= self.cap {
            self.spill()?;
        }
        Ok(())
    }

    fn spill(&mut self) -> Result<(), StoreError> {
        self.buf.sort_unstable();
        let path = self.dir.join(format!("{}.run{}", self.name, self.runs.len()));
        let file = File::create(&path).map_err(|e| StoreError::io(&path, e))?;
        let mut w = BufWriter::with_capacity(1 << 20, file);
        for r in &self.buf {
            for v in r {
                w.write_all(&v.to_le_bytes()).map_err(|e| StoreError::io(&path, e))?;
            }
        }
        w.flush().map_err(|e| StoreError::io(&path, e))?;
        self.buf.clear();
        self.runs.push(path);
        Ok(())
    }

    /// Spilled runs so far (for tests and reports).
    pub fn runs(&self) -> usize {
        self.runs.len()
    }

    /// All records in sorted order, passed to `out`.
    pub fn finish(mut self, mut out: impl FnMut([u64; N]) -> Result<(), StoreError>) -> Result<(), StoreError> {
        if self.runs.is_empty() {
            self.buf.sort_unstable();
            for r in self.buf.drain(..) {
                out(r)?;
            }
            return Ok(());
        }
        if !self.buf.is_empty() {
            self.spill()?;
        }
        let mut readers = Vec::new();
        for p in &self.runs {
            let f = File::open(p).map_err(|e| StoreError::io(p, e))?;
            readers.push((BufReader::with_capacity(1 << 20, f), p.clone()));
        }
        let next = |r: &mut (BufReader<File>, PathBuf)| -> Result<Option<[u64; N]>, StoreError> {
            let mut rec = [0u64; N];
            let mut bytes = [0u8; 8];
            for (k, v) in rec.iter_mut().enumerate() {
                match r.0.read_exact(&mut bytes) {
                    Ok(()) => *v = u64::from_le_bytes(bytes),
                    Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof && k == 0 => return Ok(None),
                    Err(e) => return Err(StoreError::io(&r.1, e)),
                }
            }
            Ok(Some(rec))
        };
        let mut heap = BinaryHeap::new();
        for (k, r) in readers.iter_mut().enumerate() {
            if let Some(rec) = next(r)? {
                heap.push(Reverse((rec, k)));
            }
        }
        while let Some(Reverse((rec, k))) = heap.pop() {
            out(rec)?;
            if let Some(n) = next(&mut readers[k])? {
                heap.push(Reverse((n, k)));
            }
        }
        for p in &self.runs {
            let _ = std::fs::remove_file(p);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spilled_runs_merge_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = ExtSorter::<2>::new(dir.path(), "t", 7);
        let mut want = Vec::new();
        let mut x = 12345u64;
        for i in 0..1000 {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let r = [x % 50, i];
            want.push(r);
            s.push(r).unwrap();
        }
        assert!(s.runs() > 100);
        want.sort();
        let mut got = Vec::new();
        s.finish(|r| {
            got.push(r);
            Ok(())
        })
        .unwrap();
        assert_eq!(got, want);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0, "runs removed");
    }
}
