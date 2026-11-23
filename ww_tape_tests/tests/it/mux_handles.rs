//! Typed track handles on a [TapeMux]: events and series encoded on the producers' threads, their summaries built
//! per piece on the writing thread, a quiet handle's chunk sealed by its latency (MUX-2).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use crate::mux_multi::pieces;
use exact_units::{Count, Magnitude};
use shrink_wrap::prelude::*;
use ww_tape::chunk::ChunkHead;
use ww_tape::index::{IndexEntry, IndexPolicy};
use ww_tape::payloads::{EncodedPayloads, PayloadCodec, Payloads};
use ww_tape::samples::Samples;
use ww_tape::times::{EncodedRelation, EncodedTimes, EventTimes, RelationColumn};
use ww_tape::{
    BlockKind, ChainOptions, ClockNum, ClockOrigin, ClockSpec, ColumnKind, CutPolicy, Definitions,
    Element, ElementDesc, Ends, EventsDef, ItemNum, Items, MemTapes, OwnerId, PieceChain,
    PieceTapes, Ratio64, RelationDef, RelationIndex, RelationValues, SeriesDef, SeriesGrid,
    SyncPolicy, TapeReader, TrackId, TrackKind, TrackNum, TrackRepeat, TrackSpec, WriterOptions,
    clock_key, track_key,
};
use ww_tape_mux::{Event, EventsOptions, MuxOptions, SeriesOptions, TapeMux};
use ww_tape_summary::{EventFields, EventTree, SeriesStats, SeriesSummarySpec, Summary};

const OWNER: OwnerId = OwnerId([0x4d; 32]);

/// A chain cut every `chunks` chunk blocks, with no sync and no checkpoint by time.
fn options(chunks: u64) -> ChainOptions {
    ChainOptions {
        cut: CutPolicy {
            chunks: Count::new(chunks),
            ..CutPolicy::default()
        },
        writer: WriterOptions {
            sync: SyncPolicy::None,
            index: IndexPolicy {
                interval: None,
                ..IndexPolicy::default()
            },
            ..WriterOptions::default()
        },
        tapes: PieceTapes::Each,
    }
}

pub(crate) fn clock() -> ClockSpec<'static> {
    ClockSpec {
        key: clock_key(0x4d, &[], 1),
        rate: Magnitude::pow10(9).into(),
        resolution: Magnitude::pow10(-9).into(),
        origin: ClockOrigin::Boot,
        label: "host",
    }
}

/// Relations of [events_track] `2`: `source` monotone with a range index, `context` without values, `key` any.
const RELATIONS: &[RelationDef<'static>] = &[
    RelationDef {
        name: "source",
        target: None,
        values: RelationValues::Stored,
        monotone: true,
        index: RelationIndex::Range,
    },
    RelationDef {
        name: "context",
        target: None,
        values: RelationValues::None,
        monotone: false,
        index: RelationIndex::None,
    },
    RelationDef {
        name: "key",
        target: None,
        values: RelationValues::Stored,
        monotone: false,
        index: RelationIndex::None,
    },
];

/// Events track `k`: 0 instants, 1 spans with flags, 2 spans with flags and [RELATIONS], 3 and up instants.
pub(crate) fn events_track(k: u32, clock: ClockNum) -> TrackSpec<'static> {
    let (def, relations) = match k {
        1 => (
            EventsDef {
                ends: Ends::Spans,
                flags: true,
            },
            &[][..],
        ),
        2 => (
            EventsDef {
                ends: Ends::Spans,
                flags: true,
            },
            RELATIONS,
        ),
        _ => (EventsDef::INSTANTS, &[][..]),
    };
    TrackSpec {
        key: track_key(0x4d, &[ww_tape::Segment::Index(k)]),
        clock,
        dimensions: RefVec::Slice { slice: &[] },
        items: Items::RawBytes,
        label: "events",
        repeat: TrackRepeat::Stream,
        kind: TrackKind::Events(def),
        relations: RefVec::Slice { slice: relations },
        owner: [0; 32],
    }
}

