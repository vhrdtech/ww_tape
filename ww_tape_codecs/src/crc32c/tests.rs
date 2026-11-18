use super::*;

/// xorshift64*, a fixed seed.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
}

fn checksum_with(kernel: Kernel, bytes: &[u8]) -> Option<u32> {
    let mut d = digest_with(kernel)?;
    d.update(bytes);
    Some(d.finalize())
}

/// `x^(8n)` by square-and-multiply straight from `x^8` (no table, so no assumption about [X2N]'s period):
/// ground truth for [x8nmodp].
fn reference_x8nmodp(mut n: u64) -> u32 {
    let x1 = 1u32 << 30;
    let x2 = multmodp(x1, x1);
    let x4 = multmodp(x2, x2);
    let mut base = multmodp(x4, x4); // x^8
    let mut result = 1u32 << 31; // x^0
    while n != 0 {
        if n & 1 != 0 {
            result = multmodp(base, result);
        }
        base = multmodp(base, base);
        n >>= 1;
    }
    result
}

/// `x8nmodp` against [reference_x8nmodp] well past the 31-wide table's first period (`n` up to and over
/// `2^29`, where the previous period-32 table first disagreed) and at the top of `u64`.
#[test]
fn x8nmodp_matches_square_and_multiply() {
    let mut r = Rng(0xA17E);
    let ns: Vec<u64> = [
        0,
        1,
        2,
        7,
        8,
        (1 << 29) - 1,
        1 << 29,
        (1 << 29) + 1,
        1 << 30,
        1 << 31,
        1 << 40,
        1 << 60,
        u64::MAX,
    ]
    .into_iter()
    .chain((0..50).map(|_| r.next()))
    .collect();
    for n in ns {
        assert_eq!(x8nmodp(n), reference_x8nmodp(n), "n = {n}");
    }
}

/// [crc32c_combine] against the same reference at the lengths that caught the bug and beyond.
#[test]
fn combine_at_large_lengths() {
    let mut r = Rng(0xB16);
    for len_b in [
        1u64 << 29,
        (1 << 29) + 1,
        (1 << 29) * 3,
        1 << 32,
        1 << 40,
        u64::MAX,
    ] {
        let crc_a = u32::try_from(r.next() & 0xffff_ffff).expect("masked to 32 bits");
        let crc_b = u32::try_from(r.next() & 0xffff_ffff).expect("masked to 32 bits");
        let expected = multmodp(reference_x8nmodp(len_b), crc_a) ^ crc_b;
        assert_eq!(
            crc32c_combine(crc_a, crc_b, len_b),
            expected,
            "len_b = {len_b}"
        );
    }
}

/// Every kernel this CPU runs agrees with [TABLE], on lengths around every boundary the hardware paths split on
/// (a word, a lane, [hw_x86_64::MIN_LANE_WORDS]'s multiple-of-3, misaligned tails), and chained `update` calls
/// split at several points (and three-way, as a block's header-then-payload updates do) give the same result as
/// one. [Kernel::Table] always runs; a hardware kernel this CPU lacks is skipped, not failed, so the test passes
/// the same on a CPU without SSE4.2 or the CRC extension: the ARM path wants an aarch64 CPU with the CRC
/// extension to check here (`is_aarch64_feature_detected!("crc")`), the x86 path an SSE4.2 one.
#[test]
fn kernels_agree() {
    let mut r = Rng(0xC0FFEE);
    let lens: Vec<usize> = (0..200)
        .map(|_| usize::try_from(r.next() % 4096).expect("under 4096"))
        .chain([0, 1, 7, 8, 9, 64 * 8 * 3 - 1, 64 * 8 * 3, 64 * 8 * 3 + 1])
        .collect();
    let kernels = [
        Kernel::Table,
        #[cfg(target_arch = "x86_64")]
        Kernel::Sse42,
        #[cfg(target_arch = "aarch64")]
        Kernel::Crc32,
    ];
    for len in lens {
        let data: Vec<u8> = (0..len)
            .map(|_| u8::try_from(r.next() & 0xff).expect("masked to a byte"))
            .collect();
        let expected = TABLE.checksum(&data);
        for kernel in kernels {
            let Some(got) = checksum_with(kernel, &data) else {
                continue;
            };
            assert_eq!(got, expected, "{kernel:?}, len {len}");

            let splits = [
                1,
                data.len() / 3,
                data.len() / 2,
                data.len().wrapping_sub(1),
            ];
            for at in splits {
                if at == 0 || at >= data.len() {
                    continue;
                }
                let Some(mut d) = digest_with(kernel) else {
                    continue;
                };
                d.update(&data[..at]);
                d.update(&data[at..]);
                assert_eq!(
                    d.finalize(),
                    expected,
                    "{kernel:?} chained at {at}, len {len}"
                );
            }
            if data.len() > 4 {
                let (a, b) = (data.len() / 4, data.len() / 2);
                let Some(mut d) = digest_with(kernel) else {
                    continue;
                };
                d.update(&data[..a]);
                d.update(&data[a..b]);
                d.update(&data[b..]);
                assert_eq!(
                    d.finalize(),
                    expected,
                    "{kernel:?} chained 3-way, len {len}"
                );
            }
        }
    }
}

/// [Crc32c] (the fastest kernel this CPU runs) agrees with [TABLE] too.
#[test]
fn best_agrees_with_table() {
    let mut r = Rng(0x5EED);
    for len in [0usize, 1, 1000, 1 << 16] {
        let data: Vec<u8> = (0..len)
            .map(|_| u8::try_from(r.next() & 0xff).expect("masked to a byte"))
            .collect();
        assert_eq!(Crc32c.checksum(&data), TABLE.checksum(&data));
    }
}
