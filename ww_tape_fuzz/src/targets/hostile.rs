//! The structure-aware targets `tape-<group>-hostile`: each builds a block or file with [crate::hostile], then runs
//! the decoders of its group, read and salvage paths, and goes through everything they hand out.

use super::{chunk, index, payloads, reader, samples, series, summary, times};
use crate::hostile;
use shrink_wrap::DeserializeShrinkWrap;
use ww_tape::times::EventTimes;
use ww_tape::{
    Block, ChunkHead, Describe, Element, ItemHash, Layout, Medium, MemSink, OpenOptions, Ratio64,
    SampleShape, Samples, SeriesGrid, SeriesTimeline, Slot, TapeReader, TapeSink, TapeWriter,
    TrackKind, block_len,
};

/// The blocks back to back at the start of `f`, up to the first that doesn't parse.
fn blocks(mut f: &[u8]) -> Vec<&[u8]> {
    let mut out = vec![];
    while let Ok(len) = block_len(f) {
        let Some((b, rest)) = f.split_at_checked(len) else {
            break;
        };
        out.push(b);
        f = rest;
    }
    out
}

/// Every record of a block read or salvaged, its coded body decoded, its describe records decoded.
fn records_of(b: &[u8]) {
    let Ok(block) = Block::salvage(b) else {
        return;
    };
    let _ = (
        block.body_codec(),
        block.clocks().count(),
        block.body_known(),
    );
    for r in block.records().flatten() {
        if r.at.is_none() {
            let _ = Describe::from_ww_bytes(r.item);
        }
    }
    let mut plain = Vec::new();
    if let Ok(decoded) = block.decode(&mut plain) {
        for r in decoded.records().flatten() {
            if r.at.is_none() {
                let _ = Describe::from_ww_bytes(r.item);
            }
        }
    }
}

/// `tape-records-hostile`: a records block, of data or of definitions, of hostile clock tables, record heads, lengths
/// and coded bodies, read alone and walked as a file.
pub fn records_hostile(data: &[u8]) {
    let b = hostile::records(data);
    records_of(&b);
    reader(&b);
}

/// Most items a target reads from one chunk: ww_tape's cap. A chunk that reads as more is a failure of the case, not a
/// loop of minutes.
const CHUNK_ITEMS_READ_MAX: usize = ww_tape::times::ITEMS_MAX;

/// The times of the chunk `b` read and salvaged hold at most [CHUNK_ITEMS_READ_MAX] items.
fn items_bounded(b: &[u8]) {
    let Ok(block) = Block::salvage(b) else { return };
    let Some(c) = block.chunk() else { return };
    for t in [EventTimes::read(&c), EventTimes::salvage(&c)]
        .into_iter()
        .flatten()
    {
        assert!(
            t.len() <= CHUNK_ITEMS_READ_MAX,
            "a chunk of {} bytes reads as {} items",
            b.len(),
            t.len()
        );
    }
}

/// `tape-chunk-hostile`: a chunk block of a hostile head, read, salvaged and renumbered.
pub fn chunk_hostile(data: &[u8]) {
    let b = hostile::chunk(data);
    items_bounded(&b);
    chunk::walk(&b);
    times::walk(&b);
    payloads::walk(&b, None);
}

/// `tape-times-hostile`: an events chunk of hostile time group records, its times read and salvaged.
pub fn times_hostile(data: &[u8]) {
    let b = hostile::times(data);
    items_bounded(&b);
    times::walk(&b);
}

/// `tape-payloads-hostile`: an events chunk of hostile payload lengths and lz4 lengths, its payloads read and
/// salvaged.
pub fn payloads_hostile(data: &[u8]) {
    let b = hostile::payloads(data);
    payloads::walk(&b, None);
}

/// The shape of the series track a block of definitions names; None when it names none.
fn series_def(b: &[u8]) -> Option<(SampleShape, Ratio64)> {
    let block = Block::read(b).ok()?;
    let r = block.records().flatten().next()?;
    match Describe::from_ww_bytes(r.item).ok()? {
        Describe::Track(t) => {
            let _ = (t.check_kind(), t.id(), t.dimensions_canonical());
            match t.kind {
                TrackKind::Series(s) => {
                    let _ = (s.check(), s.first_grid(0));
                    Some((s.shape, s.period))
                }
                _ => None,
            }
        }
        _ => None,
    }
}

