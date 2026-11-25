//! Bytes tracks: a chain of pieces in one file holding values written as parts, some across a cut, one torn, one
//! empty, one coded in lz4; then a part's head, its flags word, a seal's bytes, a part's bytes or an index entry of
//! the track takes hostile values behind good CRCs, a byte is damaged, or the file is cut.

use std::sync::OnceLock;

use super::chunk::{column_start, hit_head, reseal};
use super::index::{entry_fields, reseal_index};
use super::{Draw, get, hit_some};
use shrink_wrap::prelude::RefVec;
use ww_tape::bytes_track::{BytesOptions, BytesPart, BytesWriter, ValueId, WriteValueError};
use ww_tape::index::ENTRY_LEN;
use ww_tape::payloads::PayloadCodec;
use ww_tape::{
    BlockKind, ChainOptions, ClockOrigin, ClockSpec, CutPolicy, Definitions, FileBlocks, ItemNum,
    Items, Magnitude, MemPieces, OwnerId, PieceChain, PieceTapes, Recovery, Segment, SyncPolicy,
    TrackId, TrackKind, TrackNum, TrackRepeat, TrackSpec, WriterOptions, clock_key, track_key,
};

/// Bytes of a part in the template: small, so a value of a few hundred bytes takes several.
const PART: usize = 48;
/// Chunks of a piece before a cut: the template's values cross cuts.
const CUT_CHUNKS: u64 = 5;
/// The owner of the template's chain.
const OWNER: OwnerId = OwnerId([0xb7; 32]);

/// The template: its bytes, its bytes track and each value it holds, with its bytes or, for a torn one, None.
pub struct BytesTemplate {
    pub bytes: Vec<u8>,
    pub track: TrackId,
    pub values: Vec<(ValueId, Option<Vec<u8>>)>,
}

/// The definitions the template holds: a clock and the bytes track.
fn definitions() -> (Definitions, TrackNum) {
    let mut defs = Definitions::new();
    let clock = defs
        .define_clock(ClockSpec {
            key: clock_key(0xb7, &[], 1),
            rate: Magnitude::pow10(9).into(),
            resolution: Magnitude::pow10(-9).into(),
            origin: ClockOrigin::Boot,
            label: "host",
        })
        .expect("the clock holds");
    let track = defs
        .define_track(TrackSpec {
            key: track_key(0xb7, &[Segment::Index(1)]),
            clock,
            dimensions: RefVec::Slice { slice: &[] },
            items: Items::RawBytes,
            label: "documents",
            repeat: TrackRepeat::Stream,
            kind: TrackKind::Bytes,
            relations: RefVec::Slice { slice: &[] },
            owner: [0; 32],
            defined_twice: false,
        })
        .expect("the track holds");
    (defs, track)
}

/// `len` bytes of value `seed`.
fn value(seed: u8, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(29).wrapping_add(seed) ^ (i >> 5) as u8)
        .collect()
}

type Chain = PieceChain<MemPieces>;

/// Write `bytes` as one value at `ticks` with `w` into `c`.
fn put(
    c: &mut Chain,
    defs: &Definitions,
    w: &mut BytesWriter,
    ticks: i64,
    bytes: &[u8],
) -> ValueId {
    w.write_from(ticks, bytes, |p: BytesPart<'_>| {
        c.write_chunk(defs, &p.head, p.columns()).map(drop)
    })
    .map_err(|e: WriteValueError<_>| format!("{e}"))
    .expect("a value writes")
    .id
}

fn template() -> BytesTemplate {
    let options = ChainOptions {
        tapes: PieceTapes::One,
        writer: WriterOptions {
            sync: SyncPolicy::None,
            integrity: ww_tape::Integrity::Full,
            ..WriterOptions::default()
        },
        cut: CutPolicy {
            chunks: exact_units::Count::new(CUT_CHUNKS),
            ..CutPolicy::default()
        },
    };
    let mut c =
        PieceChain::create_on(MemPieces::new(), OWNER, 0, &[], options).expect("a chain starts");
    let (defs, track) = definitions();
    let raw = BytesOptions {
        part_len: PART,
        codec: PayloadCodec::Raw,
    };
    let mut w = BytesWriter::new(&defs, track, ItemNum::new(0), raw).expect("a bytes track");
    let mut values = vec![];
    let v = value(1, 150);
    values.push((put(&mut c, &defs, &mut w, 0, &v), Some(v)));
    values.push((put(&mut c, &defs, &mut w, 1, &[]), Some(vec![])));
    // torn: two parts written, then given up
    let torn = w.begin(2).expect("no value open");
    w.write(&value(2, 2 * PART + 4), |p: BytesPart<'_>| {
        c.write_chunk(&defs, &p.head, p.columns())
            .map(drop)
            .map_err(|e| WriteValueError::<String>::Write(format!("{e}")))
    })
    .expect("parts write");
    w.abandon();
    values.push((torn, None));
    let mut lz4 = BytesWriter::new(
        &defs,
        track,
        w.next_part(),
        BytesOptions {
            part_len: PART,
            codec: PayloadCodec::LZ4,
        },
    )
    .expect("a bytes track");
    let v: Vec<u8> = b"abcdefgh".iter().copied().cycle().take(130).collect();
    values.push((put(&mut c, &defs, &mut lz4, 3, &v), Some(v)));
    let mut w = BytesWriter::new(&defs, track, lz4.next_part(), raw).expect("a bytes track");
    let v = value(4, 97);
    values.push((put(&mut c, &defs, &mut w, 4, &v), Some(v)));
    let mem = c.store().clone();
    c.close().expect("the chain closes");
    let bytes = mem
        .get(&0)
        .expect("memory")
        .expect("the tape")
        .to_vec()
        .expect("the bytes");
    let track = defs.track_identity(track).expect("the track is held").0;
    BytesTemplate {
        bytes,
        track,
        values,
    }
}

