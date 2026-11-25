//! The structure-aware targets `tape-<group>-hostile`: each builds a block or file with [crate::hostile], then runs
//! the decoders of its group, read and salvage paths, and goes through everything they hand out.

use super::{chunk, flags, index, payloads, reader, samples, series, summary, times};
use crate::hostile;
use shrink_wrap::DeserializeShrinkWrap;
use ww_tape::samples::Scalar;
use ww_tape::times::EventTimes;
use ww_tape::{
    Block, ChunkHead, Describe, Element, ElementDesc, ItemHash, Layout, Medium, MemSink,
    OpenOptions, Ratio64, SampleShape, Samples, SeriesGrid, SeriesTimeline, Slot, TapeReader,
    TapeSink, TapeWriter, TrackKind, block_len,
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
            let _ = (
                t.check_kind(),
                t.id(),
                t.dimensions_canonical(),
                t.items_valid(),
            );
            match t.kind {
                TrackKind::Series(ref s) => {
                    let _ = (s.check(), s.first_grid(0));
                    Some((t.series_shape()?, s.period))
                }
                _ => None,
            }
        }
        _ => None,
    }
}

/// Decode a samples column as `T` whole, from `from` on and group by group.
pub(crate) fn decode_column(
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
        Scalar::I8 => go!(i8),
        Scalar::I16 => go!(i16),
        Scalar::I32 => go!(i32),
        Scalar::I64 => go!(i64),
        Scalar::U8 => go!(u8),
        Scalar::U16 => go!(u16),
        Scalar::U32 => go!(u32),
        Scalar::U64 => go!(u64),
        Scalar::F32 => go!(f32),
        Scalar::F64 => go!(f64),
        Scalar::Bool => go!(bool),
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
    let shape = bs.first().and_then(|b| series_def(b)).map_or(
        SampleShape::scalar(ElementDesc::of(Element::I16)),
        |(s, _)| s,
    );
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

/// `tape-flags-hostile`: a series track and a series chunk of hostile flag runs, read and salvaged, the chunk's
/// samples pushed with them into summaries, the whole read as a file.
pub fn flags_hostile(data: &[u8]) {
    let f = hostile::flags(data);
    let bs = blocks(&f);
    let shape = bs.first().and_then(|b| series_def(b)).map(|(s, _)| s);
    for b in bs.iter().skip(1) {
        chunk::walk(b);
        flags::walk(b, shape);
    }
    reader(&f);
}

/// `tape-series-hostile`: a series track and its chunks of hostile grids, ticks, scales and grid start marks.
///
/// Their heads are read alone (the same as read from the whole block) and built into a timeline that is then asked
/// about every stretch. The file's timeline from its entries ([TapeReader::series_timeline], reading only the heads
/// it must) holds up the same way.
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
    let firsts: Vec<u64> = t.stretches().iter().map(|s| s.first_item.get()).collect();
    for at in firsts {
        series::query(&t, at, count);
    }
    series::query(&t, u64::from(count), 1);
    if let Ok(r) = TapeReader::open(&f[..]) {
        for i in 0..r.pieces().len() {
            if let Ok((t, _)) = r.series_timeline(i, ww_tape::TrackId::new(7), period) {
                series::check(&t);
                series::query(&t, u64::from(count), 1);
            }
        }
    }
}

/// A grid track a block of definitions names: its id, tile layout and the shape of a cell.
pub(crate) struct GridOf {
    pub id: ww_tape::TrackId,
    pub layout: ww_tape::TileLayout,
    pub shape: Option<SampleShape>,
}

impl GridOf {
    /// The box of every cell of the grid at overview level `level`; None past the grid's levels.
    pub fn whole(&self, level: u8) -> Option<ww_tape::TileBox> {
        let at = self.layout.overview(level)?;
        let cells: Vec<_> = at.extents().iter().map(|(e, _)| 0..*e).collect();
        at.tile_box(&cells)
    }
}