/// Decode a samples column as `T` whole, from `from` on and group by group.
fn decode_column(
    column: &ww_tape::chunk::Column<'_>,
    shape: SampleShape,
    count: usize,
    from: usize,
) {
    macro_rules! go {
        ($t:ty) => {
            samples::read::<$t>(column.codec, shape, count, column.bytes, from)
        };
    }
    match shape.element.scalar() {
        Element::I8 => go!(i8),
        Element::I16 => go!(i16),
        Element::I32 => go!(i32),
        Element::I64 => go!(i64),
        Element::U8 => go!(u8),
        Element::U16 => go!(u16),
        Element::U32 => go!(u32),
        Element::U64 => go!(u64),
        Element::F32 => go!(f32),
        Element::F64 => go!(f64),
        Element::Bool => go!(bool),
        _ => {}
    }
    if let Ok(s) = Samples::of(column, shape, count) {
        let _ = s.sample_bytes(0..count.min(s.count()));
    }
}

/// `tape-samples-hostile`: a series track and a series chunk of a hostile sample column, its samples decoded as the
/// track's shape says, from the chunk's count.
pub fn samples_hostile(data: &[u8]) {
    let f = hostile::samples(data);
    let bs = blocks(&f);
    let shape = bs
        .first()
        .and_then(|b| series_def(b))
        .map_or(SampleShape::scalar(Element::I16), |(s, _)| s);
    for b in &bs {
        chunk::walk(b);
        let Ok(block) = Block::salvage(b) else {
            continue;
        };
        let Some(c) = block.chunk() else { continue };
        let count = c.head().count as usize;
        for col in c.columns() {
            let _ = c.column_damaged(&col);
            decode_column(&col, shape, count, data.len());
        }
    }
}

/// `tape-series-hostile`: a series track and its chunks of hostile grids, ticks and scales, their heads read alone
/// (the same as read from the whole block) and built into a timeline that is then asked about every stretch.
pub fn series_hostile(data: &[u8]) {
    let f = hostile::series(data);
    let bs = blocks(&f);
    let period = bs
        .first()
        .and_then(|b| series_def(b))
        .map_or(Ratio64::new(1, 1).expect("a ratio"), |(_, p)| p);
    let heads: Vec<_> = bs
        .iter()
        .skip(1)
        .filter_map(|b| {
            let head = ChunkHead::read_head(b).ok();
            if let Some(whole) = Block::read(b).ok().and_then(|b| b.chunk()) {
                assert_eq!(head, Some(whole.head()), "a good chunk's head reads alone");
            }
            head
        })
        .collect();
    for h in &heads {
        if let Ok(g) = SeriesGrid::of_chunk(h, period) {
            let _ = g.tick(period, h.first_item);
        }
    }
    let (t, _) = SeriesTimeline::from_chunks(period, &heads);
    series::check(&t);
    let count = data.len() as u32 % 300;
    let firsts: Vec<u64> = t.stretches().iter().map(|s| s.first_item).collect();
    for at in firsts {
        series::query(&t, at, count);
    }
    series::query(&t, u64::from(count), 1);
}

/// `tape-summary-hostile`: a summary block of a hostile head, scale or node values, every node and column read.
pub fn summary_hostile(data: &[u8]) {
    let b = hostile::summary(data);
    summary::walk(&b);
    let _ = Block::salvage(&b).map(|b| ww_tape_summary::Summary::of_block(&b).map(|s| s.scale()));
}

/// `tape-index-hostile`: an index, close, piece head or tail block of hostile offsets, directory or entries, read
/// alone, from its head and as a file.
pub fn index_hostile(data: &[u8]) {
    let b = hostile::index(data);
    index::decode(&b);
    index::read(&b, OpenOptions::default());
}

/// `tape-reader-hostile`: a piece of hostile offsets, lengths, layout or stretches between blocks, opened by the
/// positional reader as a file and as a raw medium, every piece's entries, blocks, state entries and definitions
/// read, and walked block by block.
pub fn reader_hostile(data: &[u8]) {
    let f = hostile::file(data);
    let raw = OpenOptions {
        medium: Medium::Raw,
        layout: Some(Layout::UNALIGNED.on_sd_card()),
        ..OpenOptions::default()
    };
    for options in [OpenOptions::default(), raw] {
        index::read(&f, options);
        if let Ok(r) = TapeReader::open_with(&f[..], options) {
            for i in 0..r.pieces().len() {
                let _ = r.definitions_at(i, u64::MAX);
                let _ = r.state_entries(i);
            }
        }
    }
    reader(&f);
}

