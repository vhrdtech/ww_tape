//! Definitions and state items near their use: [TapeWriter::define], [TapeWriter::put], [TapeWriter::close], the
//! copies, the open-definition cap, and the reader's lookups ([TapeReader::state_at], [TapeReader::definitions_at])
//! with blocks damaged.

use std::cell::Cell;
use ww_tape::TrackId;
use ww_tape::{ClockNum, RecordTrack, TrackNum};

use shrink_wrap::prelude::*;
use ww_tape::index::IndexPolicy;
use ww_tape::piece::{OwnerId, PieceHead, PieceId, PieceIdent};
use ww_tape::reader::{ByteSource, DefinitionsAt};
use ww_tape::writer::NearPolicy;
use ww_tape::{
    Block, BlockId, BlockKind, BlockWriter, ClockDef, ClockOrigin, ClockTime, Definitions,
    Describe, DescribeError, DescribeOwned, EventsDef, FileBlocks, FullVersion, Layout, Magnitude,
    MemSink, Numbers, Producer, Recovery, RelationDef, RelationIndex, RelationTarget,
    RelationValues, STATE_ITEM_MAX, Segment, SessionTag, Slot, StatePolicy, SyncPolicy, TapeReader,
    TapeSink, TapeWriter, TrackDef, TrackKind, TrackRepeat, TypeRef, TypedItem, Version,
    WriteError, WriterOptions, clock_key, track_key,
};

const TRACK_1: TrackNum = TrackNum::new(1);
const TRACK_2: TrackNum = TrackNum::new(2);
const TRACK_20: TrackNum = TrackNum::new(20);
const TRACK_6: TrackNum = TrackNum::new(6);
const TRACK_7: TrackNum = TrackNum::new(7);

/// The clock of the tests' tracks.
pub(crate) const CLOCK: u32 = 1;

pub(crate) fn clock(number: u32) -> Describe<'static> {
    Describe::Clock(ClockDef {
        number: ClockNum::new(number),
        key: clock_key(0xad, &[], u64::from(number)),
        rate: Magnitude::pow10(9).into(),
        resolution: Magnitude::pow10(-9).into(),
        origin: ClockOrigin::Boot,
        label: "host",
    })
}

/// Events track `number` on `clock`.
pub(crate) fn track(number: u32, clock: u32) -> Describe<'static> {
    labeled(number, clock, "defined")
}

/// The id of [track] `number`: what its chunks' heads carry.
pub(crate) fn track_id(number: u32) -> TrackId {
    let Describe::Track(t) = track(number, CLOCK) else {
        unreachable!("track() gives a track")
    };
    t.id()
}

/// Events track `number` on `clock`, labeled `label`.
pub(crate) fn labeled(number: u32, clock: u32, label: &'static str) -> Describe<'static> {
    Describe::Track(TrackDef {
        number: TrackNum::new(number),
        key: track_key(0xad, &[Segment::Index(number)]),
        clock: ClockNum::new(clock),
        dimensions: RefVec::Slice { slice: &[] },
        items: ww_tape::Items::WireWeaver {
            api: FullVersion::new("definitions_api", Version::new(0, 1, 0)),
            bundle: RefVec::Slice { slice: &[] },
            path: RefVec::Slice {
                slice: &[UNib32(7)],
            },
            keys: RefVec::Slice { slice: &[] },
            item: None,
        },
        label,
        repeat: TrackRepeat::Stream,
        kind: TrackKind::Events(EventsDef::INSTANTS),
        relations: RefVec::Slice { slice: &[] },
        owner: [0; 32],
    })
}

/// A state events track `number` on [CLOCK], repeated as [StatePolicy::DEFAULT].
pub(crate) fn state(number: u32) -> Describe<'static> {
    state_with(number, StatePolicy::DEFAULT)
}

/// A state events track `number` on [CLOCK], repeated as `policy`.
fn state_with(number: u32, policy: StatePolicy) -> Describe<'static> {
    let Describe::Track(t) = track(number, CLOCK) else {
        unreachable!("track() gives a track")
    };
    Describe::Track(TrackDef {
        repeat: TrackRepeat::State(policy),
        ..t
    })
}

/// What the tests' data needs, `layout` first: a producer, the layout, [CLOCK], and events tracks 1 to 9 on it
/// ([crate::pieces::records] writes on track 9, [crate::pieces::chunk] on tracks 1 to 3).
pub(crate) fn base(layout: Layout) -> Vec<Describe<'static>> {
    let mut out = vec![
        Describe::Producer(Producer { name: "tests" }),
        Describe::Layout(layout),
        clock(CLOCK),
    ];
    out.extend((1..=9).map(|n| track(n, CLOCK)));
    out
}

