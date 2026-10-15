//! Per-file durability model. Lease FLT-02 (sonnet): `fsync_fail`, `crash_image`. The rest is
//! implemented.
//!
//! Model (CONTRACT): a file has `durable` bytes (what survives power loss), `current` bytes (what
//! reads see: durable + all pending ops applied in order) and an ordered `pending` list of
//! unsynced mutations. `sync()` makes current durable. Power loss produces a durable image from
//! `durable` plus a CrashMode-selected subset of `pending` (applied in order; tearing at
//! `sector_size`-aligned absolute offsets).
use crate::plan::CrashMode;
use crate::rng::DetRng;
use pgarbor_common::Result;

// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pending {
    Write { offset: u64, data: Vec<u8> },
    SetLen(u64),
}

// CONTRACT
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FileState {
    pub durable: Vec<u8>,
    pub current: Vec<u8>,
    pub pending: Vec<Pending>,
    /// Emulated advisory locks across handles: shared count / exclusive flag.
    pub shared_locks: usize,
    pub exclusive_lock: bool,
}

/// Apply one pending op to a byte vector (zero-filling gaps).
pub fn apply(buf: &mut Vec<u8>, op: &Pending) {
    match op {
        Pending::Write { offset, data } => {
            let start = *offset as usize;
            let end = start + data.len();
            if buf.len() < end {
                buf.resize(end, 0);
            }
            buf[start..end].copy_from_slice(data);
        }
        Pending::SetLen(len) => buf.resize(*len as usize, 0),
    }
}

impl FileState {
    /// A file whose durable and current contents are `bytes`.
    pub fn new(bytes: Vec<u8>) -> FileState {
        FileState { durable: bytes.clone(), current: bytes, ..FileState::default() }
    }
    pub fn len(&self) -> u64 {
        self.current.len() as u64
    }
    pub fn is_empty(&self) -> bool {
        self.current.is_empty()
    }
    /// Read from `current`; returns bytes copied (0 at/after EOF).
    pub fn read(&self, offset: u64, buf: &mut [u8]) -> usize {
        let len = self.current.len() as u64;
        if offset >= len {
            return 0;
        }
        let start = offset as usize;
        let n = buf.len().min(self.current.len() - start);
        buf[..n].copy_from_slice(&self.current[start..start + n]);
        n
    }
    /// Unsynced write.
    pub fn write(&mut self, offset: u64, data: &[u8]) {
        let op = Pending::Write { offset, data: data.to_vec() };
        apply(&mut self.current, &op);
        self.pending.push(op);
    }
    /// Unsynced truncate/extend.
    pub fn set_len(&mut self, len: u64) {
        let op = Pending::SetLen(len);
        apply(&mut self.current, &op);
        self.pending.push(op);
    }
    /// Successful fsync/fdatasync: everything current becomes durable.
    pub fn sync(&mut self) {
        self.durable = self.current.clone();
        self.pending.clear();
    }

    /// Failed fsync with Linux "fsyncgate" semantics: each pending op is independently persisted
    /// into `durable` (keep 1/2, tear 1/4 using `sector_size`, drop 1/4, decided by `rng`), then
    /// `pending` is CLEARED while `current` is left unchanged — the page cache still shows the
    /// data but it will never be retried, so a later successful sync does not make the dropped
    /// parts durable. (This is exactly why the storage layer poisons after an fsync error.)
    pub fn fsync_fail(&mut self, sector_size: usize, rng: &mut DetRng) -> Result<()> {
        let pending = std::mem::take(&mut self.pending);
        for op in &pending {
            match rng.below(4) {
                0 | 1 => apply(&mut self.durable, op),
                2 => tear_op(&mut self.durable, op, sector_size, rng),
                _ => {}
            }
        }
        Ok(())
    }

    /// Durable image after a power loss, without mutating self. `DropUnsynced` → `durable`;
    /// `KeepUnsynced` → `current`; `Random{keep, tear}`: start from `durable`, for each pending op
    /// in order draw `r = rng.below(1000)`: `r < keep` → apply fully; `r < keep + tear` → Write:
    /// apply only a random non-empty proper subset of its sector chunks (chunk boundaries at
    /// absolute offsets multiple of `sector_size`; the first/last chunk may be partial; a write
    /// inside one sector is kept or dropped whole), SetLen: apply; else drop.
    /// Example: durable "AAAA", pending [Write{0,"BB"}], KeepUnsynced → "BBAA".
    pub fn crash_image(&self, mode: CrashMode, sector_size: usize, rng: &mut DetRng) -> Result<Vec<u8>> {
        match mode {
            CrashMode::DropUnsynced => Ok(self.durable.clone()),
            CrashMode::KeepUnsynced => Ok(self.current.clone()),
            CrashMode::Random { keep_permille, tear_permille } => {
                let keep = keep_permille as u64;
                let tear = keep + tear_permille as u64;
                let mut img = self.durable.clone();
                for op in &self.pending {
                    let r = rng.below(1000);
                    if r < keep {
                        apply(&mut img, op);
                    } else if r < tear {
                        tear_op(&mut img, op, sector_size, rng);
                    }
                }
                Ok(img)
            }
        }
    }
}

