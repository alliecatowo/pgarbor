//! Deterministic RNG (SplitMix64). CONTRACT. Fully implemented.
//!
//! Own implementation instead of `rand` so seeds reproduce across dependency upgrades and so
//! workers never touch rand's renamed APIs (`gen` is a reserved keyword in edition 2024).

// CONTRACT
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DetRng {
    state: u64,
}

impl DetRng {
    pub fn new(seed: u64) -> DetRng {
        DetRng { state: seed }
    }
    /// Derive an independent stream (e.g. one for workload generation, one for crash images).
    pub fn fork(&mut self, salt: u64) -> DetRng {
        DetRng::new(self.next_u64() ^ salt.wrapping_mul(0x9E37_79B9_7F4A_7C15))
    }
    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    /// Uniform in `0..n` (n > 0; returns 0 when n == 0).
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            return 0;
        }
        // Lemire's multiply-shift; bias is negligible for test purposes.
        ((self.next_u64() as u128 * n as u128) >> 64) as u64
    }
    /// Uniform in `lo..=hi` (returns lo when hi < lo).
    pub fn range_inclusive(&mut self, lo: u64, hi: u64) -> u64 {
        if hi < lo { lo } else { lo + self.below(hi - lo + 1) }
    }
    /// True with probability `permille / 1000`.
    pub fn chance(&mut self, permille: u16) -> bool {
        self.below(1000) < permille as u64
    }
    pub fn bytes(&mut self, len: usize) -> Vec<u8> {
        let mut v = Vec::with_capacity(len);
        while v.len() < len {
            let w = self.next_u64().to_le_bytes();
            let take = (len - v.len()).min(8);
            v.extend_from_slice(&w[..take]);
        }
        v
    }
    /// Pick an index into a non-empty slice length (0 when len == 0).
    pub fn pick(&mut self, len: usize) -> usize {
        self.below(len as u64) as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn deterministic() {
        let mut a = DetRng::new(7);
        let mut b = DetRng::new(7);
        assert_eq!(a.next_u64(), b.next_u64());
        assert!(a.below(10) < 10);
        assert_eq!(a.bytes(13).len(), 13);
        assert_eq!(DetRng::new(0).next_u64(), 0xE220_A839_7B1D_CDAF);
    }
}
