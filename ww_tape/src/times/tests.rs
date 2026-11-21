use super::*;
use crate::ItemNum;
use crate::chunk::{ColumnSpec, chunk_len, write_chunk};
use crate::tests::id;
use crate::{Block, RelationSet};
use std::vec::Vec;

/// xorshift64*, fixed seeds.
pub(crate) struct Rng(pub(crate) u64);

impl Rng {
    pub(crate) fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    pub(crate) fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// A chunk block of these times, with a payload column after them.
pub(crate) fn block(starts: &[i64], ends: Option<&[i64]>, flags: Option<&[u32]>) -> Vec<u8> {
    let enc = EncodedTimes::encode(starts, ends, flags).expect("valid times");
    let payload = [0xa5u8; 5];
    let mut cols: Vec<(ColumnSpec, &[u8])> = enc.columns().collect();
    cols.push((
        ColumnSpec {
            kind: ColumnKind::Payloads,
            codec: Codec::RAW,
            param: 0,
            len: payload.len(),
        },
        &payload,
    ));
    let specs: Vec<ColumnSpec> = cols.iter().map(|c| c.0).collect();
    let head = enc
        .head
        .chunk_head(TrackId::new(9), TrackNum::new(1), ItemNum::new(1000));
    let mut buf = std::vec![0u8; chunk_len(&head, &specs, 0)];
    let len = write_chunk(&mut buf, id(1), &head, &cols, 0).expect("a valid chunk");
    buf.truncate(len);
    buf
}

/// Every read of `t` against the plain values.
fn check(t: &EventTimes<'_>, starts: &[i64], ends: &[i64], flags: &[u32], r: &mut Rng) {
    let n = starts.len();
    assert_eq!(t.len(), n);
    let s = t.starts();
    for i in 0..n {
        assert_eq!(s.get(i), starts[i], "start {i}");
        assert_eq!(t.end(i), ends[i], "end {i}");
        assert_eq!(t.flag(i), flags[i], "flag {i}");
    }
    assert!(s.iter().eq(starts.iter().copied()), "forward");
    assert!(s.iter().rev().eq(starts.iter().rev().copied()), "backward");
    let (a, b) = (n / 3, n - n / 5);
    assert!(s.range(a..b).eq(starts[a..b].iter().copied()), "range");
    check_both_ends(s, starts);
    check_groups(t, starts, ends, flags);
    check_searches(s, starts, r);
    assert_eq!(t.suspect_items().count(), 0);
}

/// Iterating from both ends at once meets in the middle with the plain values.
fn check_both_ends(s: &Starts<'_>, starts: &[i64]) {
    let n = starts.len();
    let mut both = s.iter();
    let mut k = 0;
    while let (Some(x), Some(y)) = (both.next(), both.next_back()) {
        assert_eq!((x, y), (starts[k], starts[n - 1 - k]), "both ends {k}");
        k += 1;
    }
}

/// Every group of `t` decodes to the plain values, with its first start, flags OR and duration bound.
fn check_groups(t: &EventTimes<'_>, starts: &[i64], ends: &[i64], flags: &[u32]) {
    let s = t.starts();
    let mut buf = [0i64; GROUP];
    let mut fbuf = [0u32; GROUP];
    for g in 0..s.group_count() {
        let m = s.decode_group(g, &mut buf);
        assert_eq!(&buf[..m], &starts[g * GROUP..g * GROUP + m], "group {g}");
        assert_eq!(s.group_first(g), starts[g * GROUP]);
        let m = t.decode_group_ends(g, &mut buf);
        assert_eq!(
            &buf[..m],
            &ends[g * GROUP..g * GROUP + m],
            "ends of group {g}"
        );
        let or = flags[g * GROUP..g * GROUP + m].iter().fold(0, |a, f| a | f);
        assert_eq!(t.group_flags(g), or, "flags OR of group {g}");
        if let Some(f) = t.flags() {
            f.decode_group(g, &mut fbuf);
            assert_eq!(&fbuf[..m], &flags[g * GROUP..g * GROUP + m]);
        }
        if let Ends::Durations(d) = t.ends() {
            let longest = (g * GROUP..g * GROUP + m)
                .map(|i| ends[i].wrapping_sub(starts[i]) as u64)
                .max()
                .unwrap();
            assert!(d.group_bound(g) >= longest, "duration bound of group {g}");
        }
    }
}

/// Lower bound and last-at-or-before agree with the plain values at every start, its neighbours, random and extreme
/// ticks.
fn check_searches(s: &Starts<'_>, starts: &[i64], r: &mut Rng) {
    let n = starts.len();
    let probe = |q: i64| {
        let want = starts.partition_point(|x| *x < q);
        assert_eq!(s.lower_bound(q), want, "lower bound {q}");
        let want = starts.partition_point(|x| *x <= q).checked_sub(1);
        assert_eq!(s.last_at_or_before(q), want, "last at or before {q}");
    };
    for &x in starts.iter().step_by(1 + n / 500) {
        for q in [x.saturating_sub(1), x, x.saturating_add(1)] {
            probe(q);
        }
    }
    let (lo, hi) = (starts[0], starts[n - 1]);
    for _ in 0..500 {
        let span = (hi as i128 - lo as i128 + 3) as u128;
        let q = lo as i128 - 1 + (u128::from(r.next()) % span) as i128;
        probe(q.clamp(i64::MIN as i128, i64::MAX as i128) as i64);
    }
    for q in [i64::MIN, i64::MAX] {
        probe(q);
    }
}

fn round_trip(starts: &[i64], ends: Option<&[i64]>, flags: Option<&[u32]>) -> (TimeMode, EndMode) {
    let b = block(starts, ends, flags);
    let block = Block::read(&b).expect("a good block");
    let chunk = block.chunk().expect("a chunk");
    let t = EventTimes::read(&chunk).expect("times read");
    let zeros = std::vec![0u32; starts.len()];
    let mut r = Rng(starts.len() as u64 + 7);
    check(
        &t,
        starts,
        ends.unwrap_or(starts),
        flags.unwrap_or(&zeros),
        &mut r,
    );
    (t.starts().mode(), chunk.head().end_mode)
}

/// Start tick sets: name and ticks.
pub(crate) fn workloads() -> Vec<(&'static str, Vec<i64>)> {
    let mut r = Rng(11);
    let mut w = Vec::new();
    let bursty = {
        let mut t = 1_700_000_000_000_000_000i64;
        (0..4096)
            .map(|_| {
                t += if r.below(32) == 0 {
                    1_000_000 + r.below(2_000_000_000) as i64
                } else {
                    1000 + r.below(19_000) as i64
                };
                t
            })
            .collect()
    };
    w.push(("bursty", bursty));
    w.push((
        "jitter",
        (0..4096i64)
            .map(|i| 5_000_000_000 + i * 1_000_000 + r.below(600) as i64 - 300)
            .collect(),
    ));
    w.push(("exact", (0..4096i64).map(|i| -7 + i * 1_000_000).collect()));
    w.push((
        "exact with gaps",
        (0..4000i64)
            .map(|i| i * 1000 + if i >= 1500 { 5000 } else { 0 })
            .collect(),
    ));
    w.push(("one", std::vec![42]));
    w.push(("two", std::vec![-5, 1000]));
    w.push(("same tick", std::vec![77; 300]));
    w.push(("duplicates", (0..1000i64).map(|i| i / 7 * 13).collect()));
    w.push((
        "wide",
        (0..700i64)
            .map(|i| i * (1 << 40) + (r.next() >> 30) as i64)
            .collect(),
    ));
    let mut extremes: Vec<i64> = (0..300)
        .map(|_| r.next() as i64)
        .chain([i64::MIN, i64::MAX, 0])
        .collect();
    extremes.sort();
    w.push(("extremes", extremes));
    w.push((
        "frames on 1 ns",
        (0..3000i64)
            .map(|i| i * 1_000_000_000 * 1001 / 60_000)
            .collect(),
    ));
    w.push((
        "sparse",
        (0..129i64)
            .map(|i| i * i * 1_000_003 + (i % 3) * 7)
            .collect(),
    ));
    // gaps of 2^63 ticks and more: periods that don't fit an i64
    w.push(("extreme gaps", std::vec![i64::MIN, 0, 1]));
    w.push(("extreme pair", std::vec![i64::MIN, i64::MAX]));
    w.push(("extreme even", std::vec![i64::MIN, -1, i64::MAX - 1]));
    w
}

#[test]
fn starts_read_back_every_way() {
    for (name, s) in workloads() {
        let (mode, end) = round_trip(&s, None, None);
        assert_eq!(end, EndMode::Absent, "{name}");
        let implicit = matches!(name, "exact" | "one" | "two" | "same tick" | "extreme even");
        assert_eq!(mode == TimeMode::Implicit, implicit, "{name}");
    }
}

#[test]
fn end_modes_and_flags_read_back() {
    let mut r = Rng(5);
    for (name, s) in workloads() {
        let n = s.len();
        let flags: Vec<u32> = (0..n)
            .map(|_| {
                if r.below(10) == 0 {
                    r.next() as u32 >> r.below(32)
                } else {
                    0
                }
            })
            .collect();
        // instants given as ends
        assert_eq!(
            round_trip(&s, Some(&s), Some(&flags)).1,
            EndMode::Absent,
            "{name}"
        );
        // back to back
        let mut next: Vec<i64> = s[1..].to_vec();
        next.push(s[n - 1].saturating_add(3));
        assert_eq!(
            round_trip(&s, Some(&next), None).1,
            EndMode::NextStart,
            "{name}"
        );
        // spans of their own
        let spans: Vec<i64> = s
            .iter()
            .map(|x| x.saturating_add(r.below(1_000_000) as i64))
            .collect();
        let want = if n == 1 {
            EndMode::NextStart
        } else {
            EndMode::Durations
        };
        assert_eq!(round_trip(&s, Some(&spans), Some(&flags)).1, want, "{name}");
    }
}

#[test]
fn zero_flags_take_no_column() {
    let s: Vec<i64> = (0..10).map(|i| i * i).collect();
    let enc = EncodedTimes::encode(&s, None, Some(&[0; 10])).unwrap();
    assert_eq!(enc.columns().count(), 1);
}

#[test]
fn encode_refuses_bad_times() {
    assert_eq!(
        EncodedTimes::encode(&[], None, None),
        Err(EncodeError::Empty)
    );
    assert_eq!(
        EncodedTimes::encode(&[1, 3, 2], None, None),
        Err(EncodeError::Unsorted { at: 2 })
    );
    assert_eq!(
        EncodedTimes::encode(&[1, 3], Some(&[2, 2]), None),
        Err(EncodeError::EndBeforeStart { at: 1 })
    );
    assert_eq!(
        EncodedTimes::encode(&[1, 3], Some(&[2]), None),
        Err(EncodeError::Length)
    );
    assert_eq!(
        EncodedTimes::encode(&[1, 3], None, Some(&[0])),
        Err(EncodeError::Length)
    );
}

#[test]
fn the_slope_narrows_periodic_groups() {
    let s: Vec<i64> = (0..4096i64)
        .map(|i| i * 1_000_000 + (i * 7919) % 601 - 300)
        .collect();
    let enc = EncodedTimes::encode(&s, None, None).unwrap();
    let (_, bytes) = enc.columns().next().unwrap();
    // 25 B per group plus 10 bits per item
    assert_eq!(bytes.len(), 32 * 25 + 4096 * 10 / 8);
}

/// The time columns' byte ranges in a block: payload start and the end of the last time column.
fn time_bytes(b: &[u8]) -> core::ops::Range<usize> {
    let block = Block::read(b).unwrap();
    let c = block.chunk().unwrap();
    let p = c.payload_offset();
    let end = c
        .columns()
        .filter(|c| c.kind != ColumnKind::Payloads)
        .map(|c| c.offset + c.bytes.len())
        .max()
        .unwrap();
    p..p + end
}

/// Read everything there is to read, panicking on nothing.
fn read_all(t: &EventTimes<'_>) {
    let s = t.starts();
    let mut buf = [0i64; GROUP];
    for i in 0..t.len() {
        let _ = (t.start(i), t.end(i), t.flag(i), t.is_suspect(i));
    }
    for g in 0..s.group_count() {
        t.decode_group_ends(g, &mut buf);
        let _ = t.group_flags(g);
        if let Ends::Durations(d) = t.ends() {
            let _ = d.group_bound(g);
        }
    }
    let _ = s.iter().rev().count();
    for q in [i64::MIN, -1, 0, 1 << 40, i64::MAX] {
        let _ = (s.lower_bound(q), s.last_at_or_before(q));
    }
    let _ = t.suspect_items().count();
}

#[test]
fn every_flipped_byte_reads_without_panic() {
    let mut r = Rng(3);
    // every fourth byte, a different quarter per workload; the fuzz target tape-times goes further
    for (k, (_, mut s)) in workloads()
        .into_iter()
        .filter(|w| w.1.len() >= 129)
        .enumerate()
    {
        s.truncate(300);
        let spans: Vec<i64> = s
            .iter()
            .map(|x| x.saturating_add(r.below(1000) as i64))
            .collect();
        let flags: Vec<u32> = (0..s.len()).map(|i| (i % 5 == 0) as u32).collect();
        let good = block(&s, Some(&spans), Some(&flags));
        for at in time_bytes(&good).skip(k % 4).step_by(4) {
            let mut b = good.clone();
            b[at] ^= 1 << (r.below(8) as u32);
            let block = Block::salvage(&b).expect("a payload flip leaves the head good");
            let chunk = block.chunk().unwrap();
            if let Ok(t) = EventTimes::read(&chunk) {
                read_all(&t);
            }
            if let Ok(t) = EventTimes::salvage(&chunk) {
                read_all(&t);
            }
        }
    }
}

#[test]
fn salvage_marks_only_the_bad_group() {
    let s: Vec<i64> = workloads().swap_remove(0).1;
    let good = block(&s, None, None);
    let p = time_bytes(&good).start;
    // group 5's width: byte 20 of its record
    let at = p + 5 * 25 + 20;
    for width in [65u8, 0, 63] {
        let mut b = good.clone();
        b[at] = width;
        let block = Block::salvage(&b).unwrap();
        assert!(!block.is_intact());
        let chunk = block.chunk().unwrap();
        assert!(EventTimes::read(&chunk).is_err(), "width {width}");
        let t = EventTimes::salvage(&chunk).unwrap();
        let bad: Vec<_> = t.suspect_items().collect();
        assert_eq!(bad, std::vec![(5 * GROUP..6 * GROUP)], "width {width}");
        for i in (0..s.len()).filter(|i| i / GROUP != 5) {
            assert_eq!(t.start(i), s[i], "item {i}");
        }
        for i in 5 * GROUP..6 * GROUP {
            assert_eq!(t.start(i), s[5 * GROUP], "stand-in {i}");
        }
        assert_eq!(
            t.starts().lower_bound(s[1000]),
            s.partition_point(|x| *x < s[1000])
        );
    }
    // a flipped residual bit inside a group damages one item and no group: bit 0 of group 0's item 64
    let mut b = good.clone();
    let w = usize::from(good[p + 20]);
    b[p + 32 * 25 + 8 * w] ^= 1;
    let block = Block::salvage(&b).unwrap();
    let t = EventTimes::salvage(&block.chunk().unwrap()).unwrap();
    assert_eq!(t.suspect_items().count(), 0);
    let wrong: Vec<usize> = (0..s.len()).filter(|&i| t.start(i) != s[i]).collect();
    assert_eq!(wrong, std::vec![64]);
}

#[test]
fn salvage_checks_what_a_group_record_says() {
    let s: Vec<i64> = workloads().swap_remove(0).1;
    let good = block(&s, None, None);
    let p = time_bytes(&good).start;
    let rec = |g: usize, at: usize| p + g * 25 + at;
    let w = usize::from(good[rec(5, 20)]);
    let res5 = p
        + 32 * 25
        + usize::from_le_bytes([
            good[rec(5, 21)],
            good[rec(5, 22)],
            good[rec(5, 23)],
            0,
            0,
            0,
            0,
            0,
        ]);
    // (byte, bit, the groups that go suspect): a low bit of the residual minimum or of item 0's residual breaks
    // item 0 == first tick; a first tick or slope that breaks the order with a neighbour marks both, either may be
    // the damaged one
    let cases = [
        (rec(5, 16), 0x01, 5..6),
        (res5, 0x01, 5..6),
        (rec(5, 7), 0x01, 5..6),
        (rec(5, 3), 0x80, 4..6),
        (rec(5, 14), 0x01, 5..7),
    ];
    assert!(w > 0, "group 5 has residuals");
    for (at, bit, groups) in cases {
        let mut b = good.clone();
        b[at] ^= bit;
        let block = Block::salvage(&b).unwrap();
        let chunk = block.chunk().unwrap();
        assert!(EventTimes::read(&chunk).is_err(), "byte {at}");
        let t = EventTimes::salvage(&chunk).unwrap();
        let bad: Vec<_> = t.suspect_items().collect();
        let want: Vec<_> = groups.map(|g| g * GROUP..(g + 1) * GROUP).collect();
        assert_eq!(bad, want, "byte {at}");
        for i in (0..s.len()).filter(|i| !t.is_suspect(*i)) {
            assert_eq!(t.start(i), s[i], "byte {at} item {i}");
        }
        let starts = t.starts();
        assert!(starts.iter().all(|x| (s[0]..=s[s.len() - 1]).contains(&x)));
    }
    // a residual pushed past the chunk's last tick reads clamped: the top bit of group 31's item 64
    let mut b = good.clone();
    let w31 = usize::from(good[rec(31, 20)]);
    let res31 = usize::from_le_bytes([
        good[rec(31, 21)],
        good[rec(31, 22)],
        good[rec(31, 23)],
        0,
        0,
        0,
        0,
        0,
    ]);
    let bit = 64 * w31 + w31 - 1;
    b[p + 32 * 25 + res31 + bit / 8] ^= 1 << (bit % 8);
    let block = Block::salvage(&b).unwrap();
    let t = EventTimes::salvage(&block.chunk().unwrap()).unwrap();
    let i = 31 * GROUP + 64;
    assert_ne!(t.start(i), s[i]);
    assert!(t.start(i) <= s[s.len() - 1]);
}

/// The block offset of the first byte of the `kind` column.
fn column_start(b: &[u8], kind: ColumnKind) -> usize {
    let block = Block::read(b).unwrap();
    let c = block.chunk().unwrap();
    c.payload_offset() + c.columns().find(|c| c.kind == kind).unwrap().offset
}

/// Per packed column: kind, record length, where the width (a u8, or a u32 OR for flags) and the offset sit.
const RECORDS: [(ColumnKind, usize, usize, usize); 3] = [
    (ColumnKind::Starts, 25, 20, 21),
    (ColumnKind::Ends, 13, 8, 9),
    (ColumnKind::Flags, 8, 0, 4),
];

#[test]
fn salvage_bounds_every_group_by_the_column() {
    // offsets that agree with each other but point far past the column
    let s: Vec<i64> = (0..129i64).map(|i| i * i).collect();
    let spans: Vec<i64> = s
        .iter()
        .enumerate()
        .map(|(i, x)| x + (i % 7) as i64)
        .collect();
    let flags: Vec<u32> = (0..129).map(|i| (i % 3) as u32).collect();
    let good = block(&s, Some(&spans), Some(&flags));
    for (kind, len, width, offset) in RECORDS {
        let mut b = good.clone();
        let c = column_start(&b, kind);
        if kind == ColumnKind::Flags {
            b[c + width..c + width + 4].copy_from_slice(&1u32.to_le_bytes());
        } else {
            b[c + width] = 1;
        }
        b[c + offset..c + offset + 4].copy_from_slice(&0x0100_0000u32.to_le_bytes());
        b[c + len + offset..c + len + offset + 4].copy_from_slice(&0x0100_0010u32.to_le_bytes());
        let block = Block::salvage(&b).unwrap();
        let chunk = block.chunk().unwrap();
        assert!(EventTimes::read(&chunk).is_err(), "{kind:?}");
        let t = EventTimes::salvage(&chunk).unwrap();
        read_all(&t);
        assert!(t.is_suspect_group(0), "{kind:?}");
    }
}

#[test]
fn a_damaged_record_marks_its_group_only() {
    let s: Vec<i64> = workloads().swap_remove(0).1;
    let mut r = Rng(5);
    let spans: Vec<i64> = s.iter().map(|x| x + r.below(5000) as i64).collect();
    let flags: Vec<u32> = (0..s.len()).map(|i| (i % 3) as u32).collect();
    let good = block(&s, Some(&spans), Some(&flags));
    for (kind, len, width, offset) in RECORDS {
        // a flipped offset breaks the links on both sides of group 5; a flipped width only the one after it
        for (at, bit) in [(offset, 0x04), (offset + 1, 0x01), (width, 0x04)] {
            let mut b = good.clone();
            let c = column_start(&b, kind);
            b[c + 5 * len + at] ^= bit;
            let block = Block::salvage(&b).unwrap();
            let t = EventTimes::salvage(&block.chunk().unwrap()).unwrap();
            let bad: Vec<_> = t.suspect_items().collect();
            assert_eq!(bad, std::vec![(5 * GROUP..6 * GROUP)], "{kind:?} byte {at}");
            for i in (0..s.len()).filter(|i| i / GROUP != 5) {
                let got = (t.start(i), t.end(i), t.flag(i));
                assert_eq!(
                    got,
                    (s[i], spans[i], flags[i]),
                    "{kind:?} byte {at} item {i}"
                );
            }
        }
    }
}

#[test]
fn salvage_keeps_durations_and_flags_in_bounds() {
    let s: Vec<i64> = (0..300i64).map(|i| i * 1000 + (i * i) % 17).collect();
    let spans: Vec<i64> = s
        .iter()
        .enumerate()
        .map(|(i, x)| x + (i % 100) as i64)
        .collect();
    let flags: Vec<u32> = (0..300).map(|i| (i % 4 == 0) as u32 * 2).collect();
    let good = block(&s, Some(&spans), Some(&flags));
    let latest = *spans.iter().max().unwrap();
    // bit 18 of group 2's duration minimum: in range, but its items end past the latest end; bit 0 of item 64's
    // flags in group 1 (OR 2, width 2)
    let mut b = good.clone();
    let e = column_start(&good, ColumnKind::Ends);
    b[e + 2 * 13 + 2] ^= 0x04;
    let f = column_start(&good, ColumnKind::Flags);
    let f_off = usize::from_le_bytes([good[f + 8 + 4], good[f + 8 + 5], 0, 0, 0, 0, 0, 0]);
    b[f + 3 * 8 + f_off + 64 * 2 / 8] ^= 1;
    let block = Block::salvage(&b).unwrap();
    let t = EventTimes::salvage(&block.chunk().unwrap()).unwrap();
    assert_eq!(t.suspect_items().count(), 0);
    let i = GROUP + 64;
    assert_eq!(
        t.flag(i),
        flags[i] & 2,
        "a bit outside the group's OR is dropped"
    );
    let mut out = [0u32; GROUP];
    t.flags().unwrap().decode_group(1, &mut out);
    assert!(out.iter().all(|f| f & !2 == 0));
    assert_eq!(t.end(299), latest, "clamped to the latest end");
    let mut ends = [0i64; GROUP];
    let n = t.decode_group_ends(2, &mut ends);
    for (j, e) in ends[..n].iter().enumerate() {
        assert_eq!(*e, t.end(2 * GROUP + j));
        assert!(*e <= latest);
    }
}

#[test]
fn an_intact_column_is_read_strictly_in_salvage() {
    let s: Vec<i64> = workloads().swap_remove(0).1;
    let flags: Vec<u32> = (0..s.len()).map(|i| (i % 3) as u32).collect();
    let good = block(&s, None, Some(&flags));
    let mut b = good.clone();
    // damage the payload column after the time columns: the times read as they are
    let end = time_bytes(&good).end;
    b[end + 1] ^= 0xff;
    let block = Block::salvage(&b).unwrap();
    let t = EventTimes::salvage(&block.chunk().unwrap()).unwrap();
    assert_eq!(t.suspect_items().count(), 0);
    assert_eq!(t.start(4000), s[4000]);
}

#[test]
fn a_salvaged_chunk_has_no_suspect_past_its_items() {
    let s: Vec<i64> = (0..300i64).map(|i| i * i).collect();
    let spans: Vec<i64> = s.iter().map(|x| x + 3).collect();
    let flags: Vec<u32> = (0..300).map(|i| (i % 3) as u32).collect();
    let good = block(&s, Some(&spans), Some(&flags));
    let mut b = good.clone();
    // the residual minimum of group 1: the starts column is read by salvage
    b[column_start(&good, ColumnKind::Starts) + 25 + 16] ^= 0x01;
    let block = Block::salvage(&b).unwrap();
    let t = EventTimes::salvage(&block.chunk().unwrap()).unwrap();
    assert!(t.suspect_items().eq(core::iter::once(GROUP..2 * GROUP)));
    for i in [s.len(), 3 * GROUP, usize::MAX] {
        assert!(!t.is_suspect(i), "item {i}");
    }
    for g in [3, 1000, usize::MAX] {
        assert!(!t.is_suspect_group(g), "group {g}");
        assert!(!t.starts().is_suspect_group(g), "group {g}");
        assert!(!t.flags().unwrap().is_suspect_group(g), "group {g}");
        let Ends::Durations(d) = t.ends() else {
            panic!("durations")
        };
        assert!(!d.is_suspect_group(g), "group {g}");
    }
}

#[test]
fn read_refuses_wrong_shapes() {
    let s = [1i64, 5, 6];
    let enc = EncodedTimes::encode(&s, None, None).unwrap();
    let cols: Vec<(ColumnSpec, &[u8])> = enc.columns().collect();
    let write = |head: crate::ChunkHead, cols: &[(ColumnSpec, &[u8])]| {
        let specs: Vec<ColumnSpec> = cols.iter().map(|c| c.0).collect();
        let mut buf = std::vec![0u8; chunk_len(&head, &specs, 0)];
        let len = write_chunk(&mut buf, id(1), &head, cols, 0).unwrap();
        buf.truncate(len);
        buf
    };
    let head = enc
        .head
        .chunk_head(TrackId::new(1), TrackNum::new(1), ItemNum::new(0));
    let read = |b: &[u8]| {
        let block = Block::read(b).unwrap();
        EventTimes::read(&block.chunk().unwrap()).map(|_| ())
    };
    assert_eq!(read(&write(head, &cols)), Ok(()));
    let mut h = head;
    h.time_mode = 7;
    assert_eq!(read(&write(h, &cols)), Err(TimesError::UnknownMode(7)));
    h = head;
    h.end_mode = EndMode::Durations;
    h.latest_end = 9;
    assert_eq!(
        read(&write(h, &cols)),
        Err(TimesError::Missing(ColumnKind::Ends))
    );
    let mut raw = cols.clone();
    raw[0].0.codec = Codec::RAW;
    assert_eq!(
        read(&write(head, &raw)),
        Err(TimesError::UnknownCodec(ColumnKind::Starts, Codec::RAW))
    );
    h = head;
    h.last_tick = 7;
    h.latest_end = 7;
    assert!(matches!(
        read(&write(h, &cols)),
        Err(TimesError::Corrupt { .. })
    ));
}

/// The golden chunks: each workload cut to 300 items, with spans and flags on some.
fn golden_blocks() -> Vec<Vec<u8>> {
    let mut r = Rng(17);
    let mut out = Vec::new();
    for (k, (_, mut s)) in workloads().into_iter().enumerate() {
        s.truncate(300);
        let n = s.len();
        out.push(block(&s, None, None));
        if k % 3 == 0 {
            let spans: Vec<i64> = s
                .iter()
                .map(|x| x.saturating_add(r.below(5000) as i64))
                .collect();
            let flags: Vec<u32> = (0..n)
                .map(|i| if i % 9 == 0 { 1 << (i % 5) } else { 0 })
                .collect();
            out.push(block(&s, Some(&spans), Some(&flags)));
        }
        if k % 4 == 1 {
            let mut next = s[1..].to_vec();
            next.push(s[n - 1].saturating_add(1));
            out.push(block(&s, Some(&next), None));
        }
    }
    out
}

/// The same times give the same bytes, and they are golden/v1-times.hex's (`WW_TAPE_GOLDEN=update` rewrites it):
/// one chunk block per line, in hex.
#[test]
fn times_deterministic_and_golden() {
    let blocks = golden_blocks();
    assert_eq!(blocks, golden_blocks(), "two encodes of the same times");
    let mut hex = String::new();
    for b in &blocks {
        for x in b {
            hex.push_str(&std::format!("{x:02x}"));
        }
        hex.push('\n');
    }
    let golden = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("golden/v1-times.hex");
    if std::env::var("WW_TAPE_GOLDEN").as_deref() == Ok("update") {
        std::fs::write(&golden, &hex).unwrap();
    }
    let want = std::fs::read_to_string(&golden)
        .expect("golden/v1-times.hex: run with WW_TAPE_GOLDEN=update");
    assert_eq!(
        hex, want,
        "the format changed: bytes differ from golden/v1-times.hex"
    );
    // and the golden blocks read back to the workloads' starts
    let starts: Vec<Vec<i64>> = workloads()
        .into_iter()
        .map(|(_, mut s)| {
            s.truncate(300);
            s
        })
        .collect();
    for line in want.lines() {
        let b: Vec<u8> = (0..line.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&line[i..i + 2], 16).unwrap())
            .collect();
        let block = Block::read(&b).unwrap();
        let t = EventTimes::read(&block.chunk().unwrap()).unwrap();
        let got: Vec<i64> = t.starts().iter().collect();
        assert!(starts.contains(&got));
    }
}

