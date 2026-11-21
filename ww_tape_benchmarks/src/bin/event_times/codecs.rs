//! The codecs ww_tape's linear groups are compared with, each over one chunk of non-decreasing ticks: raw 64-bit
//! ticks, zigzag varints of the delta of deltas with a skip entry every 128 items, and Elias-Fano with select samples.
//! They are bench-only; the format has none of them.

/// Bytes of padding after packed data, so a read of 16 bytes never runs past the buffer.
const PAD: usize = 16;

/// One chunk of ticks in a codec that reads in place.
pub trait Column: Sized {
    /// The name the table gives it.
    const NAME: &'static str;
    /// `v` is non-decreasing and not empty.
    fn encode(v: &[i64]) -> Self;
    /// Bytes as stored, its own header and skip index included.
    fn bytes(&self) -> usize;
    /// Item `i`.
    fn get(&self, i: usize) -> i64;
    /// The first item at or after `t`; the item count when none is.
    fn lower_bound(&self, t: i64) -> usize;
}

/// Raw 64-bit ticks, found by binary search.
pub struct Raw(Vec<i64>);

impl Column for Raw {
    const NAME: &'static str = "raw 64-bit ticks";

    fn encode(v: &[i64]) -> Self {
        Self(v.to_vec())
    }

    fn bytes(&self) -> usize {
        self.0.len() * 8
    }

    fn get(&self, i: usize) -> i64 {
        self.0[i]
    }

    fn lower_bound(&self, t: i64) -> usize {
        self.0.partition_point(|&x| x < t)
    }
}

fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

fn unzigzag(v: u64) -> i64 {
    ((v >> 1) as i64) ^ -((v & 1) as i64)
}