/// `tape-state-hostile`: a piece of puts, copies and definitions whose blocks of state records and index state
/// entries lie behind good CRCs, or which is cut. Every lookup of the reader hands out only a record that is the one
/// its entry names (its hash, its slot) and lies before the place asked; a resume of it takes new definitions, puts
/// and blocks, and closes whole. The piece is read as a file, as a raw medium, with a small cache and small
/// read budgets.
pub fn state_hostile(data: &[u8]) {
    let a = hostile::state_attacked(data);
    let f = a.bytes;
    for options in state_readers() {
        if let Ok(r) = TapeReader::open_with(&f[..], options) {
            for i in 0..r.pieces().len() {
                lookups(&r, i);
            }
            if !a.forged {
                model(&r, f.len() as u64);
            }
        }
    }
    resume_state(&f, a.forged);
}

/// The readers of a state piece: a file; a raw medium, which reads past damage as gaps; a state cache too small for
/// the piece, which sorts its entries again for each lookup; read budgets that run out in a lookup or in its first
/// block.
fn state_readers() -> [OpenOptions; 5] {
    let file = OpenOptions::default();
    [
        file,
        OpenOptions {
            medium: Medium::Raw,
            layout: Some(hostile::state_options().layout),
            ..file
        },
        OpenOptions {
            state_cache_max: 1,
            ..file
        },
        OpenOptions {
            state_read_max: 2000,
            ..file
        },
        OpenOptions {
            state_read_max: 60,
            ..file
        },
    ]
}

/// The model of a piece damaged where a CRC finds it, or cut, nothing forged behind a good CRC: a lookup that
/// doesn't hand out what the template holds there names why, a block it lost, a stretch the index doesn't cover or
/// a budget spent. Each slot's record in force, every definition and each item by its hash are asked for. The
/// template's records past a cut aren't asked for at the piece's end.
fn model(r: &TapeReader<&[u8]>, len: u64) {
    let Some(p) = r
        .pieces()
        .first()
        .filter(|p| p.start == 0 && p.version.is_none())
    else {
        return;
    };
    let truth = template_reader();
    let Ok(entries) = truth.state_entries(0) else {
        return;
    };
    let mut slots: Vec<Slot> = entries.iter().map(|(_, s)| s.slot).collect();
    slots.sort();
    slots.dedup();
    // the end of the piece found: where the bytes stop, unless the piece reads past a cut
    let end = p.end.min(len);
    let places = entries
        .iter()
        .map(|(e, _)| e.offset.saturating_add(1))
        .chain([u64::MAX]);
    for place in places.step_by(3) {
        let held = if place == u64::MAX { end } else { place };
        for slot in &slots {
            let want = truth
                .state_at(0, *slot, held)
                .expect("the template's lookup")
                .item;
            let Ok(got) = r.state_at(0, *slot, place) else {
                continue;
            };
            if got.item.as_ref().map(|s| (s.offset, &s.item))
                != want.as_ref().map(|s| (s.offset, &s.item))
            {
                assert!(
                    !got.is_whole(),
                    "{slot:?} at {place}: {:?} instead of {:?}, nothing named",
                    got.item.map(|s| s.offset),
                    want.map(|s| s.offset)
                );
            }
        }
        let want = truth
            .definitions_at(0, held)
            .expect("the template's lookup");
        if let Ok(got) = r.definitions_at(0, place)
            && placed(&got.items) != placed(&want.items)
        {
            assert!(
                !got.is_whole(),
                "definitions at {place}: {:?} instead of {:?}, nothing named",
                placed(&got.items),
                placed(&want.items)
            );
        }
    }
    // each item by its hash: the newest of the template's records of it in the piece found
    let mut hashes: Vec<ItemHash> = entries.iter().map(|(_, s)| s.hash).collect();
    hashes.sort();
    hashes.dedup();
    for hash in hashes {
        let want = entries
            .iter()
            .filter(|(e, s)| s.hash == hash && e.offset < end)
            .map(|(e, _)| e.offset)
            .max();
        let Ok(got) = r.item_by_hash(0, hash) else {
            continue;
        };
        if got.item.as_ref().map(|s| s.offset) != want {
            assert!(
                !got.is_whole(),
                "item {hash:?}: at {:?} instead of {want:?}, nothing named",
                got.item.map(|s| s.offset)
            );
        }
    }
}