/// A chunk block of `n` instants with the relation columns `relations` (index, values, monotone) after the times.
fn relation_block(relations: &[(u16, &[u64], bool)]) -> Vec<u8> {
    let n = relations.first().map_or(1, |r| r.1.len());
    let starts: Vec<i64> = (0..n as i64).collect();
    let enc = EncodedTimes::encode(&starts, None, None).expect("valid times");
    let encoded: Vec<EncodedRelation> = relations
        .iter()
        .map(|(i, v, m)| EncodedRelation::encode(*i, v, *m).expect("values encode"))
        .collect();
    let mut cols: Vec<(ColumnSpec, &[u8])> = enc.columns().collect();
    cols.extend(encoded.iter().map(EncodedRelation::column));
    let specs: Vec<ColumnSpec> = cols.iter().map(|c| c.0).collect();
    let head = enc
        .head
        .chunk_head(TrackId::new(9), TrackNum::new(1), ItemNum::new(1000));
    let mut buf = std::vec![0u8; chunk_len(&head, &specs, 0)];
    let len = write_chunk(&mut buf, id(1), &head, &cols, 0).expect("a valid chunk");
    buf.truncate(len);
    buf
}

/// Every read of relation 3 in a chunk of `values` against the plain values: get, range, lower bound and its codec.
fn relation_round_trip(values: &[u64], monotone: bool) {
    let b = relation_block(&[(3, values, monotone)]);
    let block = Block::read(&b).expect("a good block");
    let chunk = block.chunk().expect("a chunk");
    let stored = StoredRelations {
        stored: RelationSet::single(3),
        monotone: RelationSet::from_bits(u16::from(monotone) << 3),
    };
    assert!(
        RelationColumn::read(&chunk, stored, 2)
            .expect("reads")
            .is_none(),
        "no column of relation 2"
    );
    let c = RelationColumn::read(&chunk, stored, 3)
        .expect("reads")
        .expect("relation 3's column");
    assert_eq!(c.len(), values.len());
    for (i, v) in values.iter().enumerate() {
        assert_eq!(c.get(i), *v, "value {i}");
    }
    let lo = values.iter().copied().min().unwrap();
    let hi = values.iter().copied().max().unwrap();
    assert_eq!(c.range(), (lo, hi));
    let sorted = values.windows(2).all(|w| w[0] <= w[1]);
    assert_eq!(
        c.codec() == STARTS,
        sorted,
        "codec 1 exactly when the values never decrease"
    );
    match c.lower_bound(lo) {
        Some(_) => {
            let mut probes: Vec<u64> = values
                .iter()
                .flat_map(|v| [v.saturating_sub(1), *v, v.saturating_add(1)])
                .collect();
            probes.extend([0, u64::MAX]);
            for p in probes {
                assert_eq!(
                    c.lower_bound(p),
                    Some(values.partition_point(|v| *v < p)),
                    "lower bound of {p}"
                );
            }
        }
        None => assert!(!sorted),
    }
}

