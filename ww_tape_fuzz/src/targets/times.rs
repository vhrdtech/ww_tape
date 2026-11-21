//! ww_tape's event time codec on any bytes: a chunk block's times read and salvage without panic; times built from
//! the input read back exactly and survive, without panic, a flipped bit and input bytes written over their columns
//! (a good head over a damaged payload, as salvage meets it); every bit-pack kernel decodes any bytes as the scalar
//! code does.
use ww_tape::TrackNum;
use ww_tape::bitpack::{self, GROUP, Kernel};
use ww_tape::chunk::{chunk_len, write_chunk};
use ww_tape::times::{
    EncodedRelation, EncodedTimes, Ends, EventTimes, RelationColumn, StoredRelations,
};
use ww_tape::{Block, ColumnSpec};
use ww_tape::{RelationSet, TrackId};

/// Items read one by one in full; past it, the first and last half of these and a stride between. An implicit chunk
/// may state up to [ww_tape::times::ITEMS_MAX] items in a few bytes, and every item read four ways under the
/// fuzzer's instrumentation takes seconds.
const ITEMS_EACH: usize = 1 << 16;

/// The items [read_all] reads one by one: all of `n` up to [ITEMS_EACH], else both ends and a stride.
fn item_sample(n: usize) -> impl Iterator<Item = usize> {
    let (head, tail) = match n <= ITEMS_EACH {
        true => (n, n),
        false => (ITEMS_EACH / 2, n - ITEMS_EACH / 2),
    };
    let stride = (tail - head) / ITEMS_EACH + 1;
    (0..head).chain((head..tail).step_by(stride)).chain(tail..n)
}

/// Read everything the times give.
fn read_all(t: &EventTimes<'_>) {
    let s = t.starts();
    let mut buf = [0i64; GROUP];
    for i in item_sample(t.len()) {
        let _ = (t.start(i), t.end(i), t.flag(i), t.is_suspect(i));
    }
    for g in 0..s.group_count() {
        assert!(t.decode_group_ends(g, &mut buf) <= GROUP);
        let _ = t.group_flags(g);
        if let Ends::Durations(d) = t.ends() {
            let _ = d.group_bound(g);
        }
    }
    assert_eq!(s.iter().rev().count(), t.len());
    for q in [
        i64::MIN,
        -1,
        0,
        1 << 40,
        i64::MAX,
        s.first_tick(),
        s.last_tick(),
    ] {
        assert!(s.lower_bound(q) <= t.len());
        let _ = s.last_at_or_before(q);
    }
    let _ = t.suspect_items().count();
}

/// Read everything a relation column gives.
fn read_relation(r: &RelationColumn<'_>) {
    for i in item_sample(r.len()) {
        let _ = r.get(i);
    }
    let (lo, hi) = r.range();
    for q in [0, 1, lo, hi, hi.saturating_add(1), u64::MAX] {
        assert!(r.lower_bound(q).is_none_or(|i| i <= r.len()));
    }
}

/// Starts read whole (not by salvage) never decrease and run from the chunk's first tick to its last.
fn starts_in_order(t: &EventTimes<'_>) {
    let s = t.starts();
    let got: Vec<i64> = item_sample(t.len()).map(|i| s.get(i)).collect();
    assert!(got.windows(2).all(|w| w[0] <= w[1]), "starts decrease");
    assert_eq!(got.first(), Some(&s.first_tick()), "the first start");
    assert_eq!(got.last(), Some(&s.last_tick()), "the last start");
}

/// A relation column read whole in [STARTS](ww_tape::times::STARTS): its values never decrease, `range` is their
/// smallest and largest, and `lower_bound` agrees with a search of them.
fn relation_in_order(r: &RelationColumn<'_>) {
    let n = r.len();
    let got: Vec<u64> = item_sample(n).map(|i| r.get(i)).collect();
    assert!(
        got.windows(2).all(|w| w[0] <= w[1]),
        "a monotone relation decreases"
    );
    let (Some(&lo), Some(&hi)) = (got.first(), got.last()) else {
        return;
    };
    assert_eq!(r.range(), (lo, hi), "range");
    let some = got.iter().step_by(got.len() / 8 + 1).copied();
    for q in some
        .flat_map(|v| [v.saturating_sub(1), v, v.saturating_add(1)])
        .chain([0, hi, u64::MAX])
    {
        let at = r.lower_bound(q).expect("a column in STARTS is searched");
        assert!(
            at == n || r.get(at) >= q,
            "lower bound of {q}: item {at} below it"
        );
        assert!(
            at == 0 || r.get(at - 1) < q,
            "lower bound of {q}: item before {at} at or above it"
        );
        if n <= ITEMS_EACH {
            assert_eq!(at, got.partition_point(|v| *v < q), "lower bound of {q}");
        }
    }
}

