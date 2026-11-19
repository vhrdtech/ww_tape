use exact_units::prefix::{Micro, Milli, One};
use exact_units::units::{Ampere, Volt};
use exact_units::{Complex, Fixed};
use shrink_wrap::prelude::*;

use super::*;
use crate::summary::{SeriesStats, SeriesSummarySpec};
use crate::{
    BlockKind, ChunkHead, Describe, DescribeError, Dimension, EndMode, Ends, EventsDef,
    FullVersion, TrackDef, TrackKind, Version,
};

/// 48 kHz on a 1 ns clock.
fn period() -> Ratio64 {
    Ratio64::new(62_500, 3).unwrap()
}

fn milli() -> Magnitude {
    Magnitude::pow10(-3)
}

fn micro() -> Magnitude {
    Magnitude::pow10(-6)
}

fn sample_type() -> TypeRef<'static> {
    TypeRef {
        bundle: RefVec::Slice { slice: &[7, 7] },
        krate: FullVersion::new("motor_api", Version::new(0, 3, 0)),
        path: "PhaseCurrent",
    }
}

fn def() -> SeriesDef<'static> {
    SeriesDef {
        summaries: Some(SeriesSummarySpec {
            base_shift: 6,
            stats: SeriesStats::all(Element::I32),
            clip: None,
        }),
        media: Some(MediaStream {
            media: RefVec::Slice { slice: &[1, 2, 3] },
            stream: UNib32(1),
        }),
        ..SeriesDef::of::<Fixed<Ampere, Milli, i32>>(Some(sample_type()), 3, period())
    }
}

fn encode<T: SerializeShrinkWrap>(v: &T) -> std::vec::Vec<u8> {
    let mut buf = [0u8; 512];
    v.to_ww_bytes(&mut buf).unwrap().to_vec()
}

#[test]
fn def_takes_element_and_scale_from_the_type() {
    let d = def();
    assert_eq!(d.shape, SampleShape::new(Element::I32, 3));
    assert_eq!(d.scale, milli());
    assert_eq!(d.check(), Ok(()));
    let bytes = encode(&d);
    assert_eq!(SeriesDef::from_ww_bytes(&bytes).unwrap(), d);

    assert_eq!(<Fixed<Volt, Micro, i16>>::ELEMENT, Element::I16);
    assert_eq!(<Fixed<Volt, Micro, i16>>::SCALE, micro());
    assert_eq!(<Complex<Volt, One, i16>>::ELEMENT, Element::ComplexI16);
    assert_eq!(<Complex<Volt, One, f32>>::ELEMENT, Element::ComplexF32);
    assert_eq!(<f32 as SeriesValue>::SCALE, Magnitude::ONE);
    assert_eq!(<bool as SeriesValue>::ELEMENT, Element::Bool);

    let mut out = [0i16; 2];
    assert_eq!(
        Complex::<Volt, One, i16>::new(3, -4).write_parts(&mut out),
        2
    );
    assert_eq!(out, [3, -4]);
    assert_eq!(
        Complex::<Volt, One, i16>::new(5, 6).write_parts(&mut out[..1]),
        1
    );
    assert_eq!(out, [5, -4]);
    let mut one = [0i32; 1];
    assert_eq!(
        Fixed::<Ampere, Milli, i32>::new(-7).write_parts(&mut one),
        1
    );
    assert_eq!(one, [-7]);
}

#[test]
fn bad_defs_are_refused_on_decode() {
    let bad = [
        SeriesDef {
            shape: SampleShape::new(Element::I32, 0),
            ..def()
        },
        SeriesDef {
            shape: SampleShape::new(Element::I32, SERIES_CHANNELS_MAX + 1),
            ..def()
        },
        SeriesDef {
            shape: SampleShape::new(Element::Unknown(40), 1),
            summaries: None,
            ..def()
        },
        SeriesDef {
            summaries: Some(SeriesSummarySpec {
                base_shift: 2,
                stats: SeriesStats::all(Element::I32),
                clip: None,
            }),
            ..def()
        },
        SeriesDef {
            summaries: Some(SeriesSummarySpec {
                base_shift: 6,
                stats: SeriesStats::default(),
                clip: None,
            }),
            ..def()
        },
        SeriesDef {
            summaries: Some(SeriesSummarySpec {
                base_shift: 6,
                stats: SeriesStats::all(Element::ComplexI16),
                clip: None,
            }),
            ..def()
        },
    ];
    for d in bad {
        assert!(d.check().is_err(), "{d:?}");
        let bytes = encode(&d);
        assert!(SeriesDef::from_ww_bytes(&bytes).is_err(), "{d:?}");
    }
}

