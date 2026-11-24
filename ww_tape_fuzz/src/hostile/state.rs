//! State records and their index entries: a piece a writer wrote with puts, copies and definitions made, then its
//! blocks of state records and the state entries of its index blocks (slot code, number, hash, offset, length) take
//! hostile values behind good CRCs, or the piece is cut.

use std::sync::OnceLock;
use ww_tape::{ClockNum, TrackNum};

use super::index::reseal_index;
use super::{Draw, Field, HEADER_FIELDS, get, hit, hit_some, seal};
use shrink_wrap::prelude::{RefVec, UNib32};
use ww_tape::index::{ENTRY_LEN, IndexPolicy, STATE_TRACK_ID};
use ww_tape::writer::NearPolicy;
use ww_tape::{
    BlockKind, ClockOrigin, ClockSpec, ClockTime, Definitions, EventsDef, FileBlocks, FullVersion,
    Layout, Magnitude, MemSink, Recovery, Segment, StatePolicy, SyncPolicy, TapeWriter, TrackKind,
    TrackRepeat, TrackSpec, Version, clock_key, track_key,
};

/// The number of the clock of the template's tracks.
pub(crate) const CLOCK: u32 = 1;
/// The number of the template's stream track.
pub(crate) const STREAM: u32 = 1;
/// The numbers of the template's state tracks, which the hostile values of a state entry's number name.
pub(crate) const STATES: [u32; 2] = [5, 6];
/// Rounds of puts and records in the template.
const ROUNDS: i64 = 24;

/// A policy of `near` bytes and `copies` copies.
const fn policy(near: u32, copies: u8) -> StatePolicy {
    StatePolicy {
        near: exact_units::Bytes::new(near),
        copies: exact_units::Count::new(copies),
    }
}

/// The writer's options for the template and for a resume of it: checkpoints every 1 KiB, copies 512 bytes apart,
/// a memory of 4 slots, so it forgets; the gaps of an SD card, so a raw medium reads past damage.
pub(crate) fn options() -> ww_tape::WriterOptions {
    ww_tape::WriterOptions {
        sync: SyncPolicy::None,
        clock: ww_tape::ClockRef::NONE,
        index: IndexPolicy {
            bytes: exact_units::Bytes::new(1024),
            interval: None,
            ..IndexPolicy::default()
        },
        layout: Layout::UNALIGNED
            .on_sd_card()
            .with_definitions(policy(1024, 2)),
        producer: "state".to_owned(),
        integrity: ww_tape::Integrity::Full,
        near: NearPolicy {
            copy_spacing: exact_units::Bytes::new(512),
            memory_max: exact_units::Count::new(4),
            ..NearPolicy::default()
        },
    }
}

/// Track `index` on clock `clock`, labeled `label`, a state track of `state` or a stream: its key is the index's,
/// the number the template gives it.
fn track(
    index: u32,
    clock: ClockNum,
    label: &'static str,
    state: Option<StatePolicy>,
) -> TrackSpec<'static> {
    TrackSpec {
        key: track_key(0x57, &[Segment::Index(index)]),
        clock,
        dimensions: RefVec::Slice { slice: &[] },
        items: ww_tape::Items::WireWeaver {
            api: FullVersion::new("state_api", Version::new(0, 1, 0)),
            bundle: RefVec::Slice { slice: &[] },
            path: RefVec::Slice {
                slice: &[UNib32(1)],
            },
            keys: RefVec::Slice { slice: &[] },
            item: None,
        },
        label,
        repeat: state.map_or(TrackRepeat::Stream, TrackRepeat::State),
        kind: TrackKind::Events(EventsDef::INSTANTS),
        relations: RefVec::Slice { slice: &[] },
        owner: [0; 32],
        defined_twice: false,
    }
}