pub(crate) fn walk(block: &[u8]) {
    if let Ok(b) = Block::salvage(block)
        && let Some(c) = b.chunk()
    {
        // the relations the chunk's own columns name (a STARTS column monotone), relation 0 kept the other way, relation 2
        // monotone the other way
        let own = c
            .columns()
            .filter(|col| col.kind == ww_tape::ColumnKind::Relation && col.param < 16)
            .fold(StoredRelations::default(), |mut s, col| {
                s.stored.insert(col.param);
                if col.codec == ww_tape::times::STARTS {
                    s.monotone.insert(col.param);
                }
                s
            });
        let others = [
            StoredRelations {
                stored: RelationSet::from_bits(own.stored.bits() ^ 1),
                ..own
            },
            StoredRelations {
                monotone: RelationSet::from_bits(own.monotone.bits() ^ 4),
                ..own
            },
        ];
        for track in [own].into_iter().chain(others) {
            for relation in 0..3 {
                let read = RelationColumn::read(&c, track, relation);
                assert!(
                    track != others[0] || read.is_err(),
                    "relation 0's column read against a track that keeps it the other way"
                );
                if let Ok(Some(r)) = read {
                    read_relation(&r);
                    if r.codec() == ww_tape::times::STARTS {
                        relation_in_order(&r);
                    }
                }
                if let Ok(Some(r)) = RelationColumn::salvage(&c, track, relation) {
                    read_relation(&r);
                }
            }
        }
        if let Ok(t) = EventTimes::read(&c) {
            read_all(&t);
            starts_in_order(&t);
        }
        if let Ok(t) = EventTimes::salvage(&c) {
            read_all(&t);
        }
    }
}

fn kernels(data: &[u8]) {
    let Some((&w, src)) = data.split_first() else {
        return;
    };
    let w = u32::from(w) % 65;
    let (base, slope) = (i64::from(w) << 50, (i64::from(w) << 33) - 7);
    let mut want = [0i64; GROUP];
    let width = bitpack::Width::new(w).expect("at most 64");
    width
        .decode_line_with(Kernel::Scalar, src, base, slope, &mut want)
        .expect("the scalar kernel runs everywhere");
    for k in [Kernel::Avx2, Kernel::Avx512, Kernel::Neon] {
        if k.is_available() {
            let mut o = [0i64; GROUP];
            width
                .decode_line_with(k, src, base, slope, &mut o)
                .expect("an available kernel");
            assert_eq!(o, want, "{k:?} width {w}");
        }
    }
}