/// The template opened once: its sorted state entries stay in its cache across runs.
fn template_reader() -> &'static TapeReader<&'static [u8]> {
    static T: std::sync::OnceLock<TapeReader<&'static [u8]>> = std::sync::OnceLock::new();
    T.get_or_init(|| TapeReader::open(hostile::state_template()).expect("the template opens"))
}

/// Where each definition found lies, by slot.
fn placed(
    items: &std::collections::BTreeMap<Slot, ww_tape::reader::StateItem>,
) -> Vec<(Slot, u64)> {
    items.iter().map(|(slot, s)| (*slot, s.offset)).collect()
}

/// The lookups of piece `i` at its first, middle and last state entry and at its end: each slot's record in force,
/// each hash's record, every definition.
fn lookups(r: &TapeReader<&[u8]>, i: usize) {
    let Ok(entries) = r.state_entries(i) else {
        return;
    };
    let mut slots: Vec<Slot> = entries.iter().map(|(_, s)| s.slot).collect();
    slots.sort();
    slots.dedup();
    let mut hashes: Vec<ItemHash> = entries.iter().map(|(_, s)| s.hash).collect();
    hashes.sort();
    hashes.dedup();
    let n = entries.len();
    let places = [0, n / 2, n.saturating_sub(1)]
        .into_iter()
        .filter_map(|k| entries.get(k).map(|(e, _)| e.offset.saturating_add(1)))
        .chain([u64::MAX]);
    for place in places {
        for slot in &slots {
            if let Ok(found) = r.state_at(i, *slot, place)
                && let Some(item) = &found.item
            {
                assert_eq!(item.slot, *slot, "the slot asked");
                assert_eq!(
                    ItemHash::of(&item.item),
                    item.hash,
                    "the item its entry names"
                );
                assert!(item.offset < place, "before the place asked");
            }
        }
        if let Ok(defs) = r.definitions_at(i, place) {
            for (slot, item) in &defs.items {
                assert!(slot.is_definition(), "a definition's slot");
                assert_eq!(item.slot, *slot);
                assert_eq!(ItemHash::of(&item.item), item.hash);
                assert!(item.offset < place);
            }
        }
    }
    for hash in hashes {
        if let Ok(found) = r.item_by_hash(i, hash)
            && let Some(item) = &found.item
        {
            assert_eq!(ItemHash::of(&item.item), hash, "the hash asked");
        }
    }
}

/// A resume of `f`: when it resumes, the definitions again, a put on each state track and a records block, then a
/// close that a reader finds whole. Unless the input was `forged` (a block behind a good CRC that isn't the one its
/// index entry states, which a resume keeps the entry of), the piece verifies.
fn resume_state(f: &[u8], forged: bool) {
    let mut sink = MemSink::new();
    sink.write_all(f).expect("memory takes the bytes");
    let tape = sink.tape();
    let Ok((mut w, _)) = TapeWriter::resume_on(sink, hostile::state_options()) else {
        return;
    };
    let defs = hostile::state_definitions();
    for t in hostile::STATES {
        w.put(&defs, t, 1000, b"resumed")
            .expect("a resumed piece takes puts");
    }
    w.write_block(&defs, &hostile::state_records(w.next_id(), 1000))
        .expect("a resumed piece takes blocks");
    w.close().expect("a resumed piece closes");
    let out = tape.to_vec().expect("the bytes");
    let r = TapeReader::open(&out[..]).expect("a closed piece opens");
    let last = r.pieces().len().checked_sub(1).expect("a piece");
    assert!(
        r.verify(last).expect("a closed piece") || forged,
        "the piece verifies"
    );
    let found = r
        .state_at(
            last,
            Slot::State {
                track: hostile::STATES[0],
            },
            u64::MAX,
        )
        .expect("a lookup");
    assert_eq!(found.item.map(|s| s.item), Some(b"resumed".to_vec()));
}