/// The stream track labeled `label`.
fn stream(clock: ClockNum, label: &'static str) -> TrackSpec<'static> {
    track(STREAM, clock, label, None)
}

/// The numbers the template's data is written on.
#[derive(Clone, Copy, Debug)]
pub(crate) struct StateTracks {
    pub(crate) clock: ClockNum,
    pub(crate) stream: TrackNum,
    pub(crate) states: [TrackNum; 2],
}

/// The definitions the template holds: the clock, the stream track and the state tracks, under [CLOCK], [STREAM]
/// and [STATES] fresh, else numbered as the piece `resumed` read numbers them. The producer and the layout
/// (definitions put again 1 KiB on, two copies) are the writer's, in [options].
pub(crate) fn definitions(resumed: Option<&ww_tape::Resumed>) -> (Definitions, StateTracks) {
    let mut defs = resumed.map_or_else(Definitions::new, |r| {
        Definitions::with_numbers(r.numbers.clone())
    });
    let clock = defs
        .define_clock(ClockSpec {
            key: clock_key(0x57, &[], 1),
            rate: Magnitude::pow10(9).into(),
            resolution: Magnitude::pow10(-9).into(),
            origin: ClockOrigin::Boot,
            label: "host",
        })
        .expect("the clock holds");
    let stream = defs
        .define_track(stream(clock, "stream"))
        .expect("the track holds");
    let states = [
        (STATES[0], "near", policy(512, 1)),
        (STATES[1], "far", policy(4096, 2)),
    ]
    .map(|(index, label, p)| {
        let t = track(index, clock, label, Some(p));
        match resumed {
            Some(_) => defs.define_track(t).expect("the track holds"),
            None => crate::targets::define_track_as(&mut defs, &t, TrackNum::new(index)),
        }
    });
    if resumed.is_none() {
        assert_eq!(
            (clock.get(), stream.get()),
            (CLOCK, STREAM),
            "the template's numbers"
        );
    }
    (
        defs,
        StateTracks {
            clock,
            stream,
            states,
        },
    )
}

/// A records block of the stream track at tick `k`.
pub(crate) fn records(id: ww_tape::BlockId, on: StateTracks, k: i64) -> Vec<u8> {
    let mut buf = [0u8; 128];
    let mut b = ww_tape::BlockWriter::new(&mut buf, BlockKind::Records, id, &[on.clock])
        .expect("a block fits");
    b.push(on.stream, ClockTime::new(on.clock, k), &[&[k as u8; 24]])
        .expect("a record fits");
    let len = b.finish();
    buf[..len].to_vec()
}

/// The template: rounds of puts of a few items on the state tracks, a records block each, the stream track's
/// label changed now and then; closed.
fn template() -> Vec<u8> {
    let o = options();
    let mut w = TapeWriter::create_on(MemSink::new(), o.sync);
    w.set_index_policy(o.index)
        .expect("a policy with room for a piece head");
    w.set_near_policy(o.near);
    w.set_layout(o.layout).expect("a new piece takes a layout");
    w.set_producer(&o.producer);
    let (mut defs, on) = definitions(None);
    let labels = ["stream", "stream, again", "stream, once more"];
    for k in 0..ROUNDS {
        if k % 7 == 6 {
            let again = defs
                .define_track(stream(on.clock, labels[(k / 7) as usize % 3]))
                .expect("a label changes");
            assert_eq!(again, on.stream, "the same track keeps its number");
        }
        w.put(&defs, on.states[0], k, &[(k % 3) as u8; 12])
            .expect("a put");
        w.put(&defs, on.states[1], k, &[(k / 8) as u8 | 0x80; 40])
            .expect("a put");
        w.write_block(&defs, &records(w.next_id(), on, k))
            .expect("a records block");
    }
    let tape = w.sink().tape();
    w.close().expect("the template closes");
    tape.to_vec().expect("the bytes")
}

/// The template, made once.
fn template_bytes() -> &'static [u8] {
    static T: OnceLock<Vec<u8>> = OnceLock::new();
    T.get_or_init(template)
}

/// The blocks of `f`: each one's offset, length and kind.
fn blocks(f: &[u8]) -> Vec<(usize, usize, BlockKind)> {
    let mut rd = FileBlocks::with_recovery(f, Recovery::Prefix);
    let mut out = vec![];
    while let Some(Ok(b)) = rd.next_block() {
        let (len, kind) = (b.len(), b.kind);
        out.push((rd.offset() as usize, len, kind));
    }
    out
}

/// The fields of a state entry at `at` an attack takes: offset, length, slot code, number, a map's to clock, the
/// reserved item number, the two halves of the hash, the record's ticks.
fn state_entry_fields(at: usize) -> [Field<'static>; 9] {
    [
        Field::new(at, 8, &[ww_tape::MAX_BLOCK_LEN as u64]),
        Field::new(at + 8, 4, &[ww_tape::MAX_BLOCK_LEN as u64]),
        Field::new(at + 13, 1, &[0, 1, 4, 5, 7, 8, 15]),
        Field::new(at + 24, 4, &[5, 6]),
        Field::new(at + 28, 4, &[1]),
        Field::new(at + 32, 8, &[1, 2, 4]),
        Field::new(at + 48, 8, &[]),
        Field::new(at + 56, 8, &[]),
        Field::new(at + 64, 8, &[]),
    ]
}

