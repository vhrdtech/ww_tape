//! A small seeded generator, so every bench makes the same data on every run.

/// xorshift64*: the same seed gives the same numbers.
#[derive(Clone, Debug)]
pub struct Rng(pub u64);

impl Rng {
    /// The next 64 bits.
    pub fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    /// A number in `0..n`.
    ///
    /// # Panics
    ///
    /// When `n` is 0.
    pub fn below(&mut self, n: u64) -> u64 {
        assert!(n > 0, "a range below 0 is empty");
        self.next_u64() % n
    }

    /// A number in `lo..hi`.
    ///
    /// # Panics
    ///
    /// When `hi <= lo`.
    pub fn range(&mut self, lo: i64, hi: i64) -> i64 {
        assert!(hi > lo, "the range {lo}..{hi} is empty");
        let span = hi.abs_diff(lo);
        lo.wrapping_add_unsigned(self.below(span))
    }

    /// A number in `[0, 1)`.
    pub fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
}