/// The summary spec of [series_track].
pub(crate) const SPEC: SeriesSummarySpec = SeriesSummarySpec {
    base_shift: 8,
    stats: SeriesStats {
        min: true,
        max: true,
        sum: true,
        sum_sq: true,
        clipped: false,
        flags: false,
        power_mean: false,
        power_max: false,
    },
    clip: None,
};

/// A series of i16 on two channels, a sample per 1000 ticks, summarised as [SPEC].
pub(crate) fn series_track(clock: ClockNum) -> TrackSpec<'static> {
    let mut def = SeriesDef::of::<i16>(2, Ratio64::int(1000).unwrap());
    def.summaries = Some(SPEC);
    TrackSpec {
        key: track_key(0x4d, &[ww_tape::Segment::Name("signal")]),
        clock,
        dimensions: RefVec::Slice { slice: &[] },
        items: Items::numbers::<i16>(None),
        label: "signal",
        repeat: TrackRepeat::Stream,
        kind: TrackKind::Series(def),
        relations: RefVec::Slice { slice: &[] },
        owner: [0; 32],
    }
}

/// The entries of `track` of kind `kind` in the only piece of `bytes`, in item order.
fn entries(r: &TapeReader<&[u8]>, track: TrackId, kind: BlockKind) -> Vec<IndexEntry> {
    let mut e: Vec<_> = r
        .track_entries(0, track)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == kind)
        .collect();
    e.sort_by_key(|e| (e.level, e.first_item));
    e
}

/// An item as the tests push it: start, end, flags, payload, relation values.
type Item = (i64, i64, u32, Vec<u8>, Vec<u64>);

/// Item `i` of events track `k`.
fn item(k: u32, i: u64) -> Item {
    let start = i as i64 * 100 + i64::from(k);
    let (end, flags) = match k {
        1 | 2 => (start + (i % 7) as i64 * 30, (i % 5) as u32),
        _ => (start, 0),
    };
    let payload = vec![k as u8; (i % 13) as usize];
    let values = if k == 2 { vec![i / 3, i % 11] } else { vec![] };
    (start, end, flags, payload, values)
}

/// The items of events track `id` read back from `pieces`, and per piece the items its level-0 summary nodes cover
/// against the items of its chunks.
fn read_events(pieces: &[Vec<u8>], def: &ww_tape::TrackDef<'_>) -> (Vec<Item>, Vec<(u64, u64)>) {
    let id = def.id();
    let mut items = Vec::new();
    let mut covered = Vec::new();
    let mut buf = Vec::new();
    for p in pieces {
        let r = TapeReader::open(&p[..]).unwrap();
        let mut in_chunks = 0;
        for e in entries(&r, id, BlockKind::Events) {
            let b = r.read_block(0, &e, &mut buf).unwrap();
            let c = b.chunk().unwrap();
            let times = EventTimes::read(&c).unwrap();
            let payloads = Payloads::read(&c).unwrap();
            let stored = ww_tape::times::StoredRelations::of(def);
            let relations: Vec<RelationColumn<'_>> = (0..3)
                .filter_map(|rel| RelationColumn::read(&c, stored, rel).unwrap())
                .collect();
            for i in 0..times.len() {
                items.push((
                    times.start(i),
                    times.end(i),
                    times.flag(i),
                    payloads
                        .as_ref()
                        .map_or(vec![], |p| p.get(i).unwrap().to_vec()),
                    relations.iter().map(|r| r.get(i)).collect(),
                ));
            }
            in_chunks += e.count;
        }
        let in_nodes: u64 = entries(&r, id, BlockKind::Summary)
            .iter()
            .filter(|e| e.level == 0)
            .map(|e| e.count)
            .sum();
        covered.push((in_nodes, in_chunks));
    }
    (items, covered)
}