/// Hostile values over one state entry of the index block `b`, its CRCs computed again.
fn hit_state_entry(d: &mut Draw<'_>, b: &mut [u8]) {
    let entries = get(b, 12, 4) as usize;
    let entries_at = get(b, 32, 4) as usize;
    let states: Vec<usize> = (0..entries)
        .map(|e| entries_at + e * ENTRY_LEN)
        .filter(|at| get(b, at + 16, 8) == STATE_TRACK_ID.get())
        .collect();
    if let Some(at) = states.get(d.below(states.len().max(1))) {
        hit_some(d, b, &state_entry_fields(*at));
    }
    reseal_index(b);
}

/// Hostile values over a block of state records `b`: a header field, or a byte of its body; its CRC computed again,
/// so the record lies behind a good CRC.
fn hit_records(d: &mut Draw<'_>, b: &mut [u8]) {
    if d.bool() {
        let f = HEADER_FIELDS[d.below(HEADER_FIELDS.len())];
        hit(d, b, f);
    } else if b.len() > 36 {
        let at = 32 + d.below(b.len() - 36);
        b[at] ^= d.u8() | 1;
    }
    seal(b);
}

/// A hostile piece of [state] and what was done to it.
pub struct Attacked {
    pub bytes: Vec<u8>,
    /// An index block's state entry or a block of state records took hostile values behind a good CRC: what the
    /// piece says may lie. Else every attack is damage a CRC finds, or a cut.
    pub forged: bool,
}

/// Whether the block at `at`, `len` bytes, of `f` is a block of puts that belongs with the block after it.
fn with_next(f: &[u8], at: usize, len: usize) -> bool {
    ww_tape::Block::read(&f[at..at + len]).is_ok_and(|b| b.flags & ww_tape::FLAG_WITH_NEXT != 0)
}

/// The template with one to four attacks drawn from `data`, then, by the input, cut.
///
/// An attack hits its index blocks' state entries or its blocks of state records (hostile values behind a good CRC, or
/// damage), or cuts right after puts that belong with the block the cut leaves out.
pub fn state(data: &[u8]) -> Vec<u8> {
    attacked(data).bytes
}

/// The template, never attacked: what a lookup of an attacked piece is checked against.
pub fn state_template() -> &'static [u8] {
    template_bytes()
}

/// [state], saying whether an index block was attacked.
pub fn attacked(data: &[u8]) -> Attacked {
    let mut d = Draw::new(data);
    let mut f = template_bytes().to_vec();
    let mut forged = false;
    let all = blocks(&f);
    let indexes: Vec<_> = all.iter().filter(|b| b.2 == BlockKind::Index).collect();
    let states: Vec<_> = all
        .iter()
        .filter(|(at, len, kind)| {
            *kind == BlockKind::Records
                && ww_tape::Block::read(&f[*at..at + len]).is_ok_and(|b| {
                    b.is_repeat() || b.first_track() == Some(ww_tape::RecordTrack::Definitions)
                })
        })
        .collect();
    let leading: Vec<_> = all
        .iter()
        .filter(|(at, len, _)| with_next(&f, *at, *len))
        .collect();
    for _ in 0..1 + d.below(4) {
        // 0: an index's state entry, 1: a block of state records behind a good CRC, 2: damage to one, 3: the end
        // right after puts that belong with the next block
        let attack = d.below(4);
        if attack == 3 {
            if let Some((at, len, _)) = leading.get(d.below(leading.len().max(1))) {
                f.truncate(at + len);
                break;
            }
            continue;
        }
        let pick = if attack == 0 { &indexes } else { &states };
        let Some((at, len, _)) = pick.get(d.below(pick.len().max(1))) else {
            continue;
        };
        let b = &mut f[*at..at + len];
        match attack {
            0 => hit_state_entry(&mut d, b),
            1 => hit_records(&mut d, b),
            _ => {
                let i = d.below(b.len());
                b[i] ^= d.u8() | 1;
            }
        }
        forged |= attack < 2;
    }
    if d.bool() {
        let cut = d.below(f.len() + 1);
        f.truncate(cut);
    }
    Attacked { bytes: f, forged }
}