/// Hold `batch` in `defs` as a producer defines it: each clock through [Definitions::define_clock_with_maps], an
/// epoch clock with the maps from it in the batch, then the other definitions, then each track through
/// [Definitions::define_track].
/// The tests name tracks and clocks by the numbers they expect: each number given must be the one written in `batch`.
pub(crate) fn hold(defs: &mut Definitions, batch: &[Describe<'_>]) -> Result<(), DescribeError> {
    let maps_from = |from| -> Vec<(ClockNum, &[ww_tape::MapSegment])> {
        batch
            .iter()
            .filter_map(|d| match d {
                Describe::Map(m) if m.from == from => match &m.segments {
                    RefVec::Slice { slice } => Some((m.to, *slice)),
                    _ => None,
                },
                _ => None,
            })
            .collect()
    };
    let epochs: Vec<ClockNum> = batch
        .iter()
        .filter_map(|d| match d {
            Describe::Clock(c) if c.origin == ClockOrigin::Epoch => Some(c.number),
            _ => None,
        })
        .collect();
    for d in batch {
        if let Describe::Clock(c) = d {
            let maps = if epochs.contains(&c.number) {
                maps_from(c.number)
            } else {
                Vec::new()
            };
            let given = defs.define_clock_with_maps(c.spec(), &maps)?;
            assert_eq!(given, c.number, "clock {} is defined as {given}", c.number);
        }
    }
    let rest: Vec<Describe<'_>> = batch
        .iter()
        .filter(|d| match d {
            Describe::Clock(_) | Describe::Track(_) => false,
            Describe::Map(m) => !epochs.contains(&m.from),
            _ => true,
        })
        .cloned()
        .collect();
    defs.define_all(&rest)?;
    for d in batch {
        if let Describe::Track(t) = d {
            let given = defs.define_track(t.spec())?;
            assert_eq!(given, t.number, "track {} is defined as {given}", t.number);
        }
    }
    Ok(())
}

/// The numbers `batch` writes its tracks and clocks under: what an application that numbered them before passes to
/// [Definitions::with_numbers], so that defining them gives these numbers back.
pub(crate) fn numbers_of(batch: &[Describe<'_>]) -> Numbers {
    let mut n = Numbers::new();
    for d in batch {
        match d {
            Describe::Track(t) => n.take_track(t.number, t.id(), t.owner),
            Describe::Clock(c) => n.take_clock(c.number, c.key),
            _ => {}
        }
    }
    n
}

/// Definitions holding `batch`, numbered as `batch` writes it ([numbers_of]).
pub(crate) fn held(batch: &[Describe<'_>]) -> Definitions {
    let mut defs = Definitions::with_numbers(numbers_of(batch));
    hold(&mut defs, batch).unwrap();
    defs
}

/// A writer and the definitions the test passes with each call: the application's side of [TapeWriter].
pub(crate) struct Rig<S: TapeSink = MemSink> {
    pub w: TapeWriter<S>,
    pub defs: Definitions,
}

impl<S: TapeSink> std::ops::Deref for Rig<S> {
    type Target = TapeWriter<S>;
    fn deref(&self) -> &TapeWriter<S> {
        &self.w
    }
}

impl<S: TapeSink> std::ops::DerefMut for Rig<S> {
    fn deref_mut(&mut self) -> &mut TapeWriter<S> {
        &mut self.w
    }
}

impl<S: TapeSink> Rig<S> {
    pub(crate) fn new(w: TapeWriter<S>) -> Self {
        Self::with_numbers(w, Numbers::new())
    }

    /// A rig whose definitions number tracks and clocks as `numbers` does ([Definitions::with_numbers]).
    pub(crate) fn with_numbers(w: TapeWriter<S>, numbers: Numbers) -> Self {
        Self {
            w,
            defs: Definitions::with_numbers(numbers),
        }
    }

    /// Hold `d` ([Definitions::define]); a producer or layout goes to the writer instead.
    pub(crate) fn define(&mut self, d: &Describe<'_>) -> Result<(), DescribeError> {
        self.define_all(core::slice::from_ref(d))
    }

    /// Hold `records` as one batch; a producer or layout among them goes to the writer.
    pub(crate) fn define_all(&mut self, records: &[Describe<'_>]) -> Result<(), DescribeError> {
        let mut held = Vec::new();
        for d in records {
            match d {
                Describe::Producer(p) => self.w.set_producer(p.name),
                Describe::Layout(l) => self.w.set_layout(*l).unwrap(),
                d => held.push(d.clone()),
            }
        }
        hold(&mut self.defs, &held)
    }

    pub(crate) fn close(&mut self, number: u32) -> bool {
        self.defs.close(TrackNum::new(number))
    }

    pub(crate) fn write_block(&mut self, block: &[u8]) -> Result<(), WriteError<S::Error>> {
        self.w.write_block(&self.defs, block).map(|_| ())
    }

    pub(crate) fn defer_block(&mut self, block: Vec<u8>) -> Result<(), WriteError<S::Error>> {
        self.w.defer_block(&self.defs, block)
    }

    pub(crate) fn put(
        &mut self,
        track: u32,
        ticks: i64,
        item: &[u8],
    ) -> Result<bool, WriteError<S::Error>> {
        self.w.put(&self.defs, TrackNum::new(track), ticks, item)
    }

    pub(crate) fn put_item(&mut self, item: &TypedItem<'_>) -> Result<bool, WriteError<S::Error>> {
        self.w.put_item(&self.defs, item)
    }

    pub(crate) fn close_piece(self) -> Result<ww_tape::writer::Closed, WriteError<S::Error>> {
        self.w.close()
    }
}

/// The definitions [base] holds (its clock and tracks 1 to 9), made once.
pub(crate) fn base_defs() -> &'static Definitions {
    static D: std::sync::OnceLock<Definitions> = std::sync::OnceLock::new();
    D.get_or_init(|| {
        let mut d = Definitions::new();
        crate::definitions::hold(&mut d, &base(Layout::UNALIGNED)[2..]).unwrap();
        d
    })
}

/// Define every record of `records` on `w`, as one batch.
pub(crate) fn define_all<S: TapeSink>(w: &mut Rig<S>, records: &[Describe<'_>]) {
    w.define_all(records).unwrap();
}

/// The track numbers `defs` holds a definition of.
pub(crate) fn tracks(defs: &DefinitionsAt) -> Vec<u32> {
    defs.items
        .keys()
        .filter_map(|s| match s {
            Slot::Track { number } => Some(number.get()),
            _ => None,
        })
        .collect()
}

/// A good record block of a piece: its offset, whether marked FLAG_REPEAT (put or copied), its first record's track,
/// its bytes.
pub(crate) struct RecordsAt {
    pub offset: u64,
    pub repeat: bool,
    pub first_track: Option<u32>,
    pub bytes: Vec<u8>,
}

/// Every good record block of `bytes`, up to the first cut.
pub(crate) fn record_blocks(bytes: &[u8]) -> Vec<RecordsAt> {
    let mut rd = FileBlocks::with_recovery(bytes, Recovery::Prefix);
    let mut out = vec![];
    loop {
        let Some(Ok(b)) = rd.next_block() else {
            return out;
        };
        if b.kind != BlockKind::Records {
            continue;
        }
        let (repeat, first_track, bytes) = (b.is_repeat(), b.first_track(), b.bytes().to_vec());
        out.push(RecordsAt {
            offset: rd.offset(),
            repeat,
            first_track: first_track.map(RecordTrack::get),
            bytes,
        });
    }
}

/// The blocks of puts and copies in `bytes`: each block's offset and the slots of its records, in file order.
pub(crate) fn puts(bytes: &[u8]) -> Vec<(u64, Vec<Slot>)> {
    record_blocks(bytes)
        .into_iter()
        .filter(|b| b.repeat)
        .map(|b| {
            let block = Block::read(&b.bytes).unwrap();
            let slots = block
                .records()
                .map(|r| {
                    let r = r.unwrap();
                    Slot::of(r.track, r.item).unwrap()
                })
                .collect();
            (b.offset, slots)
        })
        .collect()
}

/// Offsets of every record of `slot` in `bytes`, made or put.
pub(crate) fn writes_of(bytes: &[u8], slot: Slot) -> Vec<u64> {
    record_blocks(bytes)
        .into_iter()
        .filter(|b| {
            Block::read(&b.bytes)
                .unwrap()
                .records()
                .any(|r| r.ok().and_then(|r| Slot::of(r.track, r.item)) == Some(slot))
        })
        .map(|b| b.offset)
        .collect()
}

/// The options of the tests: no sync, checkpoints every 1 KiB, copies `copy_spacing` apart.
pub(crate) fn options(copy_spacing: u64) -> WriterOptions {
    WriterOptions {
        sync: SyncPolicy::None,
        clock: ww_tape::ClockRef::NONE,
        index: IndexPolicy {
            bytes: exact_units::Bytes::new(1024),
            interval: None,
            ..IndexPolicy::default()
        },
        layout: Layout::UNALIGNED,
        producer: "definitions".to_owned(),
        near: NearPolicy {
            copy_spacing: exact_units::Bytes::new(copy_spacing),
            ..NearPolicy::default()
        },
    }
}

/// A first piece on a memory sink with a producer, `layout`, a clock and track 1 defined, not yet flushed.
fn begin_with(o: WriterOptions, layout: Layout) -> Rig {
    begin_numbered(o, layout, &[])
}

/// [begin_with], the definitions numbering the tracks and clocks of `later` as it writes them ([numbers_of]).
pub(crate) fn begin_numbered(o: WriterOptions, layout: Layout, later: &[Describe<'_>]) -> Rig {
    let mut w = TapeWriter::create_on(MemSink::new(), o.sync);
    w.set_index_policy(o.index)
        .expect("a policy with room for a piece head");
    w.set_near_policy(o.near);
    w.set_layout(layout).unwrap();
    w.set_producer("definitions");
    let owner = OwnerId([5; 32]);
    w.begin_piece(
        &PieceHead {
            ident: PieceIdent {
                id: PieceId::first(&owner),
                owner,
                seq: 0,
            },
            prev: None,
            prev_session: SessionTag::NONE,
            integrity: ww_tape::Integrity::Full,
        },
        &[],
    )
    .unwrap();
    let first = [clock(CLOCK), track(1, CLOCK)];
    let mut w = Rig::with_numbers(w, numbers_of(&[&first[..], later].concat()));
    define_all(&mut w, &first);
    w
}

/// [begin_with] the unaligned layout.
pub(crate) fn begin(o: WriterOptions) -> Rig {
    begin_with(o, Layout::UNALIGNED)
}

/// A record block numbered `id` of records `(track, ticks, item)` on [CLOCK].
pub(crate) fn on_tracks(id: BlockId, records: &[(u32, i64, &[u8])]) -> Vec<u8> {
    let mut buf = vec![0u8; 4096];
    let mut b =
        BlockWriter::new(&mut buf, BlockKind::Records, id, &[ClockNum::new(CLOCK)]).unwrap();
    for (track, ticks, item) in records {
        b.push(
            TrackNum::new(*track),
            ClockTime::new(ClockNum::new(CLOCK), *ticks),
            &[item],
        )
        .unwrap();
    }
    let len = b.finish();
    buf.truncate(len);
    buf
}

/// `n` record blocks of track 1 from tick `from` on.
pub(crate) fn write_records<S: TapeSink>(w: &mut Rig<S>, from: i64, n: i64) {
    write_on(w, 1, from, n);
}

/// `n` record blocks of track `track` from tick `from` on.
pub(crate) fn write_on<S: TapeSink>(w: &mut Rig<S>, track: u32, from: i64, n: i64) {
    for k in from..from + n {
        w.write_block(&on_tracks(w.next_id(), &[(track, k, &[k as u8; 30])]))
            .unwrap();
    }
}

/// Track 0's policy for the near tests: a definition is put again 2 KiB on, no copies.
const NEAR_2K: StatePolicy = StatePolicy {
    near: exact_units::Bytes::new(2048),
    copies: exact_units::Count::new(0),
};

/// A definition is put before a block of data when it doesn't lie within track 0's `near` behind: track 1's, with
/// data throughout, again and again, never further than `near` from a block of its data; track 2's only once its
/// data starts; track 3's, which never has data, never.
#[test]
fn a_definition_is_put_where_it_is_used() {
    let mut w = begin_with(
        options(u64::MAX),
        Layout::UNALIGNED.with_definitions(NEAR_2K),
    );
    define_all(&mut w, &[track(2, CLOCK), track(3, CLOCK)]);
    let tape = w.sink().tape();
    write_records(&mut w, 0, 100);
    write_on(&mut w, 2, 100, 50);
    w.close_piece().unwrap();
    let bytes = tape.to_vec().unwrap();
    let r = TapeReader::open(&bytes[..]).unwrap();
    let one = writes_of(
        &bytes,
        Slot::Track {
            number: TrackNum::new(1),
        },
    );
    assert!(one.len() > 3, "{one:?}");
    assert!(one.windows(2).all(|w| w[1] - w[0] > 2048), "{one:?}");
    let data: Vec<u64> = record_blocks(&bytes)
        .iter()
        .filter(|b| b.first_track == Some(1))
        .map(|b| b.offset)
        .collect();
    for at in data {
        let behind = one.iter().filter(|o| **o < at).max().unwrap();
        // a put block lies right before the data block it serves: within near and a block
        assert!(
            at - behind <= 2048 + 200,
            "data at {at}, definition at {behind}"
        );
        let found = r
            .state_at(
                0,
                Slot::Track {
                    number: TrackNum::new(1),
                },
                at,
            )
            .unwrap();
        assert_eq!(found.item.map(|i| i.offset), Some(*behind));
    }
    let two = writes_of(
        &bytes,
        Slot::Track {
            number: TrackNum::new(2),
        },
    );
    let first_data_two = record_blocks(&bytes)
        .iter()
        .find(|b| b.first_track == Some(2))
        .unwrap()
        .offset;
    assert!(
        two.len() >= 2,
        "made, then put again near its data: {two:?}"
    );
    assert!(two[1] > first_data_two - 300, "{two:?}");
    assert_eq!(
        writes_of(
            &bytes,
            Slot::Track {
                number: TrackNum::new(3)
            }
        )
        .len(),
        0,
        "no data: never"
    );
}

/// The reader takes a track's nearest definition behind a place: the old label before a redefinition, the new one
/// after it. With the newest block damaged, the one before is taken and the damaged block is reported (reading side
/// point 4).
#[test]
fn the_reader_takes_the_definition_in_force() {
    let mut w = begin(options(u64::MAX));
    let tape = w.sink().tape();
    write_records(&mut w, 0, 3);
    w.define(&labeled(1, CLOCK, "v2")).unwrap();
    write_records(&mut w, 3, 3);
    w.close_piece().unwrap();
    let bytes = tape.to_vec().unwrap();
    let r = TapeReader::open(&bytes[..]).unwrap();
    let data: Vec<u64> = record_blocks(&bytes)
        .iter()
        .filter(|b| b.first_track == Some(1))
        .map(|b| b.offset)
        .collect();
    let label = |r: &TapeReader<&[u8]>, at: u64| {
        let found = r
            .state_at(
                0,
                Slot::Track {
                    number: TrackNum::new(1),
                },
                at,
            )
            .unwrap();
        let item = found.item.map(|i| i.item);
        item.map(|i| match DescribeOwned::from_ww_bytes_owned(&i).unwrap() {
            DescribeOwned::Track(t) => t.label,
            other => panic!("{other:?}"),
        })
    };
    assert_eq!(label(&r, data[0]).as_deref(), Some("defined"));
    assert_eq!(label(&r, data[5]).as_deref(), Some("v2"));
    let all = r.definitions_at(0, u64::MAX).unwrap();
    assert!(all.is_whole());
    assert_eq!(tracks(&all), [1]);

    // the block of the redefinition damaged: the old one, the damage reported
    let v2 = writes_of(
        &bytes,
        Slot::Track {
            number: TrackNum::new(1),
        },
    )[1];
    let mut damaged = bytes.clone();
    damaged[v2 as usize + 40] ^= 1;
    let r = TapeReader::open(&damaged[..]).unwrap();
    let found = r
        .state_at(
            0,
            Slot::Track {
                number: TrackNum::new(1),
            },
            u64::MAX,
        )
        .unwrap();
    assert!(found.item.is_some());
    assert_eq!(
        found.lost,
        [ww_tape::reader::Lost {
            offset: v2,
            slot: Slot::Track {
                number: TrackNum::new(1)
            }
        }]
    );
    assert!(!r.definitions_at(0, u64::MAX).unwrap().is_whole());
}

/// A lookup says where the index doesn't cover the stretch from the record it returns to the place: past a cut, a
/// place past the end; a closed piece at its end has none (reading side review 2, point 4).
#[test]
fn a_lookup_names_the_stretches_the_index_does_not_cover() {
    let mut w = begin(options(u64::MAX));
    let tape = w.sink().tape();
    write_records(&mut w, 0, 3);
    w.close_piece().unwrap();
    let bytes = tape.to_vec().unwrap();
    let slot = Slot::Track {
        number: TrackNum::new(1),
    };
    let r = TapeReader::open(&bytes[..]).unwrap();
    let at_end = r.state_at(0, slot, u64::MAX).unwrap();
    assert!(at_end.item.is_some());
    assert!(at_end.is_whole());
    let len = r.pieces()[0].end - r.pieces()[0].start;
    let past = r.state_at(0, slot, len + 100).unwrap();
    assert!(matches!(past.uncovered.as_slice(), [r] if *r == (len..len + 100)));
    assert!(!r.definitions_at(0, len + 100).unwrap().is_whole());
    // an unclosed piece cut inside its last block: a newer record may lie past the cut
    let data = record_blocks(&bytes)
        .iter()
        .filter(|b| b.first_track == Some(1))
        .map(|b| b.offset)
        .collect::<Vec<_>>();
    let cut = data[2] as usize + 20;
    let mut cut_off = bytes[..cut].to_vec();
    cut_off.extend(std::iter::repeat_n(0x5a, 64));
    let r = TapeReader::open(&cut_off[..]).unwrap();
    let p = &r.pieces()[0];
    let at = p.cut.expect("the piece is cut") - p.start;
    let found = r.state_at(0, slot, u64::MAX).unwrap();
    assert!(found.item.is_some());
    assert!(matches!(found.uncovered.as_slice(), [r] if *r == (at..u64::MAX)));
    assert!(!found.is_whole());
}

/// A lookup whose read budget runs out says so: it is not damage, and no block is listed lost for it (reading side
/// review 3, point 4).
#[test]
fn a_spent_read_budget_is_its_own_answer() {
    let mut w = begin(options(u64::MAX));
    let tape = w.sink().tape();
    write_records(&mut w, 0, 3);
    w.close_piece().unwrap();
    let bytes = tape.to_vec().unwrap();
    let slot = Slot::Track {
        number: TrackNum::new(1),
    };
    let open = |max: u64| {
        let options = ww_tape::OpenOptions {
            state_read_max: exact_units::Bytes::new(max),
            ..Default::default()
        };
        TapeReader::open_with(&bytes[..], options).unwrap()
    };
    let r = open(10);
    let found = r.state_at(0, slot, u64::MAX).unwrap();
    assert_eq!(
        (found.item.as_ref(), found.lost.as_slice()),
        (None, &[][..])
    );
    assert!(found.spent);
    assert!(!found.is_whole());
    let defs = r.definitions_at(0, u64::MAX).unwrap();
    assert!(defs.spent && defs.lost.is_empty() && !defs.is_whole());
    let r = open(u64::MAX);
    assert!(r.state_at(0, slot, u64::MAX).unwrap().is_whole());
}

/// A byte source that counts its reads.
struct Counted<'a> {
    bytes: &'a [u8],
    reads: Cell<u64>,
}

impl ByteSource for Counted<'_> {
    type Error = ww_tape::reader::OutOfRange;

    fn size(&self) -> Result<u64, Self::Error> {
        self.bytes.size()
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), Self::Error> {
        self.reads.set(self.reads.get() + 1);
        self.bytes.read_at(offset, buf)
    }
}

/// A reader keeps a piece's state entries once read: a second lookup reads only the record's block, not the index
/// again (reading side review 2, point 6).
#[test]
fn a_reader_keeps_the_state_entries_once_read() {
    let mut w = begin(options(u64::MAX));
    let tape = w.sink().tape();
    let tracks: Vec<Describe<'_>> = (2..=9).map(|n| track(n, CLOCK)).collect();
    define_all(&mut w, &tracks);
    for t in 1..=9 {
        write_on(&mut w, t, 0, 2);
    }
    w.close_piece().unwrap();
    let bytes = tape.to_vec().unwrap();
    let r = TapeReader::open(Counted {
        bytes: &bytes,
        reads: Cell::new(0),
    })
    .unwrap();
    let reads = || r.source().reads.get();
    let before = reads();
    let found = r
        .state_at(
            0,
            Slot::Track {
                number: TrackNum::new(4),
            },
            u64::MAX,
        )
        .unwrap();
    assert!(found.item.is_some());
    let first = reads() - before;
    let before = reads();
    let found = r
        .state_at(
            0,
            Slot::Track {
                number: TrackNum::new(7),
            },
            u64::MAX,
        )
        .unwrap();
    assert!(found.item.is_some());
    assert_eq!(reads() - before, 1, "one read: the record's block");
    assert!(first > 1, "the first lookup read the index too");
}

/// A put writes an item unless the newest record of its track is that item within `near` behind: the same item
/// again is skipped, another one written, the first one again written too (the track moved on). A put on a stream
/// track, on a track not open or of an item too long for a block is refused.
#[test]
fn a_put_writes_only_what_is_not_close_behind() {
    let fives = [state_with(5, NEAR_2K)];
    let mut w = begin_numbered(options(u64::MAX), Layout::UNALIGNED, &fives);
    define_all(&mut w, &fives);
    let tape = w.sink().tape();
    assert!(w.put(5, 0, b"A").unwrap());
    assert!(!w.put(5, 1, b"A").unwrap());
    assert!(w.put(5, 2, b"B").unwrap());
    assert!(w.put(5, 3, b"A").unwrap());
    assert!(!w.put(5, 4, b"A").unwrap());
    write_records(&mut w, 0, 40);
    assert!(w.put(5, 5, b"A").unwrap(), "past near: written again");
    for (track, e) in [
        (
            1,
            DescribeError::NotState {
                track: TrackNum::new(1),
            },
        ),
        (
            6,
            DescribeError::UnknownTrack {
                track: TrackNum::new(6),
            },
        ),
    ] {
        assert!(
            matches!(w.put(track, 0, b"x"), Err(WriteError::Describe(got)) if got == e),
            "track {track}"
        );
    }
    let long = vec![0u8; STATE_ITEM_MAX + 1];
    assert!(matches!(
        w.put(5, 0, &long),
        Err(WriteError::Describe(DescribeError::TooLong { .. }))
    ));
    w.close_piece().unwrap();
    let bytes = tape.to_vec().unwrap();
    assert_eq!(
        writes_of(
            &bytes,
            Slot::State {
                track: TrackNum::new(5)
            }
        )
        .len(),
        4
    );
    // the reader takes the item in force
    let r = TapeReader::open(&bytes[..]).unwrap();
    let found = r
        .state_at(
            0,
            Slot::State {
                track: TrackNum::new(5),
            },
            u64::MAX,
        )
        .unwrap();
    let item = found.item.unwrap();
    assert_eq!(
        (item.item.as_slice(), item.at),
        (&b"A"[..], Some(ClockTime::new(ClockNum::new(CLOCK), 5)))
    );
    let by_hash = r.item_by_hash(0, ww_tape::ItemHash::of(b"B")).unwrap();
    assert_eq!(by_hash.item.unwrap().item, b"B");
    // a state entry's item number and count stay 0, kept for numbering every record
    let entries = r.state_entries(0).unwrap();
    assert!(!entries.is_empty());
    assert!(
        entries
            .iter()
            .all(|(e, _)| (e.first_item, e.count) == (ww_tape::ItemNum::new(0), 0))
    );
}

/// A producer's block of puts numbered `id`: definitions `defs`, then `records` (track, ticks on [CLOCK], item).
pub(crate) fn puts_of(
    id: BlockId,
    defs: &[Describe<'_>],
    records: &[(u32, i64, &[u8])],
) -> Vec<u8> {
    let items: Vec<Vec<u8>> = defs
        .iter()
        .map(|d| ww_tape::open::encode(d).unwrap())
        .collect();
    let mut refs: Vec<ww_tape::RecordRef<'_>> = items
        .iter()
        .map(|item| ww_tape::RecordRef {
            track: RecordTrack::Definitions,
            at: None,
            item,
        })
        .collect();
    refs.extend(
        records
            .iter()
            .map(|(track, ticks, item)| ww_tape::RecordRef {
                track: RecordTrack::from_u32(*track),
                at: Some(ClockTime::new(ClockNum::new(CLOCK), *ticks)),
                item,
            }),
    );
    let mut out = Vec::new();
    ww_tape::open::write_records(
        &refs,
        1 << 20,
        ww_tape::FLAG_REPEAT,
        || id,
        |b| {
            out.push(b.to_vec());
            Ok::<(), DescribeError>(())
        },
    )
    .unwrap();
    out.remove(0)
}

/// A relay passes on a live producer's stream with definitions of its own, empty at the start: a block of puts has
/// its definitions held and its records put, a records block after it is written under them.
#[test]
fn a_relay_holds_the_definitions_of_the_blocks_of_puts() {
    let mut w = begin(options(u64::MAX));
    let tape = w.sink().tape();
    let producer = BlockId::new(SessionTag::random(), 7);
    let mut defs = Definitions::new();
    let puts = puts_of(
        producer,
        &[clock(CLOCK), state(5), track(6, CLOCK)],
        &[(5, 3, b"A")],
    );
    w.w.relay_block(&mut defs, &puts).unwrap();
    assert!(
        defs.track(TrackNum::new(6)).is_some(),
        "the block's definitions are held"
    );
    let data = on_tracks(producer.next(), &[(6, 4, b"data")]);
    w.w.relay_block(&mut defs, &data).unwrap();
    // a records block on a track the relay doesn't hold is refused
    let unknown = on_tracks(producer.next().next(), &[(7, 5, b"data")]);
    assert!(w.w.relay_block(&mut defs, &unknown).is_err());
    w.w.close().unwrap();
    let bytes = tape.to_vec().unwrap();
    let r = TapeReader::open(&bytes[..]).unwrap();
    let at = r.definitions_at(0, u64::MAX).unwrap();
    assert!(at.items.contains_key(&Slot::Track {
        number: TrackNum::new(6)
    }));
    let found = r
        .state_at(
            0,
            Slot::State {
                track: TrackNum::new(5),
            },
            u64::MAX,
        )
        .unwrap();
    assert_eq!(found.item.unwrap().item, b"A");
}

/// A producer's block of puts (a live recorder's, relayed) is put on the tape, not written as it is: its definitions
/// defined, its state records put by this writer's near; the same block again writes nothing, and a record on a
/// stream track is refused with nothing put.
#[test]
fn a_producers_block_of_puts_is_put_on_the_tape() {
    let mut w = begin(options(u64::MAX));
    let tape = w.sink().tape();
    let producer = BlockId::new(SessionTag::random(), 7);
    let puts_of =
        |defs: &[Describe<'_>], records: &[(u32, i64, &[u8])]| puts_of(producer, defs, records);
    let block = puts_of(&[state(5)], &[(5, 3, b"A")]);
    // a relay that writes under the producer's definitions holds them too
    let taken = w.defs.take_block(&Block::read(&block).unwrap()).unwrap();
    assert_eq!(taken, 1);
    let len = tape.to_vec().unwrap().len();
    w.write_block(&block).unwrap();
    let after_first = tape.to_vec().unwrap().len();
    assert!(after_first > len);
    w.write_block(&block).unwrap();
    assert_eq!(tape.to_vec().unwrap().len(), after_first, "nothing new");
    // a record on a stream track is refused with nothing put
    let stream = puts_of(&[], &[(5, 4, b"B"), (1, 4, b"x")]);
    assert!(matches!(
        w.write_block(&stream),
        Err(WriteError::Describe(DescribeError::NotState {
            track: TRACK_1
        }))
    ));
    assert_eq!(tape.to_vec().unwrap().len(), after_first, "nothing put");
    // a definition the writer doesn't hold is refused with nothing put: a relay holds a block's definitions first,
    // so nothing reaches the tape unchecked (writing side review 3, point 6)
    let unheld = puts_of(&[track(6, CLOCK)], &[(5, 4, b"B")]);
    assert!(matches!(
        w.write_block(&unheld),
        Err(WriteError::Describe(DescribeError::NotHeld {
            slot: Slot::Track { number: TRACK_6 }
        }))
    ));
    assert_eq!(tape.to_vec().unwrap().len(), after_first, "nothing put");
    // a definition of a kind this crate doesn't know, from a newer producer, is passed on as it is (writing side
    // review 2, point 10)
    #[derive_shrink_wrap(borrowed, derive(Debug), ww_repr = u4)]
    enum Newer {
        A,
        B,
        C,
        D,
        E,
        F,
        G,
        H,
        I,
        J,
        K,
        L,
        M,
        N { first_seq: u64 },
    }
    let mut scratch = [0u8; 32];
    let newer = Newer::N { first_seq: 7 }
        .to_ww_bytes(&mut scratch)
        .unwrap()
        .to_vec();
    assert!(matches!(
        Describe::from_ww_bytes(&newer),
        Ok(Describe::Unknown(13))
    ));
    let mut out = Vec::new();
    ww_tape::open::write_records(
        &[
            ww_tape::RecordRef {
                track: RecordTrack::Definitions,
                at: None,
                item: &newer,
            },
            ww_tape::RecordRef {
                track: RecordTrack::from_u32(5),
                at: Some(ClockTime::new(ClockNum::new(CLOCK), 5)),
                item: b"C",
            },
        ],
        4096,
        ww_tape::FLAG_REPEAT,
        || producer,
        |b| {
            out.push(b.to_vec());
            Ok::<(), DescribeError>(())
        },
    )
    .unwrap();
    w.write_block(&out[0]).unwrap();
    w.close_piece().unwrap();
    let bytes = tape.to_vec().unwrap();
    let relayed = record_blocks(&bytes).iter().any(|b| {
        Block::read(&b.bytes)
            .unwrap()
            .records()
            .any(|r| r.unwrap().item == newer.as_slice())
    });
    assert!(relayed, "the unknown definition is on the tape");
    assert_eq!(
        writes_of(
            &bytes,
            Slot::State {
                track: TrackNum::new(5)
            }
        )
        .len(),
        2
    );
    let r = TapeReader::open(&bytes[..]).unwrap();
    let found = r
        .state_at(
            0,
            Slot::State {
                track: TrackNum::new(5),
            },
            u64::MAX,
        )
        .unwrap();
    assert_eq!(
        found.item.unwrap().at,
        Some(ClockTime::new(ClockNum::new(CLOCK), 5))
    );
    // it is listed under its discriminant plus 1, the code a reader that knows it lists it under too
    assert!(r.verify(0).unwrap().holds);
    let at = r.definitions_at(0, u64::MAX).unwrap();
    assert!(at.items.keys().any(|s| matches!(
        s,
        Slot::Own {
            kind: ww_tape::state::DefinitionKind::Unknown(14),
            ..
        }
    )));
    // the writer's own session and seq: the producer's block id isn't kept
    assert!(
        record_blocks(&bytes)
            .iter()
            .all(|b| Block::read(&b.bytes).unwrap().session != producer.session)
    );
}

/// A use writes a copy of an item while fewer than `copies` copies lie in the piece and the newest record is at
/// least `copy_spacing` behind; nothing is written ahead, and a new item in the slot starts over.
#[test]
fn a_use_writes_the_copies() {
    let policy = StatePolicy {
        near: exact_units::Bytes::new(1 << 20),
        copies: exact_units::Count::new(2),
    };
    let states = [state_with(5, policy), state_with(6, policy)];
    let mut w = begin_numbered(options(1024), Layout::UNALIGNED, &states);
    define_all(&mut w, &states);
    let tape = w.sink().tape();
    assert!(w.put(5, 0, b"five").unwrap());
    w.put(6, 0, b"six").unwrap();
    // five is put at each use; six only once
    for k in 0..60 {
        write_records(&mut w, k, 1);
        w.put(5, k, b"five").unwrap();
    }
    w.put(6, 1, b"six again").unwrap();
    for k in 60..120 {
        write_records(&mut w, k, 1);
        w.put(6, k, b"six again").unwrap();
    }
    w.close_piece().unwrap();
    let bytes = tape.to_vec().unwrap();
    let five = writes_of(
        &bytes,
        Slot::State {
            track: TrackNum::new(5),
        },
    );
    assert_eq!(five.len(), 3, "the first write and two copies: {five:?}");
    assert!(five.windows(2).all(|w| w[1] - w[0] >= 1024), "{five:?}");
    assert_eq!(
        writes_of(
            &bytes,
            Slot::State {
                track: TrackNum::new(6)
            }
        )
        .len(),
        1 + 3,
        "six once, never used again: no copy; six again and its two copies"
    );
}

/// Dropping a track from the definitions drops it and the clock only it counts through: data on it is refused until
/// it is held again, and the writer writes nothing for it.
#[test]
fn a_track_dropped_from_the_definitions_is_refused() {
    let mut w = begin_numbered(
        options(u64::MAX),
        Layout::UNALIGNED,
        &[clock(2), track(7, 2)],
    );
    define_all(&mut w, &[clock(2), track(7, 2)]);
    write_records(&mut w, 0, 1);
    let tape = w.sink().tape();
    let len = tape.to_vec().unwrap().len();
    assert!(w.close(7));
    assert!(!w.close(7));
    assert_eq!(tape.to_vec().unwrap().len(), len, "nothing written");
    assert!(w.defs.track(TrackNum::new(7)).is_none());
    assert!(!w.defs.has_clock(ClockNum::new(2)));
    assert!(
        w.defs.has_clock(ClockNum::new(CLOCK)),
        "track 1 counts through it"
    );
    assert!(matches!(
        w.write_block(&on_tracks(w.next_id(), &[(7, 9, b"x")])),
        Err(WriteError::Describe(DescribeError::UnknownTrack {
            track: TRACK_7
        }))
    ));
    assert_eq!(
        w.define(&track(7, 2)),
        Err(DescribeError::UnknownClock {
            clock: ClockNum::new(2)
        })
    );
    define_all(&mut w, &[clock(2), track(7, 2)]);
    assert!(w.defs.track(TrackNum::new(7)).is_some());
    w.close_piece().unwrap();
}

/// The track spec of [track]-like `d`.
fn track_spec(d: Describe<'static>) -> ww_tape::TrackSpec<'static> {
    let Describe::Track(t) = d else {
        unreachable!("a track")
    };
    t.spec()
}

/// The clock spec of [clock] `number`.
fn clock_spec(number: u32) -> ww_tape::ClockSpec<'static> {
    let Describe::Clock(c) = clock(number) else {
        unreachable!("clock() gives a clock")
    };
    c.spec()
}

/// Definitions number tracks and clocks by identity: the same one defined again gets its number back, a new one the
/// lowest not taken; a closed track's number stays its own, and the same key with another owner is another track.
#[test]
fn definitions_number_tracks_and_clocks_by_identity() {
    let mut defs = Definitions::new();
    assert_eq!(defs.define_clock(clock_spec(CLOCK)), Ok(ClockNum::new(1)));
    assert_eq!(
        defs.define_clock(clock_spec(CLOCK)),
        Ok(ClockNum::new(1)),
        "the same key"
    );
    assert_eq!(defs.define_clock(clock_spec(7)), Ok(ClockNum::new(2)));
    let (a, b, c) = (
        track_spec(track(40, CLOCK)),
        track_spec(track(41, CLOCK)),
        track_spec(track(42, CLOCK)),
    );
    assert_eq!(defs.define_track(a.clone()), Ok(TRACK_1));
    assert_eq!(defs.define_track(b.clone()), Ok(TRACK_2));
    // the same identity with another label: its number, the definition replaced
    assert_eq!(
        defs.define_track(track_spec(labeled(40, CLOCK, "renamed"))),
        Ok(TRACK_1)
    );
    assert_eq!(defs.tracks().count(), 2);
    // a new track doesn't take a closed track's number; the closed one defined again gets it back
    assert!(defs.close(TRACK_1));
    assert_eq!(defs.define_track(c), Ok(TrackNum::new(3)));
    assert_eq!(defs.define_track(a.clone()), Ok(TRACK_1));
    // the owner is part of the identity, not of the id
    let owned = ww_tape::TrackSpec {
        owner: [4; 32],
        ..b.clone()
    };
    assert_eq!(owned.id(), b.id());
    assert_eq!(defs.define_track(owned), Ok(TrackNum::new(4)));
    assert_eq!(defs.numbers().track(b.id(), b.owner), Some(TRACK_2));
    assert_eq!(defs.numbers().track(a.id(), [4; 32]), None);
}

/// [Definitions::define] and [Definitions::define_all] take no track and no clock ([DescribeError::Numbered]): those
/// are numbered by [Definitions::define_track] and [Definitions::define_clock]. Nothing of a refused batch is held.
#[test]
fn define_all_refuses_a_track_or_a_clock() {
    let mut defs = Definitions::new();
    assert_eq!(defs.define(&clock(CLOCK)), Err(DescribeError::Numbered));
    assert_eq!(defs.define(&track(1, CLOCK)), Err(DescribeError::Numbered));
    assert!(defs.is_empty());
    defs.define_clock(clock_spec(CLOCK)).unwrap();
    defs.define_clock(clock_spec(2)).unwrap();
    let seg =
        [ww_tape::MapSegment::new(0, None, 0, ww_tape::Ratio64::new(1, 1).unwrap(), 0).unwrap()];
    let map = Describe::Map(ww_tape::ClockMap {
        from: ClockNum::new(CLOCK),
        to: ClockNum::new(2),
        segments: RefVec::Slice { slice: &seg },
    });
    assert_eq!(
        defs.define_all(&[map.clone(), track(1, CLOCK)]),
        Err(DescribeError::Numbered)
    );
    assert_eq!(defs.len(), 2, "the map is not held");
    assert_eq!(defs.define_all(core::slice::from_ref(&map)), Ok(()));
    assert_eq!(defs.len(), 3);
}

/// The definitions say how much they hold, for the application to bound; the producer and the layout are the
/// writer's own, not definitions to hold.
#[test]
fn definitions_report_their_size() {
    let mut defs = Definitions::new();
    assert!(defs.is_empty());
    crate::definitions::hold(&mut defs, &[clock(CLOCK), track(1, CLOCK)]).unwrap();
    assert_eq!(defs.len(), 2);
    let bytes = defs.bytes();
    assert!(bytes > 0);
    crate::definitions::hold(
        &mut defs,
        core::slice::from_ref(&labeled(1, CLOCK, "a longer label than before")),
    )
    .unwrap();
    assert_eq!(defs.len(), 2);
    assert!(defs.bytes() > bytes);
    for d in [
        Describe::Producer(Producer { name: "p" }),
        Describe::Layout(Layout::UNALIGNED),
    ] {
        assert_eq!(
            crate::definitions::hold(&mut defs, core::slice::from_ref(&d)),
            Err(DescribeError::WriterOwned)
        );
    }
}

/// Definitions that don't hold together (tracks on a clock not held) are refused, with none of them held; a caller's
/// block of definitions is refused (they are held and put by the writer), and so is a block of data on a track not
/// held. The piece goes on and ends without them; a deferred block on a track not held is refused when deferred, the
/// good ones still placed (writing side point 1).
#[test]
fn definitions_that_do_not_hold_together_are_refused() {
    let mut w = begin(options(u64::MAX));
    write_records(&mut w, 0, 3);
    let tape = w.sink().tape();
    let len = tape.to_vec().unwrap().len();
    let next = w.next_seq();
    for e in [
        w.define(&track(2, 9)).unwrap_err(),
        w.define_all(&[track(2, 9), track(3, 9)]).unwrap_err(),
    ] {
        assert_eq!(
            e,
            DescribeError::UnknownClock {
                clock: ClockNum::new(9)
            }
        );
    }
    assert!(w.defs.track(TrackNum::new(2)).is_none());

    // a caller's block of definitions, through write_block
    let item = ww_tape::open::encode(&track(2, CLOCK)).unwrap();
    let mut buf = vec![0u8; 4096];
    let mut b = BlockWriter::new(&mut buf, BlockKind::Records, w.next_id(), &[]).unwrap();
    b.push_describe(&[&item]).unwrap();
    let n = b.finish();
    assert!(matches!(
        w.write_block(&buf[..n]),
        Err(WriteError::Describe(DescribeError::DefinitionBlock))
    ));
    assert!(matches!(
        w.write_block(&on_tracks(w.next_id(), &[(2, 0, b"x")])),
        Err(WriteError::Describe(DescribeError::UnknownTrack {
            track: TRACK_2
        }))
    ));
    assert_eq!(tape.to_vec().unwrap().len(), len, "nothing written");
    assert_eq!(w.next_seq(), next);
    write_records(&mut w, 10, 3);
    w.close_piece().unwrap();
    let bytes = tape.to_vec().unwrap();
    let r = TapeReader::open(&bytes[..]).unwrap();
    assert!(r.verify(0).map(|v| v.holds).unwrap());
    assert_eq!(tracks(&r.definitions_at(0, u64::MAX).unwrap()), [1]);

    // an aligned writer: a deferred block on a track not held is refused, a good one is placed at the close
    let mut w = Rig::new(TapeWriter::create_on(MemSink::new(), SyncPolicy::None));
    w.set_alignment(ww_tape::ALIGNMENT).unwrap();
    define_all(&mut w, &base(Layout::ALIGNED));
    let tape = w.sink().tape();
    assert!(matches!(
        w.defer_block(on_tracks(w.next_id(), &[(20, 0, b"x")])),
        Err(WriteError::Describe(DescribeError::UnknownTrack {
            track: TRACK_20
        }))
    ));
    w.defer_block(on_tracks(w.next_id(), &[(2, 0, b"deferred")]))
        .unwrap();
    w.close_piece().unwrap();
    let bytes = tape.to_vec().unwrap();
    assert!(
        record_blocks(&bytes)
            .iter()
            .any(|b| b.first_track == Some(2))
    );
}

/// A resumed piece of a chain takes the application's definitions as before: data on their tracks goes on, each
/// definition put again in the piece before the data that needs it.
#[test]
fn a_resumed_piece_takes_the_definitions_passed() {
    let dir = crate::pieces::scratch("definitions_resume");
    let o = options(u64::MAX);
    let opts = ww_tape::ChainOptions {
        tapes: ww_tape::PieceTapes::Each,
        writer: o,
        cut: ww_tape::CutPolicy::default(),
    };
    let owner = OwnerId([6; 32]);
    let defs = held(&[clock(CLOCK), track(5, CLOCK), track(9, CLOCK)]);
    let write = |c: &mut ww_tape::PieceChain<ww_tape_fs::Files>,
                 defs: &Definitions,
                 track: u32,
                 from: i64| {
        for k in from..from + 2 {
            let w = c.writer().unwrap();
            let b = on_tracks(w.next_id(), &[(track, k, b"data")]);
            c.write_block(defs, &b).unwrap();
        }
    };
    let mut c = ww_tape_fs::chain::create(&dir, owner, 0, &[], opts.clone()).unwrap();
    write(&mut c, &defs, 5, 0);
    c.cut_now(&defs).unwrap();
    write(&mut c, &defs, 9, 10);
    c.cut_now(&defs).unwrap();
    write(&mut c, &defs, 9, 20);
    // a crash: the last piece is left unclosed
    drop(c);
    let (mut c, _) = ww_tape_fs::chain::resume(&dir, owner, 0, opts).unwrap();
    write(&mut c, &defs, 9, 30);
    write(&mut c, &defs, 5, 40);
    let pieces = c.close().unwrap();
    let bytes = std::fs::read(ww_tape_fs::Files::new(&dir).path_of(pieces[2].tape)).unwrap();
    let r = TapeReader::open(&bytes[..]).unwrap();
    for b in record_blocks(&bytes)
        .iter()
        .filter(|b| !b.repeat && b.first_track.is_some_and(|t| t > 0))
    {
        let t = b.first_track.unwrap();
        let found = r
            .state_at(
                0,
                Slot::Track {
                    number: TrackNum::new(t),
                },
                b.offset,
            )
            .unwrap();
        assert!(found.item.is_some(), "track {t} at {}", b.offset);
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A typed item is put on track 0 like a state item: written once while it lies close behind.
#[test]
fn a_typed_item_is_put() {
    let mut w = begin(options(u64::MAX));
    let item = TypedItem {
        ty: TypeRef {
            bundle: RefVec::Slice { slice: &[] },
            krate: FullVersion::new("eventfull", Version::new(0, 4, 0)),
            path: "run::RunRecord",
        },
        value: RefVec::Slice { slice: &[1, 2, 3] },
    };
    assert!(w.put_item(&item).unwrap());
    assert!(!w.put_item(&item).unwrap());
    let huge = vec![0u8; STATE_ITEM_MAX];
    let too_long = TypedItem {
        value: RefVec::Slice { slice: &huge },
        ..item.clone()
    };
    // longer than a block holds: it doesn't even encode
    assert!(w.put_item(&too_long).is_err());
    assert!(w.define(&Describe::Item(too_long)).is_err());
}

/// A [ByteSource] that counts the bytes read from it.
struct Counting<'a> {
    bytes: &'a [u8],
    read: Cell<u64>,
}

impl ByteSource for Counting<'_> {
    type Error = ww_tape::reader::OutOfRange;

    fn size(&self) -> Result<u64, Self::Error> {
        self.bytes.size()
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), Self::Error> {
        self.read.set(self.read.get() + buf.len() as u64);
        self.bytes.read_at(offset, buf)
    }
}

/// 10 000 tracks defined one at a time, each with a block of data after it: each define writes only what is new, so
/// the piece grows by about one definition and one data block per track; the reader gives them all back, reading
/// each block of definitions once (reading side point 5).
#[test]
fn ten_thousand_tracks_one_at_a_time() {
    const TRACKS: u32 = 10_000;
    let mut w = begin(options(u64::MAX));
    let tape = w.sink().tape();
    write_records(&mut w, 0, 1);
    let start = tape.to_vec().unwrap().len();
    for n in 2..TRACKS + 2 {
        w.define(&track(n, CLOCK)).unwrap();
        w.write_block(&on_tracks(w.next_id(), &[(n, i64::from(n), b"x")]))
            .unwrap();
    }
    let grown = tape.to_vec().unwrap().len() - start;
    // a block of definitions and a data block per track are about 200 bytes; checkpoints add an entry for each
    let per_track = 200 + 2 * ww_tape::index::ENTRY_LEN;
    assert!(grown < TRACKS as usize * per_track, "{grown} bytes");
    w.close_piece().unwrap();
    let bytes = tape.to_vec().unwrap();
    let src = Counting {
        bytes: &bytes,
        read: Cell::new(0),
    };
    let r = TapeReader::open(&src).unwrap();
    let opened = src.read.get();
    let defs = r.definitions_at(0, u64::MAX).unwrap();
    assert_eq!(tracks(&defs).len(), TRACKS as usize + 1);
    assert!(defs.is_whole());
    let read = src.read.get() - opened;
    assert!(
        read < bytes.len() as u64,
        "{read} bytes read of {}",
        bytes.len()
    );
}

/// One block of data on 2000 tracks: what they need is put once each before it, their shared clock at most once
/// (writing side review 2, point 9).
#[test]
fn a_block_on_many_tracks_puts_each_definition_once() {
    const TRACKS: u32 = 2000;
    let mut w = begin_with(
        options(u64::MAX),
        Layout::UNALIGNED.with_definitions(NEAR_2K),
    );
    let tape = w.sink().tape();
    let defs: Vec<Describe<'_>> = (2..TRACKS + 2).map(|n| track(n, CLOCK)).collect();
    w.define_all(&defs).unwrap();
    // the definitions made lie more than 2 KiB behind
    write_records(&mut w, 0, 100);
    let before = tape.to_vec().unwrap().len() as u64;
    let mut buf = vec![0u8; 1 << 20];
    let mut b = BlockWriter::new(
        &mut buf,
        BlockKind::Records,
        w.next_id(),
        &[ClockNum::new(CLOCK)],
    )
    .unwrap();
    for n in 2..TRACKS + 2 {
        b.push(
            TrackNum::new(n),
            ClockTime::new(ClockNum::new(CLOCK), 1),
            &[b"x"],
        )
        .unwrap();
    }
    let len = b.finish();
    w.write_block(&buf[..len]).unwrap();
    let bytes = tape.to_vec().unwrap();
    let put: Vec<Slot> = puts(&bytes)
        .into_iter()
        .filter(|(at, _)| *at >= before)
        .flat_map(|(_, slots)| slots)
        .collect();
    let mut unique = put.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), put.len(), "a slot put twice");
    assert!(put.len() > TRACKS as usize / 2, "{} put", put.len());
}

/// A definition is skipped only when it lies within `near` of the data, not of where the puts before the data
/// start: one block on 60 tracks puts their definitions (over 2 KiB), so track 2's, written just before them, is put
/// again (writing side review 2, point 11).
#[test]
fn near_is_measured_to_the_data() {
    let defs: Vec<Describe<'_>> = (10..70).map(|n| track(n, CLOCK)).collect();
    let mut w = begin_numbered(
        options(u64::MAX),
        Layout::UNALIGNED.with_definitions(NEAR_2K),
        &[&defs[..], &[track(2, CLOCK)]].concat(),
    );
    let tape = w.sink().tape();
    w.define_all(&defs).unwrap();
    write_records(&mut w, 0, 100);
    w.define(&track(2, CLOCK)).unwrap();
    let mut on: Vec<(u32, i64, &[u8])> = vec![(2, 1, b"x")];
    on.extend((10..70).map(|n| (n, 1, b"x".as_slice())));
    let before = tape.to_vec().unwrap().len() as u64;
    w.write_block(&on_tracks(w.next_id(), &on)).unwrap();
    let bytes = tape.to_vec().unwrap();
    let data = record_blocks(&bytes)
        .into_iter()
        .rfind(|b| b.first_track == Some(2) && !b.repeat)
        .unwrap()
        .offset;
    let newest = writes_of(
        &bytes,
        Slot::Track {
            number: TrackNum::new(2),
        },
    )
    .into_iter()
    .rfind(|at| *at < data)
    .unwrap();
    assert!(newest >= before, "track 2 put again before the data");
}

/// Events track `number` on [CLOCK] declaring `relations`.
fn related(number: u32, relations: &'static [RelationDef<'static>]) -> Describe<'static> {
    let Describe::Track(t) = track(number, CLOCK) else {
        unreachable!("track() gives a track")
    };
    Describe::Track(TrackDef {
        relations: RefVec::Slice { slice: relations },
        owner: [0; 32],
        ..t
    })
}

/// A relation named `name`.
const fn rel(
    name: &'static str,
    values: RelationValues,
    monotone: bool,
    index: RelationIndex,
) -> RelationDef<'static> {
    RelationDef {
        name,
        target: Some(RelationTarget {
            run: [7; 16],
            track: TrackId::new(0x5eed),
        }),
        values,
        monotone,
        index,
    }
}