fn track(kind: TrackKind<'static>, state: bool) -> TrackDef<'static> {
    TrackDef {
        number: UNib32(2),
        key: 0x5e71e5,
        clock: UNib32(1),
        dimensions: RefVec::Slice {
            slice: &[Dimension {
                name: "phase",
                value: "a",
            }],
        },
        api: FullVersion::new("motor_api", Version::new(0, 3, 0)),
        bundle: RefVec::Slice { slice: &[7, 7] },
        path: RefVec::Slice {
            slice: &[UNib32(4), UNib32(1)],
        },
        keys: RefVec::Slice { slice: &[] },
        item: None,
        label: "phase[a].current",
        repeat: if state {
            crate::TrackRepeat::STATE
        } else {
            crate::TrackRepeat::Stream
        },
        kind,
        relations: RefVec::Slice { slice: &[] },
        owner: [0; 32],
    }
}

#[test]
fn track_kind_round_trips_and_is_checked() {
    for t in [
        track(TrackKind::Series(def()), false),
        track(
            TrackKind::Events(EventsDef {
                ends: Ends::Spans,
                flags: true,
            }),
            false,
        ),
    ] {
        let d = Describe::Track(t.clone());
        let bytes = encode(&d);
        assert_eq!(Describe::from_ww_bytes(&bytes).unwrap(), d);
        assert_eq!(t.check_kind(), Ok(()));
    }
    let t = track(TrackKind::Series(def()), false);
    assert_eq!(t.series(), Some(&def()));
    assert!(Ends::Instants.allows(EndMode::Absent));
    assert!(!Ends::Instants.allows(EndMode::Durations));
    assert!(Ends::Spans.allows(EndMode::NextStart));

    // a series' chunks are data: the state policy is refused
    let state = track(TrackKind::Series(def()), true);
    let with_item = TrackDef {
        item: Some(sample_type()),
        ..t.clone()
    };
    for t in [state, with_item] {
        assert!(matches!(
            crate::open::check_definition(&Describe::Track(t)),
            Err(DescribeError::Kind { track: 2, .. })
        ));
    }
}

/// A series chunk head of `count` samples from `first` on `grid`, its ticks the grid's.
fn head_on(grid: SeriesGrid, first: u64, count: u32) -> ChunkHead {
    let (first_tick, last_tick) = grid.chunk_ticks(period(), first, count).unwrap();
    ChunkHead {
        kind: BlockKind::Series,
        track_id: 1,
        track_number: 2,
        first_item: first,
        count,
        first_tick,
        last_tick,
        latest_end: last_tick,
        time_mode: 0,
        end_mode: EndMode::Absent,
        grid: Some(grid),
    }
}

#[test]
fn rescaled_grid_keeps_sample_ticks() {
    let p = period();
    let a = SeriesGrid::start(0, 1_000, micro());
    let b = a.rescaled(milli());
    assert!(a.same_grid(&b));
    let mut t = SeriesTimeline::new(p);
    t.push_chunk(&head_on(a, 0, 301)).unwrap();
    t.push_chunk(&head_on(b, 301, 100)).unwrap();
    // 301 · 62500/3 = 6270833.3..: the same tick on both chunks
    assert_eq!(a.tick(p, 301), Some(1_000 + 6_270_833));
    assert_eq!(t.tick(301), a.tick(p, 301));
    assert_eq!(t.tick(302), Some(1_000 + 6_291_666));
    assert_eq!(t.scale(300), Some(micro()));
    assert_eq!(t.scale(301), Some(milli()));
    assert_eq!(t.scale(401), None);
    assert_eq!(
        a.chunk_ticks(p, 0, 301),
        Ok((1_000, a.tick(p, 300).unwrap()))
    );
    assert!(!t.gap_before(1));
    // a repeat is taken once, and chunks of one grid and scale join one stretch
    t.push_chunk(&head_on(a, 0, 301)).unwrap();
    t.push_chunk(&head_on(a, 100, 50)).unwrap();
    t.push_chunk(&head_on(b, 401, 99)).unwrap();
    assert_eq!(
        t.stretches(),
        &[
            SeriesStretch {
                first_item: 0,
                end_item: 301,
                grid: a
            },
            SeriesStretch {
                first_item: 301,
                end_item: 500,
                grid: b
            },
        ]
    );
}

#[test]
fn chunks_are_checked_against_their_neighbours() {
    let p = period();
    let a = SeriesGrid::start(10, 0, micro());
    let mut t = SeriesTimeline::new(p);
    assert_eq!(t.tick(0), None);
    t.push_chunk(&head_on(a, 10, 10)).unwrap();
    assert_eq!(t.tick(9), None);
    // the same samples on another grid or at another scale
    assert_eq!(
        t.push_chunk(&head_on(SeriesGrid::start(10, 5, micro()), 10, 10)),
        Err(SeriesError::Overlap { first_item: 10 })
    );
    assert_eq!(
        t.push_chunk(&head_on(a.rescaled(milli()), 15, 10)),
        Err(SeriesError::Overlap { first_item: 15 })
    );
    // sample 19 is at 9 · 62500/3 = 187500: a new grid starts more than a period (20833.3, rounded up 20834) after
    // it, so a phase shift of less than a period on the next sample is refused, never taken as a gap
    for shift in [0, 1, 20_834] {
        assert_eq!(
            t.push_chunk(&head_on(
                SeriesGrid::start(20, 187_500 + shift, micro()),
                20,
                5
            )),
            Err(SeriesError::GridTooClose { first_item: 20 }),
            "shift {shift}"
        );
    }
    assert!(!is_gap(p, 187_500, 187_500 + 20_834));
    assert!(is_gap(p, 187_500, 187_500 + 20_835));
    assert_eq!(
        t.push_chunk(&head_on(SeriesGrid::start(20, 187_500, micro()), 20, 5)),
        Err(SeriesError::GridTooClose { first_item: 20 })
    );
    // a chunk before the first must end before its first tick, 0
    assert_eq!(
        t.push_chunk(&head_on(SeriesGrid::start(0, -100, micro()), 0, 10)),
        Err(SeriesError::GridTooClose { first_item: 0 })
    );
    t.push_chunk(&head_on(SeriesGrid::start(0, -1_000_000, micro()), 0, 10))
        .unwrap();
    let gap = SeriesGrid::start(20, 187_500 + 20_835, micro());
    t.push_chunk(&head_on(gap, 20, 5)).unwrap();
    assert!(t.gap_before(1) && t.gap_before(2));
    // a lost chunk between two of one grid: its samples' ticks are known, their scale not
    t.push_chunk(&head_on(gap, 40, 5)).unwrap();
    assert_eq!(t.tick(30), gap.tick(p, 30));
    assert_eq!(t.scale(30), None);
    assert!(!t.gap_before(3));
    // a lost sample right after a stretch, before one on the same grid: its tick is known
    assert_eq!(t.tick(25), gap.tick(p, 25));
    let grid_after = SeriesGrid::start(31, 0, micro());
    let mut head = head_on(gap, 30, 2);
    head.grid = Some(grid_after);
    assert_eq!(
        t.push_chunk(&head),
        Err(SeriesError::GridAfterFirst {
            first_item: 30,
            grid_index: 31
        })
    );
    let overflow = SeriesGrid::start(0, 0, micro());
    assert_eq!(
        overflow.chunk_ticks(p, u64::MAX - 1, 1),
        Err(SeriesError::TickOverflow {
            index: u64::MAX - 1
        })
    );
    assert_eq!(
        overflow.chunk_ticks(p, 20, 0),
        Err(SeriesError::NoSamples { first_item: 20 })
    );
    assert_eq!(
        overflow.chunk_ticks(p, u64::MAX, 2),
        Err(SeriesError::NoSamples {
            first_item: u64::MAX
        })
    );
}

#[test]
fn timeline_from_chunks_in_any_order() {
    let p = period();
    let a = SeriesGrid::start(0, 1_000, micro());
    let b = a.rescaled(milli());
    let c = SeriesGrid::start(500, 99_000_000, micro());
    let mut bad_ticks = head_on(c, 600, 10);
    bad_ticks.last_tick += 1;
    let heads = [
        head_on(c, 500, 100),
        head_on(b, 300, 200),
        head_on(a, 0, 300),
        head_on(b, 300, 200),
        bad_ticks,
        head_on(SeriesGrid::start(400, 0, micro()), 400, 10),
    ];
    let (t, errors) = SeriesTimeline::from_chunks(p, &heads);
    let grids: std::vec::Vec<_> = t.stretches().iter().map(|s| s.grid).collect();
    assert_eq!(grids, [a, b, c]);
    assert_eq!(
        errors,
        [
            SeriesError::Overlap { first_item: 400 },
            SeriesError::ChunkTicks { first_item: 600 }
        ]
    );
}

/// A grid shifted by less than a period between two chunks of one grid is refused, so a lost chunk between them is
/// never timed on a grid it wasn't on.
#[test]
fn a_phase_shift_under_a_period_is_not_a_gap() {
    let p = period();
    let g = SeriesGrid::start(0, 0, micro());
    // H earlier than G by 7 ticks: H is refused, G again joins the first G
    let h = SeriesGrid::start(10, g.tick(p, 10).unwrap() - 7, micro());
    let heads = [head_on(g, 0, 10), head_on(h, 10, 10), head_on(g, 20, 10)];
    let (t, errors) = SeriesTimeline::from_chunks(p, &heads);
    assert_eq!(errors, [SeriesError::GridTooClose { first_item: 10 }]);
    assert_eq!(t.stretches().len(), 2);
    assert_eq!(t.tick(15), g.tick(p, 15), "a lost sample on G");
    // H later by 7 ticks is a gap of 7 ticks; G again is then less than a period after H, refused
    let h = SeriesGrid::start(10, g.tick(p, 10).unwrap() + 7, micro());
    let heads = [head_on(g, 0, 10), head_on(h, 10, 10), head_on(g, 20, 10)];
    let (t, errors) = SeriesTimeline::from_chunks(p, &heads);
    assert_eq!(errors, [SeriesError::GridTooClose { first_item: 20 }]);
    assert!(t.gap_before(1));
    assert_eq!(t.tick(25), None, "past the last chunk, no grid");
}

/// 100 000 heads given in descending order build the timeline in one pass after a sort, the same as in order.
#[test]
fn many_heads_out_of_order() {
    let p = period();
    let g = SeriesGrid::start(0, 0, micro());
    let mut heads: std::vec::Vec<ChunkHead> =
        (0..100_000u64).map(|k| head_on(g, k * 64, 64)).collect();
    let (forward, _) = SeriesTimeline::from_chunks(p, &heads);
    heads.reverse();
    let (t, errors) = SeriesTimeline::from_chunks(p, &heads);
    assert!(errors.is_empty());
    assert_eq!(t, forward);
    assert_eq!(t.stretches().len(), 1, "one grid, one stretch");
}

#[test]
fn chunk_heads_are_checked_against_the_grid() {
    let p = period();
    let mut head = head_on(SeriesGrid::start(0, 1_000, micro()), 0, 100);
    assert!(SeriesGrid::of_chunk(&head, p).is_ok());
    head.last_tick += 1;
    assert_eq!(
        SeriesGrid::of_chunk(&head, p),
        Err(SeriesError::ChunkTicks { first_item: 0 })
    );
    head.kind = BlockKind::Events;
    assert_eq!(
        SeriesGrid::of_chunk(&head, p),
        Err(SeriesError::NotSeriesChunk { first_item: 0 })
    );
}

/// The reserved leaves element (code 13) is refused wherever a sample of it would be decoded: the track's definition,
/// a samples column in any codec; its summaries in the summary tests.
#[test]
fn the_leaves_element_is_refused() {
    let leaves = SampleShape::new(Element::LEAVES, 1);
    assert_eq!(Element::LEAVES.code(), 13);
    assert_eq!(Element::from_code(13), None);
    let d = SeriesDef {
        shape: leaves,
        summaries: None,
        ..def()
    };
    assert!(d.check().is_err());
    assert!(SeriesDef::from_ww_bytes(&encode(&d)).is_err());
    for codec in 0..=255u8 {
        assert!(
            crate::Samples::new(crate::Codec(codec), leaves, 2, &[0; 64]).is_err(),
            "codec {codec}"
        );
    }
}

#[test]
fn a_series_relation_has_no_values() {
    let mut t = track(TrackKind::Series(def()), false);
    assert!(t.relations_canonical());
    let relation = crate::RelationDef {
        name: "context",
        target: Some(crate::RelationTarget {
            run: [1; 16],
            track: 9,
        }),
        values: crate::RelationValues::None,
        monotone: false,
        index: crate::RelationIndex::None,
    };
    t.relations = RefVec::Slice {
        slice: core::slice::from_ref(&relation),
    };
    assert!(t.relations_canonical(), "a series read with a state track");
    let stored = crate::RelationDef {
        values: crate::RelationValues::Stored,
        ..relation.clone()
    };
    t.relations = RefVec::Slice {
        slice: core::slice::from_ref(&stored),
    };
    assert!(
        !t.relations_canonical(),
        "a series has no items to carry values"
    );
}