#[test]
fn relation_values_read_as_written() {
    let mut r = Rng(0x7e1a);
    for n in [1usize, 2, 127, 128, 129, 300, 4096] {
        // a source ordinal per output item: 1:1, with drops, several children per parent, sparse
        for step in [
            |_: &mut Rng| 1u64,
            |r: &mut Rng| 1 + u64::from(r.below(50) == 0),
            |r: &mut Rng| u64::from(r.below(10) == 0),
            |r: &mut Rng| 500 + r.below(1001),
        ] {
            let mut v = 1_000_000u64;
            let values: Vec<u64> = (0..n)
                .map(|_| {
                    v += step(&mut r);
                    v
                })
                .collect();
            relation_round_trip(&values, true);
            relation_round_trip(&values, false);
        }
        // any values: keys, wide and small
        let keys: Vec<u64> = (0..n).map(|_| r.next()).collect();
        relation_round_trip(&keys, false);
        let small: Vec<u64> = (0..n).map(|_| r.below(20)).collect();
        relation_round_trip(&small, false);
    }
    // the edges of u64, either side of i64's sign
    relation_round_trip(
        &[0, 0, 1, (1 << 63) - 1, 1 << 63, u64::MAX - 1, u64::MAX],
        true,
    );
    relation_round_trip(&[u64::MAX, 0, 1 << 63, (1 << 63) - 1], false);
}