/// Four events handles on their own threads: instants, spans with flags, relation values. Every item reads back as
/// pushed, across the cuts, and each piece's item trees cover the items of its chunks.
#[test]
fn events_handles_write_instants_spans_flags_and_relations() {
    const ITEMS: u64 = 20_000;
    let mem = MemTapes::new();
    let mux = TapeMux::create(
        mem.clone(),
        OWNER,
        0,
        &[],
        options(6),
        MuxOptions::default(),
    )
    .unwrap();
    let c = mux.define_clock(clock()).unwrap();
    let tracks: Vec<TrackNum> = (0..4)
        .map(|k| mux.define_track(events_track(k, c)).unwrap())
        .collect();
    let defs: Vec<_> = (0..4)
        .map(|k| events_track(k, c).numbered(tracks[k as usize]))
        .collect();
    thread::scope(|s| {
        for (k, track) in tracks.iter().enumerate() {
            let mut h = mux
                .events(
                    *track,
                    EventsOptions {
                        chunk_items: 700 + k as u32 * 300,
                        ..EventsOptions::default()
                    },
                )
                .unwrap();
            s.spawn(move || {
                for i in 0..ITEMS {
                    let (start, end, flags, payload, values) = item(k as u32, i);
                    h.push_related(start, end, flags, &payload, &values)
                        .unwrap();
                }
                h.finish().unwrap();
            });
        }
    });
    mux.close().unwrap();
    let pieces = pieces(&mem);
    assert!(pieces.len() > 4, "the chain is cut");
    for (k, def) in defs.iter().enumerate() {
        let (items, covered) = read_events(&pieces, def);
        let want: Vec<Item> = (0..ITEMS).map(|i| item(k as u32, i)).collect();
        assert!(items == want, "track {k} reads back as pushed");
        for (p, (nodes, chunks)) in covered.iter().enumerate() {
            assert_eq!(
                nodes, chunks,
                "track {k}, piece {p}: its tree covers its chunks"
            );
        }
    }
}

/// What an events handle refuses at the call: nothing of it is sent.
#[test]
fn an_events_handle_refuses_bad_items() {
    use ww_tape_mux::{CallError, ItemError, MuxError};
    let mux = TapeMux::create(
        MemTapes::new(),
        OWNER,
        0,
        &[],
        options(0),
        MuxOptions::default(),
    )
    .unwrap();
    let c = mux.define_clock(clock()).unwrap();
    let instants = mux.define_track(events_track(0, c)).unwrap();
    let related = mux.define_track(events_track(2, c)).unwrap();
    let series = mux.define_track(series_track(c)).unwrap();
    assert!(
        mux.events(series, EventsOptions::default()).is_err(),
        "a series track"
    );

    let why = |r: Result<_, MuxError<_>>| match r {
        Err(MuxError::Call(CallError::Item { why, .. })) => why,
        other => panic!("refused at the call: {other:?}"),
    };
    let mut h = mux.events(instants, EventsOptions::default()).unwrap();
    h.push(10, 10, 0, b"a").unwrap();
    assert_eq!(why(h.push(9, 9, 0, b"")), ItemError::Order);
    assert_eq!(why(h.push(11, 12, 0, b"")), ItemError::Span);
    assert_eq!(why(h.push(11, 11, 1, b"")), ItemError::Flags);
    assert_eq!(
        why(h.push_related(11, 11, 0, b"", &[1])),
        ItemError::Relations {
            values: 1,
            stored: 0
        }
    );
    assert_eq!(h.next_item(), ItemNum::new(1), "nothing refused was taken");

    let mut r = mux.events(related, EventsOptions::default()).unwrap();
    r.push_related(0, 5, 1, b"", &[7, 1]).unwrap();
    assert_eq!(
        why(r.push_related(1, 5, 1, b"", &[6, 1])),
        ItemError::RelationDecreases { relation: 0 }
    );
    drop((h, r));
    mux.close().unwrap();
}