/// The grid track a block of definitions names; None when it names none. A grid that decodes has a layout: its
/// axes were checked on decode.
pub(crate) fn grid_def(b: &[u8]) -> Option<GridOf> {
    let block = Block::read(b).ok()?;
    let r = block.records().flatten().nth(1)?;
    let Describe::Track(t) = Describe::from_ww_bytes(r.item).ok()? else {
        return None;
    };
    let TrackKind::Series(s) = &t.kind else {
        return None;
    };
    let l = s.tile_layout().expect("a decoded grid's axes were checked");
    let extents: Vec<u32> = s
        .axes
        .iter()
        .map(|a| a.map(|a| a.extent.0))
        .collect::<Result<_, _>>()
        .expect("a decoded grid's axes read");
    assert!(
        l.extents().iter().map(|(e, _)| *e).eq(extents),
        "the layout has the definition's extents"
    );
    Some(GridOf {
        id: t.id(),
        layout: l,
        shape: t.series_shape(),
    })
}

/// `tape-grid-hostile`: a grid track of hostile axes and grid chunks of hostile tile coordinates.
///
/// Each head reads alone as from the whole block; coordinates the grid holds pack to a tile number that unpacks to
/// them and lies in the box of the whole grid, others pack to none. Read as a tape, the box of the whole grid finds
/// only tiles the grid holds: from the tile numbers of a checkpoint's entries, packed or hostile, or from the chunk
/// heads where an entry holds none or one the grid holds no tile of. Its tile summary blocks, hostile tiles
/// included, are found and merged as `tape-grid` checks them.
pub fn grid_hostile(data: &[u8]) {
    let f = hostile::grid(data);
    let bs = blocks(&f);
    let def = bs.first().and_then(|b| grid_def(b));
    let whole = def.as_ref().and_then(|g| g.whole(0));
    for b in bs.iter().skip(1) {
        chunk::walk(b);
        let Ok(h) = ChunkHead::read_head(b) else {
            continue;
        };
        assert!(
            h.tile.len() <= ww_tape::grid::GRID_FIXED_AXES_MAX,
            "a head reads at most GRID_FIXED_AXES_MAX coordinates"
        );
        assert!(
            h.level <= ww_tape::grid::GRID_OVERVIEW_LEVELS_MAX,
            "a head reads at most GRID_OVERVIEW_LEVELS_MAX"
        );
        if let (Some(g), Some(whole)) = (&def, &whole) {
            let l = &g.layout;
            if let Some(n) = l.pack(&h.tile) {
                assert_eq!(l.unpack(n), Some(h.tile));
                assert!(whole.holds(n), "the whole grid holds the tile number");
                assert!(whole.holds_coords(&h.tile), "the whole grid holds the tile");
            } else {
                assert!(
                    !l.holds(&h.tile),
                    "a tile that doesn't pack isn't the grid's"
                );
                assert!(!whole.holds_coords(&h.tile), "nor in the whole grid's box");
            }
            if let (Ok(blk), Some(shape)) = (Block::salvage(b), g.shape)
                && let Some(c) = blk.chunk()
            {
                super::grid::overview(&c, l, shape);
            }
        }
    }
    if let Some(g) = &def
        && let Ok(r) = TapeReader::open(&f[..])
        && !r.pieces().is_empty()
    {
        super::grid::chunks(&r, g);
        if let Some(whole) = &whole {
            super::grid::summaries(&r, g.id, &g.layout, whole);
        }
    }
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

/// `tape-reader-hostile`: a piece of hostile offsets, lengths, layout or stretches between blocks.
///
/// The positional reader opens it as a file and as a raw medium, reads every piece's entries, blocks, state entries
/// and definitions, and walks it block by block.
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
                if let Ok(defs) = r.definitions_at(i, u64::MAX) {
                    named_within_marked(&r, i, &defs);
                }
                let _ = r.state_entries(i);
            }
        }
    }
    reader(&f);
}

/// The tracks `defs` (of piece `i` of `r`) names as not carried are head rows marked so, each once: the reader never
/// names a track the head doesn't mark. Which marked rows it names, the writer's tests check against the writer.
fn named_within_marked(r: &TapeReader<&[u8]>, i: usize, defs: &ww_tape::reader::DefinitionsAt) {
    let marked: Vec<_> = r
        .pieces()
        .get(i)
        .and_then(|p| p.head.as_ref())
        .map(|(_, rows)| rows.iter().filter(|t| t.not_carried).copied().collect())
        .unwrap_or_default();
    for (k, t) in defs.not_carried.iter().enumerate() {
        assert!(marked.contains(t), "a named track is a marked head row");
        assert!(!defs.not_carried[..k].contains(t), "a track is named once");
    }
}