#[test]
fn a_monotone_relation_refuses_a_decrease() {
    assert_eq!(
        EncodedRelation::encode(0, &[1, 2, 2, 1], true),
        Err(EncodeError::Unsorted { at: 3 })
    );
    assert_eq!(
        EncodedRelation::encode(0, &[], false),
        Err(EncodeError::Empty)
    );
    let c = EncodedRelation::encode(0, &[5, 4], false).expect("any values");
    assert_eq!(c.column().0.codec, DURATIONS);
}

/// A relation column holds its own item count: values that decode for as many items as the chunk holds but were
/// encoded for fewer, or a count other than the chunk's, are refused, in either codec.
#[test]
fn a_relation_column_of_another_count_is_refused() {
    let starts = [1i64, 2, 3, 4, 5];
    let enc = EncodedTimes::encode(&starts, None, None).expect("valid times");
    let track = StoredRelations {
        stored: RelationSet::from_bits(1),
        monotone: RelationSet::from_bits(0),
    };
    for values in [
        &[5u64, 4][..],
        &[1, 2],
        &[5, 4, 3, 2, 1, 0],
        &[1, 2, 3, 4, 5, 6],
    ] {
        let rel = EncodedRelation::encode(0, values, false).expect("values encode");
        let mut cols: Vec<(ColumnSpec, &[u8])> = enc.columns().collect();
        cols.push(rel.column());
        let specs: Vec<ColumnSpec> = cols.iter().map(|c| c.0).collect();
        let head = enc
            .head
            .chunk_head(TrackId::new(9), TrackNum::new(1), ItemNum::new(1000));
        let mut b = std::vec![0u8; chunk_len(&head, &specs, 0)];
        let len = write_chunk(&mut b, id(1), &head, &cols, 0).expect("a valid chunk");
        b.truncate(len);
        let block = Block::read(&b).expect("a good block");
        let chunk = block.chunk().unwrap();
        assert_eq!(
            RelationColumn::read(&chunk, track, 0).err(),
            Some(TimesError::Corrupt {
                kind: ColumnKind::Relation,
                error: ColumnError::Corrupt("an item count other than the one asked for")
            }),
            "{values:?}"
        );
    }
}