/// The template, made once.
pub fn bytes_template() -> &'static BytesTemplate {
    static T: OnceLock<BytesTemplate> = OnceLock::new();
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

/// Hostile values over one entry of the bytes track in the index block `b`, its CRCs computed again.
fn hit_entry(d: &mut Draw<'_>, b: &mut [u8], track: TrackId) {
    let entries = get(b, 12, 4) as usize;
    let entries_at = get(b, 32, 4) as usize;
    let ours: Vec<usize> = (0..entries)
        .map(|e| entries_at + e * ENTRY_LEN)
        .filter(|at| get(b, at + 16, 8) == track.get())
        .collect();
    if let Some(at) = ours.get(d.below(ours.len().max(1))) {
        hit_some(d, b, &entry_fields(*at));
    }
    reseal_index(b);
}

/// The flags word of part `b`: its bits turned, or the Flags column's bytes changed; its CRCs computed again.
fn hit_flags(d: &mut Draw<'_>, b: &mut [u8]) {
    if let Some(at) = column_start(b, 3) {
        let end = b.len().saturating_sub(4);
        let i = at + d.below(end.saturating_sub(at).max(1));
        if i < end {
            b[i] ^= [0x01, 0x02, 0x03, d.u8() | 1][d.below(4)];
        }
    }
    reseal(b, false);
}

/// A byte of part `b`'s bytes, or of a seal's first part, length or hash, changed; its CRCs computed again.
fn hit_payload(d: &mut Draw<'_>, b: &mut [u8]) {
    if let Some(at) = column_start(b, 0) {
        // the Payloads column runs to the PayloadLengths column
        let end = column_start(b, 1).unwrap_or(b.len().saturating_sub(4));
        let i = at + d.below(end.saturating_sub(at).max(1));
        if i < b.len().saturating_sub(4) {
            b[i] ^= d.u8() | 1;
        }
    }
    reseal(b, false);
}

/// A hostile file of [bytes_template] and what was done to it.
pub struct BytesAttacked {
    pub bytes: Vec<u8>,
    /// Nothing was done to the template.
    pub whole: bool,
}

/// The template with up to four attacks drawn from `data`, then, by the input, cut.
pub fn bytes(data: &[u8]) -> Vec<u8> {
    attacked(data).bytes
}

/// [bytes], saying what was done.
pub fn attacked(data: &[u8]) -> BytesAttacked {
    let mut d = Draw::new(data);
    let t = bytes_template();
    let mut f = t.bytes.clone();
    let all = blocks(&f);
    let indexes: Vec<_> = all.iter().filter(|b| b.2 == BlockKind::Index).collect();
    let parts: Vec<_> = all.iter().filter(|b| b.2 == BlockKind::Events).collect();
    let attacks = d.below(5);
    for _ in 0..attacks {
        // 0: an index entry, 1: a part's head, 2: its flags, 3: its bytes, all behind good CRCs; 4: damage
        let attack = d.below(5);
        let pick = if attack == 0 { &indexes } else { &parts };
        let Some((at, len, _)) = pick.get(d.below(pick.len().max(1))) else {
            continue;
        };
        let b = &mut f[*at..at + len];
        match attack {
            0 => hit_entry(&mut d, b, t.track),
            1 => {
                hit_head(&mut d, b);
                reseal(b, d.below(4) == 0);
            }
            2 => hit_flags(&mut d, b),
            3 => hit_payload(&mut d, b),
            _ => {
                let i = d.below(b.len());
                b[i] ^= d.u8() | 1;
            }
        }
    }
    let mut whole = attacks == 0;
    if d.bool() {
        let cut = d.below(f.len() + 1);
        whole &= cut == f.len();
        f.truncate(cut);
    }
    BytesAttacked { bytes: f, whole }
}