fn put_uvar(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

#[inline(always)]
fn get_uvar(buf: &[u8], pos: &mut usize) -> u64 {
    let mut v = 0u64;
    let mut sh = 0;
    loop {
        let b = buf[*pos];
        *pos += 1;
        v |= u64::from(b & 0x7f) << sh;
        if b < 0x80 {
            return v;
        }
        sh += 7;
    }
}

/// Zigzag varints of the delta of deltas, with a skip entry every 128 items: the tick, the delta before it and the
/// byte offset (20 bytes). A read walks from the entry to its item.
pub struct DodVarint {
    n: usize,
    data: Vec<u8>,
    skip_x: Vec<i64>,
    skip_d: Vec<i64>,
    skip_off: Vec<u32>,
}

impl DodVarint {
    /// Walk group `b` from its skip entry, calling `f(item, tick)` until it returns false.
    #[inline(always)]
    fn walk(&self, b: usize, f: &mut impl FnMut(usize, i64) -> bool) {
        let mut pos = self.skip_off[b] as usize;
        let mut x = self.skip_x[b];
        let mut d = self.skip_d[b];
        let end = ((b + 1) * 128).min(self.n);
        let mut i = b * 128;
        if !f(i, x) {
            return;
        }
        i += 1;
        while i < end {
            d += unzigzag(get_uvar(&self.data, &mut pos));
            x += d;
            if !f(i, x) {
                return;
            }
            i += 1;
        }
    }
}

impl Column for DodVarint {
    const NAME: &'static str = "delta-of-delta varints";

    fn encode(v: &[i64]) -> Self {
        let mut s = Self {
            n: v.len(),
            data: vec![],
            skip_x: vec![],
            skip_d: vec![],
            skip_off: vec![],
        };
        let mut prev_d = 0i64;
        for (i, &x) in v.iter().enumerate() {
            if i.is_multiple_of(128) {
                if i > 0 {
                    prev_d = x - v[i - 1];
                }
                s.skip_x.push(x);
                s.skip_d.push(prev_d);
                s.skip_off.push(s.data.len() as u32);
                continue;
            }
            let d = x - v[i - 1];
            put_uvar(&mut s.data, zigzag(d - prev_d));
            prev_d = d;
        }
        s.data.extend_from_slice(&[0; PAD]);
        s
    }

    fn bytes(&self) -> usize {
        self.data.len() - PAD + self.skip_x.len() * 20
    }

    fn get(&self, i: usize) -> i64 {
        let mut out = 0;
        self.walk(i / 128, &mut |k, x| {
            out = x;
            k < i
        });
        out
    }

    fn lower_bound(&self, t: i64) -> usize {
        let nb = self.skip_x.partition_point(|&x| x < t);
        if nb == 0 {
            return 0;
        }
        let b = nb - 1;
        let mut res = ((b + 1) * 128).min(self.n);
        self.walk(b, &mut |k, x| {
            if x >= t {
                res = k;
                false
            } else {
                true
            }
        });
        res
    }
}

/// Every this many ones and zeros of the high bits, a select sample.
const EF_SAMPLE: usize = 256;

fn mask(width: u32) -> u64 {
    if width >= 64 {
        u64::MAX
    } else {
        (1u64 << width) - 1
    }
}

/// Elias-Fano over `v - v[0]`: `l` low bits per item packed, the high bits in unary, and a select sample every 256
/// ones and every 256 zeros.
pub struct EliasFano {
    n: usize,
    base: i64,
    max: u64,
    l: u32,
    low: Vec<u8>,
    high: Vec<u64>,
    sel1: Vec<u32>,
    sel0: Vec<u32>,
}

impl EliasFano {
    /// Position of the `k`-th set (or clear) bit at or after bit `pos`.
    #[inline(always)]
    fn select_from(&self, pos: usize, mut k: usize, ones: bool) -> usize {
        let mut w = pos / 64;
        let word = |w: usize| if ones { self.high[w] } else { !self.high[w] };
        let mut cur = word(w) & (u64::MAX << (pos % 64));
        loop {
            let c = cur.count_ones() as usize;
            if k < c {
                for _ in 0..k {
                    cur &= cur - 1;
                }
                return w * 64 + cur.trailing_zeros() as usize;
            }
            k -= c;
            w += 1;
            cur = word(w);
        }
    }

    fn select1(&self, i: usize) -> usize {
        let s = i / EF_SAMPLE;
        self.select_from(self.sel1[s] as usize, i - s * EF_SAMPLE, true)
    }

    fn select0(&self, i: usize) -> usize {
        let s = i / EF_SAMPLE;
        self.select_from(self.sel0[s] as usize, i - s * EF_SAMPLE, false)
    }

    #[inline(always)]
    fn low(&self, i: usize) -> u64 {
        if self.l == 0 {
            return 0;
        }
        let bit = i * self.l as usize;
        let byte = bit >> 3;
        let w = u128::from_le_bytes(
            self.low[byte..byte + 16]
                .try_into()
                .expect("PAD bytes follow"),
        );
        ((w >> (bit & 7)) as u64) & mask(self.l)
    }
}

impl Column for EliasFano {
    const NAME: &'static str = "Elias-Fano";

    fn encode(v: &[i64]) -> Self {
        let n = v.len();
        let base = v[0];
        let max = (v[n - 1] - base) as u64;
        let per = max / n as u64;
        let l = if per > 0 { 63 - per.leading_zeros() } else { 0 };
        let nbits = n + (max >> l) as usize + 1;
        let mut high = vec![0u64; nbits.div_ceil(64) + 2];
        let mut low = Vec::new();
        let (mut acc, mut fill) = (0u128, 0u32);
        for &x in v {
            if l == 0 {
                break;
            }
            acc |= u128::from(((x - base) as u64) & mask(l)) << fill;
            fill += l;
            while fill >= 8 {
                low.push(acc as u8);
                acc >>= 8;
                fill -= 8;
            }
        }
        if fill > 0 {
            low.push(acc as u8);
        }
        low.extend_from_slice(&[0; PAD]);
        let mut sel1 = Vec::new();
        for (i, &x) in v.iter().enumerate() {
            let p = (((x - base) as u64) >> l) as usize + i;
            high[p / 64] |= 1 << (p % 64);
            if i.is_multiple_of(EF_SAMPLE) {
                sel1.push(p as u32);
            }
        }
        let mut sel0 = Vec::new();
        let mut zeros = 0usize;
        for p in 0..nbits {
            if high[p / 64] >> (p % 64) & 1 == 0 {
                if zeros.is_multiple_of(EF_SAMPLE) {
                    sel0.push(p as u32);
                }
                zeros += 1;
            }
        }
        Self {
            n,
            base,
            max,
            l,
            low,
            high,
            sel1,
            sel0,
        }
    }

    fn bytes(&self) -> usize {
        let high = (self.n + (self.max >> self.l) as usize + 1).div_ceil(8);
        // base, max and l, then the low bits, the high bits and the select samples
        8 + 8
            + 1
            + (self.n * self.l as usize).div_ceil(8)
            + high
            + (self.sel1.len() + self.sel0.len()) * 4
    }

    fn get(&self, i: usize) -> i64 {
        let hi = (self.select1(i) - i) as u64;
        self.base + ((hi << self.l) | self.low(i)) as i64
    }

    fn lower_bound(&self, t: i64) -> usize {
        if t <= self.base {
            return 0;
        }
        let x = (t - self.base) as u64;
        if x > self.max {
            return self.n;
        }
        let h = (x >> self.l) as usize;
        let xl = x & mask(self.l);
        // bucket h starts after the h-th zero of the high bits
        let pos = if h == 0 { 0 } else { self.select0(h - 1) + 1 };
        let mut i = pos - h;
        let mut p = pos;
        loop {
            if self.high[p / 64] >> (p % 64) & 1 == 0 || self.low(i) >= xl {
                return i;
            }
            i += 1;
            p += 1;
        }
    }
}

/// One chunk of ticks read in place, by item and by time: a codec's [Column], or ww_tape's own starts column.
pub trait Chunk {
    /// Item `i`.
    fn get(&self, i: usize) -> i64;
    /// The first item at or after `t`; the item count when none is.
    fn lower_bound(&self, t: i64) -> usize;
}

impl<C: Column> Chunk for C {
    fn get(&self, i: usize) -> i64 {
        Column::get(self, i)
    }

    fn lower_bound(&self, t: i64) -> usize {
        Column::lower_bound(self, t)
    }
}

/// A track of chunks of `chunk` items each, with the first tick of each chunk as its index has it.
pub struct Track<C> {
    chunk: usize,
    chunks: Vec<C>,
    firsts: Vec<i64>,
}

impl<C: Chunk> Track<C> {
    /// A track of `chunks` that hold `v` in chunks of `chunk` items.
    pub fn new(v: &[i64], chunk: usize, chunks: Vec<C>) -> Self {
        Self {
            chunk,
            chunks,
            firsts: v.chunks(chunk).map(|c| c[0]).collect(),
        }
    }

    /// Item `i`.
    pub fn get(&self, i: usize) -> i64 {
        self.chunks[i / self.chunk].get(i % self.chunk)
    }

    /// The first item at or after `t`.
    pub fn lower_bound(&self, t: i64) -> usize {
        let idx = self.firsts.partition_point(|&f| f < t);
        if idx == 0 {
            return 0;
        }
        let c = idx - 1;
        c * self.chunk + self.chunks[c].lower_bound(t)
    }
}

impl<C: Column> Track<C> {
    /// `v` cut into chunks of `chunk` items.
    pub fn build(v: &[i64], chunk: usize) -> Self {
        Self::new(v, chunk, v.chunks(chunk).map(C::encode).collect())
    }

    /// Bytes of every chunk's column.
    pub fn bytes(&self) -> usize {
        self.chunks.iter().map(C::bytes).sum()
    }
}