#[test]
fn a_relation_column_in_an_unknown_codec_is_refused_and_the_times_still_read() {
    let starts = [1i64, 2, 3];
    let enc = EncodedTimes::encode(&starts, None, None).expect("valid times");
    let odd = [0xa5u8; 24];
    let mut cols: Vec<(ColumnSpec, &[u8])> = enc.columns().collect();
    cols.push((
        ColumnSpec {
            kind: ColumnKind::Relation,
            codec: Codec(77),
            param: 0,
            len: odd.len(),
        },
        &odd,
    ));
    let specs: Vec<ColumnSpec> = cols.iter().map(|c| c.0).collect();
    let head = enc
        .head
        .chunk_head(TrackId::new(9), TrackNum::new(1), ItemNum::new(1000));
    let mut b = std::vec![0u8; chunk_len(&head, &specs, 0)];
    let len = write_chunk(&mut b, id(1), &head, &cols, 0).expect("a valid chunk");
    b.truncate(len);
    let block = Block::read(&b).expect("a good block");
    let chunk = block.chunk().unwrap();
    assert_eq!(
        RelationColumn::read(
            &chunk,
            StoredRelations {
                stored: RelationSet::from_bits(1),
                monotone: RelationSet::from_bits(0)
            },
            0
        )
        .err(),
        Some(TimesError::UnknownCodec(ColumnKind::Relation, Codec(77)))
    );
    assert_eq!(EventTimes::read(&chunk).expect("times read").len(), 3);
}