const SOURCE: RelationDef<'static> =
    rel("source", RelationValues::Stored, true, RelationIndex::Range);
const CONTEXT: RelationDef<'static> =
    rel("context", RelationValues::None, false, RelationIndex::None);

/// A track's relations round-trip and are checked by themselves: names not empty and unique, values only with a
/// meaning (monotone, an index), one range index on a monotone relation; their targets name runs, not open tracks.
/// Reserved kinds read, and a writer's definitions refuse them.
#[test]
fn a_tracks_relations_are_checked_by_themselves() {
    let decoded = related(3, &[SOURCE, CONTEXT]);
    let bytes = ww_tape::open::encode(&decoded).unwrap();
    assert_eq!(Describe::from_ww_bytes(&bytes).unwrap(), decoded);

    let check = |batch: &[Describe<'static>]| {
        let items: Vec<Vec<u8>> = batch
            .iter()
            .map(|d| ww_tape::open::encode(d).unwrap())
            .collect();
        Definitions::new().check_batch(batch.iter().zip(items.iter().map(Vec::as_slice)))
    };
    let with = |d: Describe<'static>| vec![clock(CLOCK), d];
    // the target is another run's track: nothing in this file needs to hold it
    assert_eq!(check(&with(decoded.clone())), Ok(()));

    let shape = Err(DescribeError::Relations {
        track: TrackNum::new(3),
    });
    const REFUSED: [(&[RelationDef<'static>], &str); 7] = [
        (
            &[rel("", RelationValues::Stored, false, RelationIndex::None)],
            "an empty name",
        ),
        (&[SOURCE, SOURCE], "a name twice"),
        (
            &[rel("c", RelationValues::None, true, RelationIndex::None)],
            "monotone without values",
        ),
        (
            &[rel(
                "c",
                RelationValues::None,
                false,
                RelationIndex::Postings,
            )],
            "an index without values",
        ),
        (
            &[rel(
                "k",
                RelationValues::IndexOnly,
                false,
                RelationIndex::None,
            )],
            "index-only values without an index",
        ),
        (
            &[rel(
                "s",
                RelationValues::Stored,
                false,
                RelationIndex::Range,
            )],
            "a range on values that may decrease",
        ),
        (
            &[
                SOURCE,
                rel("parent", RelationValues::Stored, true, RelationIndex::Range),
            ],
            "two range indexes",
        ),
    ];
    for (relations, why) in REFUSED {
        assert_eq!(check(&with(related(3, relations))), shape, "{why}");
    }
    const MANY: [RelationDef<'static>; 17] = {
        let names = [
            "r0", "r1", "r2", "r3", "r4", "r5", "r6", "r7", "r8", "r9", "r10", "r11", "r12", "r13",
            "r14", "r15", "r16",
        ];
        let mut out = [CONTEXT; 17];
        let mut i = 0;
        while i < 17 {
            out[i] = rel(names[i], RelationValues::None, false, RelationIndex::None);
            i += 1;
        }
        out
    };
    assert_eq!(
        check(&with(related(3, &MANY[..16]))),
        Ok(()),
        "16 relations"
    );
    assert_eq!(check(&with(related(3, &MANY))), shape, "17 relations");

    // a name of RELATION_NAME_MAX bytes at most
    const LONGEST: &[RelationDef<'static>] = &[rel(
        "0123456789012345678901234567890123456789012345678901234567890123",
        RelationValues::None,
        false,
        RelationIndex::None,
    )];
    const LONGER: &[RelationDef<'static>] = &[rel(
        "01234567890123456789012345678901234567890123456789012345678901234",
        RelationValues::None,
        false,
        RelationIndex::None,
    )];
    assert_eq!(LONGEST[0].name.len(), ww_tape::RELATION_NAME_MAX);
    assert_eq!(check(&with(related(3, LONGEST))), Ok(()));
    assert_eq!(
        check(&with(related(3, LONGER))),
        Err(DescribeError::RelationName {
            track: TrackNum::new(3),
            len: 65
        })
    );
    let Describe::Track(t) = related(3, LONGER) else {
        unreachable!("related() gives a track")
    };
    assert!(!t.relations_canonical());

    // reserved kinds: read and checked, but not defined by a writer
    const KEYS: &[RelationDef<'static>] = &[rel(
        "entity.piece",
        RelationValues::IndexOnly,
        false,
        RelationIndex::Postings,
    )];
    assert_eq!(check(&with(related(3, KEYS))), Ok(()));
    let mut defs = Definitions::with_numbers(numbers_of(&[clock(CLOCK), decoded.clone()]));
    crate::definitions::hold(&mut defs, core::slice::from_ref(&clock(CLOCK))).unwrap();
    assert_eq!(
        crate::definitions::hold(&mut defs, core::slice::from_ref(&related(3, KEYS))),
        Err(DescribeError::RelationNotBuilt {
            track: TrackNum::new(3)
        })
    );
    assert_eq!(
        crate::definitions::hold(&mut defs, core::slice::from_ref(&decoded)),
        Ok(())
    );
}