/// Tear one op into `buf`: a Write applies a random non-empty proper subset of its sector chunks
/// (a single chunk is kept or dropped whole with 50%); SetLen is applied.
fn tear_op(buf: &mut Vec<u8>, op: &Pending, sector_size: usize, rng: &mut DetRng) {
    match op {
        Pending::SetLen(_) => apply(buf, op),
        Pending::Write { offset, data } => {
            if data.is_empty() {
                return;
            }
            let sector = sector_size.max(1) as u64;
            let start = *offset;
            let end = start + data.len() as u64;
            let mut bounds: Vec<u64> = vec![start];
            let mut b = (start / sector + 1) * sector;
            while b < end {
                bounds.push(b);
                b += sector;
            }
            bounds.push(end);
            let n = bounds.len() - 1;
            let mut keep: Vec<bool> = (0..n).map(|_| rng.below(2) == 1).collect();
            if n >= 2 {
                let kept = keep.iter().filter(|k| **k).count();
                if kept == 0 {
                    let i = rng.pick(n);
                    keep[i] = true;
                } else if kept == n {
                    let i = rng.pick(n);
                    keep[i] = false;
                }
            }
            for (i, k) in keep.iter().enumerate() {
                if *k {
                    let lo = (bounds[i] - start) as usize;
                    let hi = (bounds[i + 1] - start) as usize;
                    let part = Pending::Write { offset: bounds[i], data: data[lo..hi].to_vec() };
                    apply(buf, &part);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(rng: &mut DetRng) -> FileState {
        let mut f = FileState::new(rng.bytes(2000));
        for _ in 0..6 {
            let off = rng.below(2500);
            let len = 1 + rng.below(1500) as usize;
            let d = vec![(rng.below(250) + 1) as u8; len];
            f.write(off, &d);
        }
        f
    }

    #[test]
    fn keep_drop_modes() {
        let mut rng = DetRng::new(1);
        let f = sample(&mut rng);
        assert_eq!(f.crash_image(CrashMode::KeepUnsynced, 512, &mut rng).unwrap(), f.current);
        assert_eq!(f.crash_image(CrashMode::DropUnsynced, 512, &mut rng).unwrap(), f.durable);
        let m = CrashMode::Random { keep_permille: 1000, tear_permille: 0 };
        assert_eq!(f.crash_image(m, 512, &mut rng).unwrap(), f.current);
        let m = CrashMode::Random { keep_permille: 0, tear_permille: 0 };
        assert_eq!(f.crash_image(m, 512, &mut rng).unwrap(), f.durable);
    }

    #[test]
    fn example() {
        let mut f = FileState::new(b"AAAA".to_vec());
        f.write(0, b"BB");
        let mut rng = DetRng::new(3);
        assert_eq!(f.crash_image(CrashMode::KeepUnsynced, 512, &mut rng).unwrap(), b"BBAA");
    }

    #[test]
    fn tear_bytes_are_old_or_new() {
        for seed in 0..200 {
            let mut rng = DetRng::new(seed);
            let old = rng.bytes(3000);
            let mut f = FileState::new(old.clone());
            let off = rng.below(1500);
            let new = vec![0xFFu8; 1 + rng.below(1400) as usize];
            f.write(off, &new);
            let m = CrashMode::Random { keep_permille: 0, tear_permille: 1000 };
            let img = f.crash_image(m, 512, &mut rng).unwrap();
            assert_eq!(img.len(), old.len());
            for (i, b) in img.iter().enumerate() {
                let covered = (i as u64) >= off && (i as u64) < off + new.len() as u64;
                assert!(*b == old[i] || (covered && *b == 0xFF));
            }
        }
    }

    #[test]
    fn tear_is_proper_subset_of_chunks() {
        for seed in 0..100 {
            let mut rng = DetRng::new(seed);
            let mut f = FileState::new(vec![0u8; 2048]);
            f.write(0, &vec![1u8; 2048]);
            let m = CrashMode::Random { keep_permille: 0, tear_permille: 1000 };
            let img = f.crash_image(m, 512, &mut rng).unwrap();
            let ones = img.iter().filter(|b| **b == 1).count();
            assert!(ones > 0 && ones < 2048);
            assert_eq!(ones % 512, 0);
        }
    }

    #[test]
    fn fsync_fail_clears_pending_keeps_current() {
        let mut rng = DetRng::new(9);
        let mut f = FileState::new(vec![0u8; 4096]);
        f.write(0, &vec![7u8; 4096]);
        let cur = f.current.clone();
        f.fsync_fail(512, &mut rng).unwrap();
        assert!(f.pending.is_empty());
        assert_eq!(f.current, cur);
        for (d, c) in f.durable.iter().zip(cur.iter()) {
            assert!(*d == 0 || *d == *c);
        }
        f.write(0, &[9u8; 4]);
        assert_eq!(f.pending.len(), 1);
        f.sync();
        assert_eq!(f.durable, f.current);
    }

    #[test]
    fn fsync_fail_dropped_parts_stay_lost_until_rewritten() {
        for seed in 0..64 {
            let mut rng = DetRng::new(seed);
            let mut f = FileState::new(vec![0u8; 8]);
            f.write(0, &[5u8; 4]);
            f.fsync_fail(512, &mut rng).unwrap();
            if f.durable == vec![0u8; 8] {
                let mut crash_rng = DetRng::new(0);
                let img = f.crash_image(CrashMode::DropUnsynced, 512, &mut crash_rng).unwrap();
                assert_eq!(img, vec![0u8; 8]);
                assert_eq!(&f.current[..4], &[5u8; 4]);
                return;
            }
        }
        panic!("no dropping seed found");
    }
}