/// Relation 0 stored and monotone.
const ONE_MONOTONE: StoredRelations = StoredRelations {
    stored: RelationSet::from_bits(1),
    monotone: RelationSet::from_bits(1),
};

#[test]
fn a_starts_coded_relation_column_that_does_not_read_names_the_relation() {
    let starts = [0i64, 10, 20, 30, 40];
    let enc = EncodedTimes::encode(&starts, None, None).expect("valid times");
    // one period for 2 values, and a line of 3 irregular values: neither spans the chunk's 5 items
    for values in [&[10u64, 11][..], &[1, 5, 6]] {
        let rel = EncodedRelation::encode(0, values, true).expect("values encode");
        assert_eq!(rel.column().0.codec, STARTS);
        let mut cols: Vec<(ColumnSpec, &[u8])> = enc.columns().collect();
        cols.push(rel.column());
        let specs: Vec<ColumnSpec> = cols.iter().map(|c| c.0).collect();
        let head = enc
            .head
            .chunk_head(TrackId::new(9), TrackNum::new(1), ItemNum::new(1000));
        let mut b = std::vec![0u8; chunk_len(&head, &specs, 0)];
        let len = write_chunk(&mut b, id(1), &head, &cols, 0).expect("a valid chunk");
        b.truncate(len);
        let block = Block::read(&b).expect("a good block");
        let chunk = block.chunk().unwrap();
        assert!(
            matches!(
                RelationColumn::read(&chunk, ONE_MONOTONE, 0),
                Err(TimesError::Corrupt {
                    kind: ColumnKind::Relation,
                    ..
                })
            ),
            "{values:?}: {:?}",
            RelationColumn::read(&chunk, ONE_MONOTONE, 0)
        );
        assert_eq!(EventTimes::read(&chunk).expect("times read").len(), 5);
    }
}