/// `tape-state-hostile`: a piece of puts, copies and definitions whose blocks of state records and index state entries
/// lie behind good CRCs, or which is cut.
///
/// Every lookup of the reader hands out only a record that is the one its entry names (its hash, its slot) and lies
/// before the place asked; a resume of it takes new definitions, puts and blocks, and closes whole. The piece is read
/// as a file, as a raw medium, with a small cache and small read budgets.
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
            state_cache_max: exact_units::Count::new(1),
            ..file
        },
        OpenOptions {
            state_read_max: exact_units::Bytes::new(2000),
            ..file
        },
        OpenOptions {
            state_read_max: exact_units::Bytes::new(60),
            ..file
        },
    ]
}

/// The model of a piece damaged where a CRC finds it, or cut, nothing forged behind a good CRC: a lookup that
/// doesn't hand out what the template holds there names why, a block it lost, a stretch the index doesn't cover or
/// a budget spent. Each slot's record in force, every definition and each item by its hash are asked for. The
/// template's records past a cut, or in a run of puts the cut tore, aren't asked for at the piece's end.
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
    // a run of puts a cut tore at the piece's end: no block after its start is listed, and none of it is in force
    let last = r
        .entries(0)
        .ok()
        .and_then(|l| l.iter().map(|e| e.offset).max());
    let torn = p
        .dropped_runs
        .iter()
        .map(|(at, _)| *at)
        .filter(|at| last.is_none_or(|l| l < *at))
        .min()
        .unwrap_or(u64::MAX);
    // the end of the piece found: where the bytes stop, unless the piece reads past a cut
    let end = p.end.min(len).min(torn);
    let places = entries
        .iter()
        .map(|(e, _)| e.offset.saturating_add(1))
        .chain([u64::MAX]);
    for place in places.step_by(3) {
        let held = if place == u64::MAX {
            end
        } else {
            place.min(torn)
        };
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
                assert!(item.offset < place, "an item starts before its place");
            }
            named_within_marked(r, i, &defs);
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
/// index entry states, which a resume keeps the entry of), the piece verifies; written on after a close, it verifies
/// exactly when the input verified up to that close, damage before it staying damage.
fn resume_state(f: &[u8], forged: bool) {
    let before = TapeReader::open(f)
        .ok()
        .and_then(|r| r.verify(0).map(|v| v.holds).ok())
        .unwrap_or(false);
    let mut sink = MemSink::new();
    sink.write_all(f).expect("memory takes the bytes");
    let tape = sink.tape();
    let Ok((mut w, resumed)) = TapeWriter::resume_on(sink, hostile::state_options()) else {
        return;
    };
    let (defs, on) = hostile::state_definitions(Some(&resumed));
    for t in on.states {
        w.put(&defs, t, 1000, b"resumed")
            .expect("a resumed piece takes puts");
    }
    w.write_block(&defs, &hostile::state_records(w.next_id(), on, 1000))
        .expect("a resumed piece takes blocks");
    w.close().expect("a resumed piece closes");
    let out = tape.to_vec().expect("the bytes");
    let r = TapeReader::open(&out[..]).expect("a closed piece opens");
    let last = r.pieces().len().checked_sub(1).expect("a piece");
    let holds = r.verify(last).map(|v| v.holds).expect("a closed piece");
    match resumed.after_close {
        None => assert!(holds || forged, "the piece verifies"),
        Some(_) => assert!(
            holds == before || forged,
            "written on after a close, the piece verifies as it did up to it"
        ),
    }
    let found = r
        .state_at(
            last,
            Slot::State {
                track: on.states[0],
            },
            u64::MAX,
        )
        .expect("a lookup");
    assert_eq!(found.item.map(|s| s.item), Some(b"resumed".to_vec()));
}