/// A track's relations are not part of its identity: its id is the same with any relations or none.
#[test]
fn relations_leave_the_track_id_unchanged() {
    let id = |d: Describe<'static>| {
        let Describe::Track(t) = d else {
            unreachable!("a track")
        };
        t.id()
    };
    let plain = id(track(3, CLOCK));
    assert_eq!(id(related(3, &[])), plain);
    assert_eq!(id(related(3, &[SOURCE])), plain);
    assert_eq!(id(related(3, &[SOURCE, CONTEXT])), plain);
    assert_ne!(id(track(4, CLOCK)), plain, "the key is part of it");
}

/// An events chunk's relation columns match its track's stored relations: one each, none other, a monotone
/// relation's in the starts codec; a relation without values or with values elsewhere takes no column.
#[test]
fn relation_columns_match_the_stored_relations() {
    const RELATIONS: &[RelationDef<'static>] = &[
        SOURCE,
        CONTEXT,
        rel("key", RelationValues::Stored, false, RelationIndex::None),
    ];
    let mut defs = Definitions::with_numbers(numbers_of(&[clock(CLOCK), related(3, RELATIONS)]));
    crate::definitions::hold(&mut defs, core::slice::from_ref(&clock(CLOCK))).unwrap();
    crate::definitions::hold(&mut defs, core::slice::from_ref(&related(3, RELATIONS))).unwrap();
    let col = |param: u16, codec: u8| ww_tape::ColumnSpec {
        kind: ww_tape::ColumnKind::Relation,
        codec: ww_tape::Codec(codec),
        param,
        len: 8,
    };
    let starts = ww_tape::ColumnSpec {
        kind: ww_tape::ColumnKind::Starts,
        codec: ww_tape::Codec(1),
        param: 0,
        len: 8,
    };
    let check = |cols: &[ww_tape::ColumnSpec]| defs.check_relation_columns(TrackNum::new(3), cols);
    let bad = Err(DescribeError::RelationColumns {
        track: TrackNum::new(3),
    });
    assert_eq!(check(&[starts, col(0, 1), col(2, 2)]), Ok(()));
    assert_eq!(
        check(&[starts, col(0, 1), col(2, 1)]),
        Ok(()),
        "any values in codec 1"
    );
    assert_eq!(
        check(&[starts, col(0, 1)]),
        bad,
        "a stored relation without its column"
    );
    assert_eq!(
        check(&[starts, col(0, 1), col(2, 2), col(1, 2)]),
        bad,
        "a column of a relation without values"
    );
    assert_eq!(
        check(&[starts, col(0, 1), col(2, 2), col(2, 2)]),
        bad,
        "a relation's column twice"
    );
    assert_eq!(
        check(&[starts, col(0, 2), col(2, 2)]),
        bad,
        "a monotone relation in codec 2"
    );
    assert_eq!(
        check(&[starts, col(0, 1), col(2, 2), col(16, 2)]),
        bad,
        "a relation past the last"
    );
}