/// A chunk's relation columns are read against the track's definition: a column of a relation it keeps none for,
/// a stored relation without one, a monotone relation's column in DURATIONS, each refused naming the relation.
#[test]
fn relation_columns_are_checked_against_the_track_on_read() {
    let up: Vec<u64> = (0..300).collect();
    let down: Vec<u64> = (0..300).rev().collect();
    let b = relation_block(&[(0, &up, true), (2, &down, false)]);
    let block = Block::read(&b).expect("a good block");
    let chunk = block.chunk().expect("a chunk");
    let both = StoredRelations {
        stored: RelationSet::from_bits(0b101),
        monotone: RelationSet::from_bits(0b001),
    };
    assert_eq!(
        RelationColumn::read(&chunk, both, 2)
            .unwrap()
            .unwrap()
            .get(0),
        299
    );
    let cases = [
        // relation 2 keeps no column
        (
            StoredRelations {
                stored: RelationSet::from_bits(0b001),
                monotone: RelationSet::from_bits(0b001),
            },
            2,
        ),
        // relation 1 keeps one the chunk lacks
        (
            StoredRelations {
                stored: RelationSet::from_bits(0b111),
                monotone: RelationSet::from_bits(0b001),
            },
            1,
        ),
        // relation 2 never decreases, its column is in DURATIONS
        (
            StoredRelations {
                stored: RelationSet::from_bits(0b101),
                monotone: RelationSet::from_bits(0b101),
            },
            2,
        ),
    ];
    for (track, relation) in cases {
        for read in [RelationColumn::read, RelationColumn::salvage] {
            assert_eq!(
                read(&chunk, track, 0).err(),
                Some(TimesError::Relation { relation }),
                "{track:?}"
            );
        }
    }
}

