//! Keyed state records and their index entries: a piece a writer wrote with puts and deletions of keys on a keyed
//! state track, then its keyed blocks and the keyed fields of its index's state entries (flags, key length, key head,
//! key hash) take hostile values behind good CRCs, or the piece is cut.

use std::sync::OnceLock;

use super::index::reseal_index;
use super::{Draw, Field, get, hit_some, seal};
use shrink_wrap::prelude::{RefVec, UNib32};
use ww_tape::index::{ENTRY_LEN, IndexPolicy, STATE_TRACK_ID};
use ww_tape::writer::NearPolicy;
use ww_tape::{
    BlockKind, ClockNum, ClockOrigin, ClockSpec, Definitions, EventsDef, FileBlocks, FullVersion,
    Layout, Magnitude, MemSink, Recovery, STATE_KEY_LEN_MAX, Segment, StatePolicy, SyncPolicy,
    TapeWriter, TrackKind, TrackNum, TrackRepeat, TrackSpec, Version, clock_key, track_key,
};

/// Rounds of puts in the template.
const ROUNDS: i64 = 16;

/// The keys the template puts: short ones whole in the index, long ones sharing a head, one at the cap.
pub fn keys() -> &'static [Vec<u8>] {
    static K: OnceLock<Vec<Vec<u8>>> = OnceLock::new();
    K.get_or_init(|| {
        let mut out: Vec<Vec<u8>> = [
            &b""[..],
            b"/a",
            b"/b",
            b"/a/b",
            b"/eventfull/layouts/main",
            b"/eventfull/layouts/side",
            b"/eventfull/x",
            b"\0\0",
        ]
        .iter()
        .map(|k| k.to_vec())
        .collect();
        out.push(vec![b'k'; STATE_KEY_LEN_MAX]);
        out
    })
}

/// Prefixes a listing asks for: none, each key's, past a key head, of no key.
pub fn prefixes() -> &'static [&'static [u8]] {
    &[
        b"",
        b"/",
        b"/a",
        b"/eventfull/",
        b"/eventfull/layouts/",
        b"/eventfull/layouts/m",
        b"/z",
        b"\0",
        b"kkkkkkkkkk",
    ]
}

/// The writer's options for the template and for a resume of it: checkpoints every 1 KiB, copies 512 bytes apart,
/// a memory of 4 slots, so it forgets.
pub(crate) fn options() -> ww_tape::WriterOptions {
    ww_tape::WriterOptions {
        sync: SyncPolicy::None,
        clock: ww_tape::ClockRef::NONE,
        index: IndexPolicy {
            bytes: exact_units::Bytes::new(1024),
            interval: None,
            ..IndexPolicy::default()
        },
        layout: Layout::UNALIGNED.on_sd_card(),
        producer: "keyed".to_owned(),
        integrity: ww_tape::Integrity::Full,
        near: NearPolicy {
            copy_spacing: exact_units::Bytes::new(512),
            memory_max: exact_units::Count::new(4),
            ..NearPolicy::default()
        },
    }
}

/// The numbers the template's keys are put on.
#[derive(Clone, Copy, Debug)]
pub struct KeyedTracks {
    pub clock: ClockNum,
    pub keyed: TrackNum,
}

/// The definitions the template holds: a clock and the keyed track, numbered as the piece `resumed` read numbers
/// them, else fresh.
pub(crate) fn definitions(resumed: Option<&ww_tape::Resumed>) -> (Definitions, KeyedTracks) {
    let mut defs = resumed.map_or_else(Definitions::new, |r| {
        Definitions::with_numbers(r.numbers.clone())
    });
    let clock = defs
        .define_clock(ClockSpec {
            key: clock_key(0x4b, &[], 1),
            rate: Magnitude::pow10(9).into(),
            resolution: Magnitude::pow10(-9).into(),
            origin: ClockOrigin::Boot,
            label: "host",
        })
        .expect("the clock holds");
    let keyed = defs
        .define_track(TrackSpec {
            key: track_key(0x4b, &[Segment::Index(1)]),
            clock,
            dimensions: RefVec::Slice { slice: &[] },
            items: ww_tape::Items::WireWeaver {
                api: FullVersion::new("keyed_api", Version::new(0, 1, 0)),
                bundle: RefVec::Slice { slice: &[] },
                path: RefVec::Slice {
                    slice: &[UNib32(1)],
                },
                keys: RefVec::Slice { slice: &[] },
                item: None,
            },
            label: "store",
            repeat: TrackRepeat::Keyed(StatePolicy {
                near: exact_units::Bytes::new(768),
                copies: exact_units::Count::new(1),
            }),
            kind: TrackKind::Events(EventsDef::INSTANTS),
            relations: RefVec::Slice { slice: &[] },
            owner: [0; 32],
        })
        .expect("the track holds");
    (defs, KeyedTracks { clock, keyed })
}