/// The bytes the items of [item] track `k` write through a chain directly, with the item tree of each piece
/// ended before each cut: what one events handle writes through the mux.
fn direct_events(k: u32, items: u64, chunk_items: u64, codec: PayloadCodec) -> Vec<Vec<u8>> {
    let mem = MemTapes::new();
    let mut defs = Definitions::new();
    let c = defs.define_clock(clock()).unwrap();
    let track = defs.define_track(events_track(k, c)).unwrap();
    let id = events_track(k, c).numbered(track).id();
    let fields = EventFields {
        flags: k == 1 || k == 2,
        latest_end: k == 1 || k == 2,
    };
    let mut chain = PieceChain::create_on(mem.clone(), OWNER, 0, &[], options(5)).unwrap();
    let mut tree: Option<EventTree> = None;
    let mut first = 0;
    while first < items {
        let all: Vec<Item> = (first..(first + chunk_items).min(items))
            .map(|i| item(k, i))
            .collect();
        if chain.cut_due()
            && let Some(mut t) = tree.take()
        {
            t.finish().unwrap();
            for b in t.take_blocks() {
                chain.defer_block(&defs, b).unwrap();
            }
        }
        let starts: Vec<i64> = all.iter().map(|i| i.0).collect();
        let ends: Vec<i64> = all.iter().map(|i| i.1).collect();
        let flags: Vec<u32> = all.iter().map(|i| i.2).collect();
        let times = EncodedTimes::encode(
            &starts,
            fields.latest_end.then_some(&ends[..]),
            fields.flags.then_some(&flags[..]),
        )
        .unwrap();
        let payloads = EncodedPayloads::encode_with(codec, all.iter().map(|i| &i.3[..])).unwrap();
        let relations: Vec<EncodedRelation> = if k == 2 {
            [(0u16, true), (2, false)]
                .iter()
                .enumerate()
                .map(|(j, (rel, mono))| {
                    let v: Vec<u64> = all.iter().map(|i| i.4[j]).collect();
                    EncodedRelation::encode(*rel, &v, *mono).unwrap()
                })
                .collect()
        } else {
            vec![]
        };
        let head: ChunkHead = times.head.chunk_head(id, track, ItemNum::new(first));
        let columns: Vec<_> = times
            .columns()
            .chain(payloads.columns())
            .chain(relations.iter().map(EncodedRelation::column))
            .collect();
        chain.write_chunk(&defs, &head, &columns).unwrap();
        let t = tree.get_or_insert_with(|| EventTree::new(id, track, fields, ItemNum::new(first)));
        for i in &all {
            t.push(i.0, i.1, i.2).unwrap();
        }
        for b in t.take_blocks() {
            chain.defer_block(&defs, b).unwrap();
        }
        first += all.len() as u64;
    }
    if let Some(mut t) = tree.take() {
        t.finish().unwrap();
        for b in t.take_blocks() {
            chain.defer_block(&defs, b).unwrap();
        }
    }
    chain.close().unwrap();
    pieces(&mem)
}

/// One events handle writes the bytes the same chunks and item trees write through a chain directly, in each payload
/// codec: the encoding, the payload hash and the summary split change no byte.
#[test]
fn one_events_handle_writes_the_bytes_of_a_chain_written_directly() {
    for (k, codec) in [
        (0, PayloadCodec::Auto),
        (2, PayloadCodec::Auto),
        (2, PayloadCodec::Raw),
        (2, PayloadCodec::LZ4),
    ] {
        let mem = MemTapes::new();
        let mux = TapeMux::create(
            mem.clone(),
            OWNER,
            0,
            &[],
            options(5),
            MuxOptions::default(),
        )
        .unwrap();
        let c = mux.define_clock(clock()).unwrap();
        let track = mux.define_track(events_track(k, c)).unwrap();
        let mut h = mux
            .events(
                track,
                EventsOptions {
                    chunk_items: 1500,
                    latency: None,
                    payload_codec: codec,
                    ..EventsOptions::default()
                },
            )
            .unwrap();
        for i in 0..40_000 {
            let (start, end, flags, payload, values) = item(k, i);
            h.push_related(start, end, flags, &payload, &values)
                .unwrap();
        }
        h.finish().unwrap();
        mux.close().unwrap();
        let got = pieces(&mem);
        let want = direct_events(k, 40_000, 1500, codec);
        assert!(got.len() > 3, "the chain is cut");
        assert!(got == want, "track {k} {codec:?}: the same pieces");
        if codec == PayloadCodec::Raw {
            assert!(
                got != direct_events(k, 40_000, 1500, PayloadCodec::Auto),
                "raw payloads differ from the lz4 that auto takes"
            );
        }
    }
}