pub fn times(data: &[u8]) {
    walk(data);
    kernels(data);
    // times from the input: [flip u32][mode u8][first tick u32][patch length u8][patch], then per item a delta u16,
    // a duration u8, a flag u8
    if data.len() < 10 {
        return;
    }
    let flip = u32::from_le_bytes(data[..4].try_into().unwrap()) as usize;
    let mode = data[4];
    let mut t = i64::from_le_bytes([data[5], data[6], data[7], data[8], 0, 0, 0, 0]) << (mode & 31);
    let Some((patch, items)) = data[10..].split_at_checked(usize::from(data[9] % 64)) else {
        return;
    };
    let (mut starts, mut ends, mut flags) = (vec![], vec![], vec![]);
    for c in items.as_chunks::<4>().0 {
        t = t.saturating_add(i64::from(u16::from_le_bytes([c[0], c[1]])) << (mode >> 5));
        starts.push(t);
        ends.push(t.saturating_add(i64::from(c[2])));
        flags.push(u32::from(c[3]) << (c[3] % 25));
    }
    if starts.is_empty() {
        return;
    }
    let enc = EncodedTimes::encode(&starts, Some(&ends), Some(&flags)).expect("valid times");
    // relation values from the same bytes: a source ordinal that never decreases, and keys in any order
    let mut src = u64::from(mode) << 56;
    let sources: Vec<u64> = items
        .as_chunks::<4>()
        .0
        .iter()
        .map(|c| {
            src = src.saturating_add(u64::from(c[2] % 3));
            src
        })
        .collect();
    let keys: Vec<u64> = items
        .as_chunks::<4>()
        .0
        .iter()
        .map(|c| u64::from_le_bytes([c[0], c[1], c[2], c[3], c[1], c[0], 0, c[3]]))
        .collect();
    let source = EncodedRelation::encode(0, &sources, true).expect("values that never decrease");
    let key = EncodedRelation::encode(2, &keys, false).expect("any values");
    let mut cols: Vec<_> = enc.columns().collect();
    cols.extend([source.column(), key.column()]);
    let specs: Vec<ColumnSpec> = cols.iter().map(|c| c.0).collect();
    let mut buf = vec![
        0u8;
        chunk_len(
            &enc.head.chunk_head(TrackId::new(1), TrackNum::new(1), 0),
            &specs,
            0
        )
    ];
    let len = write_chunk(
        &mut buf,
        ww_tape::BlockId::new(ww_tape::SessionTag::writable(1), 1),
        &enc.head.chunk_head(TrackId::new(1), TrackNum::new(1), 0),
        &cols,
        0,
    )
    .expect("a valid chunk");
    let block = Block::read(&buf[..len]).expect("written good");
    let chunk = block.chunk().expect("a chunk");
    let times = EventTimes::read(&chunk).expect("written times read");
    for i in 0..starts.len() {
        assert_eq!(
            (times.start(i), times.end(i), times.flag(i)),
            (starts[i], ends[i], flags[i])
        );
    }
    read_all(&times);
    for (relation, values) in [(0, &sources), (2, &keys)] {
        let track = StoredRelations {
            stored: RelationSet::from_bits(0b101),
            monotone: RelationSet::from_bits(0b001),
        };
        let r = RelationColumn::read(&chunk, track, relation)
            .expect("written relation reads")
            .expect("its column");
        assert_eq!((0..r.len()).map(|i| r.get(i)).collect::<Vec<_>>(), *values);
        let lo = values.iter().copied().min().expect("items");
        let hi = values.iter().copied().max().expect("items");
        assert_eq!(r.range(), (lo, hi));
        if let Some(at) = r.lower_bound(lo.saturating_add(1)) {
            assert_eq!(at, values.partition_point(|v| *v <= lo));
        }
        read_relation(&r);
    }
    let p = chunk.payload_offset();
    let at = p + flip % (len - 4 - p);
    let mut flipped = buf.clone();
    flipped[at] ^= 1 << (flip >> 29);
    walk(&flipped[..len]);
    // the patch over the payload, CRCs left as they were: group records that agree with each other but not the column
    let end = (at + patch.len()).min(len - 4);
    buf[at..end].copy_from_slice(&patch[..end - at]);
    walk(&buf[..len]);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A chunk of up to [ww_tape::times::ITEMS_MAX] items is read item by item at about 2 × [ITEMS_EACH] items,
    /// both ends whole; a smaller one whole.
    #[test]
    fn item_sample_is_bounded_and_keeps_both_ends() {
        for n in [
            0,
            1,
            ITEMS_EACH,
            ITEMS_EACH + 1,
            6_924_032,
            ww_tape::times::ITEMS_MAX,
        ] {
            let s: Vec<usize> = item_sample(n).collect();
            assert!(s.len() <= 2 * ITEMS_EACH + 1, "{n} items: {}", s.len());
            assert!(s.windows(2).all(|w| w[0] < w[1]), "{n} items: ascending");
            match n <= ITEMS_EACH {
                true => assert_eq!(s, (0..n).collect::<Vec<_>>()),
                false => {
                    assert_eq!(s[..ITEMS_EACH / 2], (0..ITEMS_EACH / 2).collect::<Vec<_>>());
                    assert_eq!(
                        s[s.len() - ITEMS_EACH / 2..],
                        (n - ITEMS_EACH / 2..n).collect::<Vec<_>>()
                    );
                }
            }
        }
    }
}