/// The template: rounds of puts and deletions over [keys], a value per round; closed.
fn template() -> Vec<u8> {
    let o = options();
    let mut w = TapeWriter::create_on(MemSink::new(), o.sync);
    w.set_index_policy(o.index)
        .expect("a policy with room for a piece head");
    w.set_near_policy(o.near);
    w.set_layout(o.layout).expect("a new piece takes a layout");
    w.set_producer(&o.producer);
    let (defs, on) = definitions(None);
    for k in 0..ROUNDS {
        for (i, key) in keys().iter().enumerate() {
            let round = k as usize + i;
            if round % 5 == 4 {
                w.delete_key(&defs, on.keyed, k, key).expect("a deletion");
            } else if round % 3 != 2 {
                let value = vec![(round % 7) as u8; round % 11];
                w.put_key(&defs, on.keyed, k, key, &value).expect("a put");
            }
        }
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

/// The fields of a state entry at `at` a keyed attack takes: offset, length, flags, track, key length, key head,
/// key hash, the hash of the item.
fn keyed_entry_fields(at: usize) -> [Field<'static>; 8] {
    [
        Field::new(at, 8, &[]),
        Field::new(at + 8, 4, &[ww_tape::MAX_BLOCK_LEN as u64]),
        Field::new(at + 15, 1, &[0x20, 0x40, 0x60, 0x21, 0x61]),
        Field::new(at + 24, 4, &[1, 2]),
        Field::new(at + 28, 4, &[0, 8, 9, STATE_KEY_LEN_MAX as u64]),
        Field::new(at + 72, 8, &[]),
        Field::new(at + 80, 8, &[]),
        Field::new(at + 48, 8, &[]),
    ]
}

/// Hostile values over one state entry of the index block `b`, its CRCs computed again.
fn hit_entry(d: &mut Draw<'_>, b: &mut [u8]) {
    let entries = get(b, 12, 4) as usize;
    let entries_at = get(b, 32, 4) as usize;
    let states: Vec<usize> = (0..entries)
        .map(|e| entries_at + e * ENTRY_LEN)
        .filter(|at| get(b, at + 16, 8) == STATE_TRACK_ID.get())
        .collect();
    if let Some(at) = states.get(d.below(states.len().max(1))) {
        hit_some(d, b, &keyed_entry_fields(*at));
    }
    reseal_index(b);
}

/// Hostile bytes in a keyed block `b`'s body (a key head, a length, a key byte), or its flags; its CRC computed
/// again.
fn hit_keyed(d: &mut Draw<'_>, b: &mut [u8]) {
    if d.below(4) == 0 {
        // the flags: keyed without repeat, repeat without keyed
        b[6] ^= [0x02, 0x40, 0x42][d.below(3)];
    } else if b.len() > 36 {
        let at = 32 + d.below(b.len() - 36);
        b[at] = match d.below(3) {
            0 => b[at] ^ 1,
            1 => d.u8(),
            _ => b[at].wrapping_add(2),
        };
    }
    seal(b);
}

/// A hostile piece of [keyed] and what was done to it.
pub struct KeyedAttacked {
    pub bytes: Vec<u8>,
    /// An index entry or a keyed block took hostile values behind a good CRC: what the piece says may lie.
    pub forged: bool,
}

/// The template with one to four attacks drawn from `data`, then, by the input, cut.
pub fn keyed(data: &[u8]) -> Vec<u8> {
    attacked(data).bytes
}

/// The template, never attacked: what a lookup of an attacked piece is checked against.
pub fn keyed_template() -> &'static [u8] {
    template_bytes()
}

/// [keyed], saying whether a block was forged behind a good CRC.
pub fn attacked(data: &[u8]) -> KeyedAttacked {
    let mut d = Draw::new(data);
    let mut f = template_bytes().to_vec();
    let mut forged = false;
    let all = blocks(&f);
    let indexes: Vec<_> = all.iter().filter(|b| b.2 == BlockKind::Index).collect();
    let keyed: Vec<_> = all
        .iter()
        .filter(|(at, len, _)| ww_tape::Block::read(&f[*at..at + len]).is_ok_and(|b| b.is_keyed()))
        .collect();
    for _ in 0..1 + d.below(4) {
        // 0: an index's state entry, 1: a keyed block behind a good CRC, 2: damage to one
        let attack = d.below(3);
        let pick = if attack == 0 { &indexes } else { &keyed };
        let Some((at, len, _)) = pick.get(d.below(pick.len().max(1))) else {
            continue;
        };
        let b = &mut f[*at..at + len];
        match attack {
            0 => hit_entry(&mut d, b),
            1 => hit_keyed(&mut d, b),
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
    KeyedAttacked { bytes: f, forged }
}