/// An events handle refuses a payload codec that is no payload codec of ww_tape.
#[test]
fn an_events_handle_refuses_an_unknown_payload_codec() {
    let mux = TapeMux::create(
        MemTapes::new(),
        OWNER,
        0,
        &[],
        options(5),
        MuxOptions::default(),
    )
    .unwrap();
    let c = mux.define_clock(clock()).unwrap();
    let track = mux.define_track(events_track(0, c)).unwrap();
    let options = EventsOptions {
        payload_codec: PayloadCodec::Named(ww_tape::Codec::SERIES),
        ..EventsOptions::default()
    };
    assert!(mux.events(track, options).is_err());
    mux.close().unwrap();
}

/// The pieces one events handle writes for the first `items` items of [item] track `k`, cut at `chunk_items` items
/// and `chunk_bytes` payload bytes: pushed one by one when `batches` is empty, else in batches of those lengths.
fn events_pieces(k: u32, items: u64, chunk_bytes: u32, batches: &[usize]) -> Vec<Vec<u8>> {
    let mem = MemTapes::new();
    let mux = TapeMux::create(
        mem.clone(),
        OWNER,
        0,
        &[],
        options(5),
        MuxOptions::default(),
    )
    .unwrap();
    let c = mux.define_clock(clock()).unwrap();
    let track = mux.define_track(events_track(k, c)).unwrap();
    let mut h = mux
        .events(
            track,
            EventsOptions {
                chunk_items: 1500,
                chunk_bytes: exact_units::Bytes::new(chunk_bytes),
                latency: None,
                ..EventsOptions::default()
            },
        )
        .unwrap();
    let all: Vec<Item> = (0..items).map(|i| item(k, i)).collect();
    if batches.is_empty() {
        for (start, end, flags, payload, values) in &all {
            h.push_related(*start, *end, *flags, payload, values)
                .unwrap();
        }
    } else {
        let mut at = 0;
        for n in batches.iter().cycle() {
            if at == all.len() {
                break;
            }
            let to = (at + n).min(all.len());
            let batch: Vec<Event<'_>> = all[at..to]
                .iter()
                .map(|(start, end, flags, payload, values)| Event {
                    start: *start,
                    end: *end,
                    flags: *flags,
                    payload,
                    values,
                })
                .collect();
            h.push_all(&batch).unwrap();
            at = to;
        }
    }
    assert_eq!(h.next_item(), ItemNum::new(items), "every item taken");
    h.finish().unwrap();
    mux.close().unwrap();
    pieces(&mem)
}

/// Items pushed in batches of any length, across chunks cut by items and by bytes, write the bytes of the same
/// items pushed one by one.
#[test]
fn events_pushed_in_batches_write_the_bytes_of_items_one_by_one() {
    for k in [0, 1, 2] {
        for chunk_bytes in [1 << 20, 4000] {
            let one = events_pieces(k, 30_000, chunk_bytes, &[]);
            assert!(one.len() > 3, "the chain is cut");
            for batches in [&[1, 2, 3][..], &[1499, 1501, 7], &[30_000], &[4096, 0, 333]] {
                let got = events_pieces(k, 30_000, chunk_bytes, batches);
                assert!(
                    got == one,
                    "track {k}, {chunk_bytes} bytes, batches {batches:?}: the same pieces"
                );
            }
        }
    }
}