/// A relation column with a damaged group record reads by salvage, as the times do: that group's items are suspect,
/// the others read as written; a strict read refuses it.
#[test]
fn a_damaged_relation_column_is_salvaged_as_times_are() {
    let mut r = Rng(0x5a1e);
    let mut v = 0u64;
    let values: Vec<u64> = (0..1000)
        .map(|_| {
            v += r.below(7);
            v
        })
        .collect();
    let good = relation_block(&[(1, &values, true)]);
    let stored = StoredRelations {
        stored: RelationSet::from_bits(0b10),
        monotone: RelationSet::from_bits(0b10),
    };
    let at = {
        let block = Block::read(&good).unwrap();
        let chunk = block.chunk().unwrap();
        let c = chunk
            .columns()
            .find(|c| c.kind == ColumnKind::Relation)
            .unwrap();
        // group 3's width: byte 20 of its 25-byte record, after the column's 17-byte head
        chunk.payload_offset() + c.offset + 17 + 3 * 25 + 20
    };
    let mut b = good.clone();
    b[at] = 65;
    let block = Block::salvage(&b).unwrap();
    assert!(!block.is_intact());
    let chunk = block.chunk().unwrap();
    assert!(RelationColumn::read(&chunk, stored, 1).is_err());
    let c = RelationColumn::salvage(&chunk, stored, 1).unwrap().unwrap();
    assert_eq!(
        c.suspect_items().collect::<Vec<_>>(),
        std::vec![3 * GROUP..4 * GROUP]
    );
    for i in (0..values.len()).filter(|i| i / GROUP != 3) {
        assert_eq!(c.get(i), values[i], "item {i}");
        assert!(!c.is_suspect(i));
    }
    assert!(EventTimes::salvage(&chunk).is_ok(), "the times read too");
}

/// A monotone relation column in [STARTS] with any one byte changed either is refused or reads values that never
/// decrease, with `range` their smallest and largest and `lower_bound` agreeing with a search of them.
#[test]
fn a_mutated_monotone_relation_column_reads_in_order_or_not_at_all() {
    let mut r = Rng(0xfeed);
    let mut v = 1_000u64;
    let values: Vec<u64> = (0..300)
        .map(|_| {
            v += r.below(40);
            v
        })
        .collect();
    let good = relation_block(&[(0, &values, true)]);
    let start = {
        let block = Block::read(&good).unwrap();
        let chunk = block.chunk().unwrap();
        let c = chunk
            .columns()
            .find(|c| c.kind == ColumnKind::Relation)
            .unwrap();
        assert_eq!(c.codec, STARTS);
        chunk.payload_offset() + c.offset..chunk.payload_offset() + c.offset + c.bytes.len()
    };
    let mut read_ok = 0;
    for at in start.clone() {
        for x in [1u8, 2, 4, 8, 16, 32, 64, 128, 0xff] {
            let mut b = good.clone();
            b[at] ^= x;
            // the payload's CRC no longer holds: read the column as an intact one, as a reader trusting it would
            let block = Block::salvage(&b).unwrap();
            let chunk = block.chunk().unwrap();
            let Ok(Some(c)) = RelationColumn::read(&chunk, ONE_MONOTONE, 0) else {
                continue;
            };
            read_ok += 1;
            let got: Vec<u64> = (0..c.len()).map(|i| c.get(i)).collect();
            let what = std::format!("byte {} ^ {x:#x}", at - start.start);
            assert!(got.windows(2).all(|w| w[0] <= w[1]), "{what}: decreases");
            let (lo, hi) = (got[0], got[got.len() - 1]);
            assert_eq!(c.range(), (lo, hi), "{what}: range");
            for q in got
                .iter()
                .flat_map(|v| [v.saturating_sub(1), *v, v.saturating_add(1)])
                .chain([0, u64::MAX])
            {
                assert_eq!(
                    c.lower_bound(q),
                    Some(got.partition_point(|v| *v < q)),
                    "{what}: lower bound of {q}"
                );
            }
        }
    }
    assert!(read_ok > 0, "some changes read, as the slope's low bits do");
    // the reviewer's case: group 0's slope, lowest byte, its top bit
    let mut b = good.clone();
    b[start.start + 4 + 17 + 8] ^= 0x80;
    let block = Block::salvage(&b).unwrap();
    assert_eq!(
        RelationColumn::read(&block.chunk().unwrap(), ONE_MONOTONE, 0).err(),
        Some(TimesError::Corrupt {
            kind: ColumnKind::Relation,
            error: ColumnError::Corrupt("group's items decrease")
        })
    );
}

/// The starts column shares the relation's code: with any one byte changed, starts that read never decrease and
/// stay between the chunk's first and last tick; in salvage, a group whose items decrease is suspect.
#[test]
fn mutated_starts_read_in_order_or_not_at_all() {
    let mut r = Rng(0x0dd);
    let mut t = 0i64;
    let starts: Vec<i64> = (0..300)
        .map(|_| {
            t += r.below(40) as i64;
            t
        })
        .collect();
    let good = block(&starts, None, None);
    let cols = time_bytes(&good);
    let (mut read_ok, mut suspect) = (0, 0);
    for at in cols.clone() {
        for x in [1u8, 2, 4, 8, 16, 32, 64, 128, 0xff] {
            let mut b = good.clone();
            b[at] ^= x;
            let block = Block::salvage(&b).unwrap();
            let chunk = block.chunk().unwrap();
            let what = std::format!("byte {} ^ {x:#x}", at - cols.start);
            if let Ok(times) = EventTimes::read(&chunk) {
                read_ok += 1;
                let s = times.starts();
                let got: Vec<i64> = s.iter().collect();
                assert!(got.windows(2).all(|w| w[0] <= w[1]), "{what}: decreases");
                assert_eq!(
                    (got[0], got[got.len() - 1]),
                    (s.first_tick(), s.last_tick()),
                    "{what}: first and last"
                );
            }
            if let Ok(times) = EventTimes::salvage(&chunk) {
                let s = times.starts();
                for g in 0..s.group_count() {
                    let items = g * GROUP..((g + 1) * GROUP).min(s.len());
                    let got: Vec<i64> = items.clone().map(|i| s.get(i)).collect();
                    if !times.is_suspect(items.start) {
                        assert!(
                            got.windows(2).all(|w| w[0] <= w[1]),
                            "{what}: group {g} decreases, not suspect"
                        );
                    } else {
                        suspect += 1;
                    }
                }
            }
        }
    }
    assert!(
        read_ok > 0 && suspect > 0,
        "{read_ok} read, {suspect} suspect groups"
    );
}