/// TrackDef as main wrote it before `owner` and `relations` (ww_tape main 344485c): the skew test's older side.
#[derive_shrink_wrap(borrowed, derive(Debug, PartialEq))]
struct TrackDefBeforeRelations<'i> {
    number: UNib32,
    key: u64,
    clock: UNib32,
    dimensions: RefVec<'i, ww_tape::Dimension<'i>>,
    items: ww_tape::Items<'i>,
    label: &'i str,
    repeat: TrackRepeat,
    kind: TrackKind<'i>,
}

/// A TrackDef written before `owner` and `relations` reads with both at their defaults and the same id; one written
/// with them reads as the older type with every older field as written.
#[test]
fn a_track_def_reads_across_the_relations_change() {
    let Describe::Track(new) = labeled(5, CLOCK, "skew") else {
        unreachable!("labeled() gives a track")
    };
    let old = TrackDefBeforeRelations {
        number: UNib32(new.number.get()),
        key: new.key.get(),
        clock: UNib32(new.clock.get()),
        dimensions: new.dimensions.clone(),
        items: new.items.clone(),
        label: new.label,
        repeat: new.repeat,
        kind: new.kind.clone(),
    };
    let mut scratch = [0u8; 512];
    let old_bytes = old.to_ww_bytes(&mut scratch).unwrap().to_vec();
    let read = TrackDef::from_ww_bytes(&old_bytes).expect("older bytes read as the new type");
    assert_eq!(read, new);
    assert_eq!(read.owner, [0; 32]);
    assert_eq!(read.relations.len(), 0);
    assert_eq!(read.id(), new.id());

    let relations = [RelationDef {
        name: "source",
        target: Some(RelationTarget {
            run: [3; 16],
            track: TrackId::new(9),
        }),
        values: RelationValues::Stored,
        monotone: true,
        index: RelationIndex::Range,
    }];
    let with = TrackDef {
        owner: [7; 32],
        relations: RefVec::Slice { slice: &relations },
        ..new.clone()
    };
    assert_eq!(
        with.id(),
        new.id(),
        "owner and relations are not the identity"
    );
    let mut scratch = [0u8; 512];
    let new_bytes = with.to_ww_bytes(&mut scratch).unwrap().to_vec();
    assert_eq!(
        TrackDefBeforeRelations::from_ww_bytes(&new_bytes)
            .expect("newer bytes read as the older type"),
        old
    );
}