/// A refused item in a batch refuses the rest: the items before it are taken, and the refusal names its number.
#[test]
fn a_batch_takes_the_items_before_a_refused_one() {
    use ww_tape_mux::{CallError, ItemError, MuxError};
    let mem = MemTapes::new();
    let mux = TapeMux::create(
        mem.clone(),
        OWNER,
        0,
        &[],
        options(0),
        MuxOptions::default(),
    )
    .unwrap();
    let c = mux.define_clock(clock()).unwrap();
    let track = mux.define_track(events_track(0, c)).unwrap();
    let def = events_track(0, c).numbered(track);
    let mut h = mux
        .events(
            track,
            EventsOptions {
                chunk_items: 4,
                ..EventsOptions::default()
            },
        )
        .unwrap();
    let at = |start| Event {
        start,
        end: start,
        payload: b"p",
        ..Event::default()
    };
    let r = h.push_all(&[at(1), at(2), at(3), at(4), at(5), at(4), at(6)]);
    match r {
        Err(MuxError::Call(CallError::Item { item, why })) => {
            assert_eq!(item, ItemNum::new(5), "the refused item's number");
            assert_eq!(why, ItemError::Order);
        }
        other => panic!("refused at the call: {other:?}"),
    }
    assert_eq!(h.next_item(), ItemNum::new(5), "the items before it taken");
    h.push_all(&[at(6)]).unwrap();
    h.finish().unwrap();

    let why = |r: Result<_, MuxError<_>>| match r {
        Err(MuxError::Call(CallError::Item { item, why })) => (item, why),
        other => panic!("refused at the call: {other:?}"),
    };
    // refused, not sealed for, when it would also pass the chunk's bytes
    let mut b = mux
        .events(
            mux.define_track(events_track(3, c)).unwrap(),
            EventsOptions {
                chunk_bytes: exact_units::Bytes::new(3),
                ..EventsOptions::default()
            },
        )
        .unwrap();
    assert_eq!(
        why(b.push_all(&[at(1), at(2), at(3), at(2)])),
        (ItemNum::new(3), ItemError::Order)
    );
    // a monotone relation's value decreasing inside the batch
    let mut r = mux
        .events(
            mux.define_track(events_track(2, c)).unwrap(),
            EventsOptions::default(),
        )
        .unwrap();
    let rel = |start, values: &'static [u64]| Event {
        start,
        end: start,
        values,
        ..Event::default()
    };
    assert_eq!(
        why(r.push_all(&[rel(1, &[1, 9]), rel(2, &[2, 0]), rel(3, &[1, 5])])),
        (
            ItemNum::new(2),
            ItemError::RelationDecreases { relation: 0 }
        )
    );
    assert_eq!(r.next_item(), ItemNum::new(2), "the items before it taken");
    drop((b, r));
    mux.close().unwrap();
    let (items, _) = read_events(&pieces(&mem), &def);
    let starts: Vec<i64> = items.iter().map(|i| i.0).collect();
    assert_eq!(starts, [1, 2, 3, 4, 5, 6], "what was taken reads back");
}

/// A series handle with summaries, its samples pushed in uneven batches and a new grid after a gap: every sample
/// reads back, each piece's level-0 summary nodes cover the samples of its chunks, and their sums are the samples'.
#[test]
fn a_series_handle_writes_samples_and_their_summaries() {
    const FRAMES: usize = 300_000;
    let mem = MemTapes::new();
    let mux = TapeMux::create(
        mem.clone(),
        OWNER,
        0,
        &[],
        options(4),
        MuxOptions::default(),
    )
    .unwrap();
    let c = mux.define_clock(clock()).unwrap();
    let track = mux.define_track(series_track(c)).unwrap();
    let id = series_track(c).numbered(track).id();
    let data: Vec<i16> = (0..FRAMES * 2)
        .map(|i| ((i as i64 * 7919) % 20_011 - 10_000) as i16)
        .collect();
    let mut h = mux
        .series::<i16>(
            track,
            SeriesOptions {
                chunk_samples: 10_000,
                ..SeriesOptions::default()
            },
        )
        .unwrap();
    let mut at = 0;
    let mut k = 0;
    let mut regridded = false;
    while at < FRAMES {
        let n = (1 + (k * 3_331) % 17_000).min(FRAMES - at);
        if at >= FRAMES / 2 && !regridded {
            regridded = true;
            // a gap of a second, the same scale
            let next = h.next_item().get();
            h.regrid(SeriesGrid::start(
                next,
                next as i64 * 1000 + 1_000_000_000,
                Magnitude::ONE,
            ))
            .unwrap();
        }
        h.push(&data[at * 2..(at + n) * 2]).unwrap();
        at += n;
        k += 1;
    }
    h.finish().unwrap();
    mux.close().unwrap();

    let pieces = pieces(&mem);
    assert!(pieces.len() > 4, "the chain is cut");
    let (mut samples, mut sums) = (Vec::new(), [0f64; 2]);
    let mut buf = Vec::new();
    for (p, bytes) in pieces.iter().enumerate() {
        let r = TapeReader::open(&bytes[..]).unwrap();
        let mut in_chunks = 0;
        for e in entries(&r, id, BlockKind::Series) {
            let b = r.read_block(0, &e, &mut buf).unwrap();
            let c = b.chunk().unwrap();
            let col = c.find(ColumnKind::Samples, 0).unwrap();
            let shape = ww_tape::SampleShape::new(ElementDesc::of(Element::I16), 2);
            let s = Samples::of(&col, shape, e.count as usize).unwrap();
            let mut out = vec![0i16; e.count as usize * 2];
            s.decode(0, &mut out).unwrap();
            samples.extend(out);
            in_chunks += e.count;
        }
        let mut in_nodes = 0;
        for e in entries(&r, id, BlockKind::Summary)
            .iter()
            .filter(|e| e.level == 0)
        {
            let b = r.read_block(0, e, &mut buf).unwrap();
            let s = Summary::of_block(&b).unwrap();
            for i in 0..s.node_count() {
                for (ch, sum) in sums.iter_mut().enumerate() {
                    *sum += s.channel_stats(ch as u16, i).unwrap().sum[0];
                }
            }
            in_nodes += e.count;
        }
        assert_eq!(
            in_nodes, in_chunks,
            "piece {p}: its summaries cover its chunks"
        );
    }
    assert!(samples == data, "every sample reads back");
    for (ch, sum) in sums.iter().enumerate() {
        let want: i64 = data.iter().skip(ch).step_by(2).map(|v| i64::from(*v)).sum();
        assert_eq!(
            *sum, want as f64,
            "channel {ch}: the nodes' sums are the samples'"
        );
    }
}

/// A quiet handle's item reaches the tape by its latency alone: the writing thread seals its open chunk, no call of
/// the producer's needed.
#[test]
fn a_quiet_handles_chunk_is_sealed_by_its_latency() {
    let mux = TapeMux::create(
        MemTapes::new(),
        OWNER,
        0,
        &[],
        options(0),
        MuxOptions::default(),
    )
    .unwrap();
    let c = mux.define_clock(clock()).unwrap();
    let track = mux.define_track(events_track(0, c)).unwrap();
    let mut h = mux
        .events(
            track,
            EventsOptions {
                latency: Some(Duration::from_millis(5)),
                ..EventsOptions::default()
            },
        )
        .unwrap();
    h.push(1, 1, 0, b"alone").unwrap();
    // waits for the writing thread to seal and place it, whatever the time it takes
    while mux.stats().placed_units == 0 {
        thread::yield_now();
    }
    let stats = mux.stats();
    assert_eq!(stats.latency_seal_count, 1);
    assert_eq!(h.next_item(), ItemNum::new(1));
    drop(h);
    mux.close().unwrap();
}

/// Eight producers, each pushing bursts on its own track and pausing with a chunk open until the writing thread
/// sealed one by its latency, while a reader reads every piece the chain has closed: no item is lost or out of
/// order, whichever side sealed each chunk.
#[test]
fn latency_seals_under_load_lose_nothing() {
    const PRODUCERS: u32 = 8;
    const ITEMS: u64 = 3_000;
    let mem = MemTapes::new();
    let mux = TapeMux::create(
        mem.clone(),
        OWNER,
        0,
        &[],
        options(8),
        MuxOptions::default(),
    )
    .unwrap();
    let c = mux.define_clock(clock()).unwrap();
    let tracks: Vec<TrackNum> = (0..PRODUCERS)
        .map(|k| mux.define_track(events_track(k + 3, c)).unwrap())
        .collect();
    let defs: Vec<_> = (0..PRODUCERS)
        .map(|k| events_track(k + 3, c).numbered(tracks[k as usize]))
        .collect();
    let ids: Vec<TrackId> = defs.iter().map(|d| d.id()).collect();
    let done = Arc::new(AtomicBool::new(false));
    thread::scope(|s| {
        let reader = {
            let (mem, done, ids) = (mem.clone(), done.clone(), ids.clone());
            s.spawn(move || {
                let mut read = 0;
                while !done.load(Ordering::Acquire) {
                    // the next piece, once its close is written: until then it opens as unclosed
                    let Some(tape) = mem.get(&read).unwrap() else {
                        thread::yield_now();
                        continue;
                    };
                    let p = tape.to_vec().unwrap();
                    let r = match TapeReader::open(&p[..]) {
                        Ok(r) if r.pieces().first().is_some_and(|p| p.close.is_some()) => r,
                        _ => {
                            thread::yield_now();
                            continue;
                        }
                    };
                    assert!(r.verify(0).unwrap().holds, "a closed piece checks");
                    for id in &ids {
                        let mut next = None;
                        for e in entries(&r, *id, BlockKind::Events) {
                            let first = e.first_item.get();
                            assert!(next.is_none_or(|n| n == first), "chunks follow each other");
                            next = Some(first + e.count);
                        }
                    }
                    read += 1;
                }
                read
            })
        };
        let producers: Vec<_> = tracks
            .iter()
            .enumerate()
            .map(|(k, track)| {
                let mut h = mux
                    .events(
                        *track,
                        EventsOptions {
                            chunk_items: 64,
                            latency: Some(Duration::from_micros(200)),
                            ..EventsOptions::default()
                        },
                    )
                    .unwrap();
                let mux = &mux;
                s.spawn(move || {
                    for i in 0..ITEMS {
                        let (start, end, flags, payload, values) = item(k as u32 + 3, i);
                        let sent = h
                            .push_related(start, end, flags, &payload, &values)
                            .unwrap();
                        // a burst, then a pause with the chunk open: until the writing thread seals a chunk by
                        // its latency, this one or another producer's, which holds items too
                        if sent.is_none() && (i + k as u64).is_multiple_of(37) {
                            let before = mux.stats().latency_seal_count;
                            while mux.stats().latency_seal_count == before {
                                thread::yield_now();
                            }
                        }
                    }
                    h.finish().unwrap();
                })
            })
            .collect();
        for p in producers {
            p.join().unwrap();
        }
        done.store(true, Ordering::Release);
        reader.join().unwrap();
    });
    let stats = mux.stats();
    mux.close().unwrap();
    assert!(
        stats.latency_seal_count > 0,
        "the writing thread sealed quiet chunks"
    );
    let pieces = pieces(&mem);
    for (k, def) in defs.iter().enumerate() {
        let (items, covered) = read_events(&pieces, def);
        let want: Vec<Item> = (0..ITEMS).map(|i| item(k as u32 + 3, i)).collect();
        assert!(items == want, "track {k} reads back as pushed");
        for (nodes, chunks) in covered {
            assert_eq!(
                nodes, chunks,
                "track {k}: each piece's tree covers its chunks"
            );
        }
    }
}
