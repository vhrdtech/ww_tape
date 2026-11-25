//! The ww_tape book's settings case: a motor controller's tuning session, kept beside its data.
//!
//! A tuning tool sets the controller's parameters, switches its profile and flashes its firmware while it records.
//! The tape holds three tracks of state and values:
//!
//! | Track | What | Kind in a tape |
//! |---|---|---|
//! | `params` | each parameter by its path, such as `pid/kp`, and its value | keyed state track |
//! | `profile` | the controller's profile: `gentle` or `fast` | state track |
//! | `firmware` | the firmware image flashed, read whole | bytes track |
//!
//! [run] writes the session into a chain of two pieces in memory and reads it back: the parameters in force at the
//! end, the history of one parameter and the firmware image. `cargo run -p example_settings` prints its [Report].
//!
//! ```
//! let report = example_settings::run()?;
//! assert_eq!(report.firmware.len, example_settings::FIRMWARE_LEN);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
#![allow(
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    reason = "the case makes its own firmware bytes and times from constants, far from any limit, and reads no \
              bytes of others"
)]

use core::fmt::{self, Write as _};
use std::error::Error;

use shrink_wrap::prelude::*;
use ww_tape::bytes_track::{BytesOptions, BytesPart, BytesWriter, ValueSeal};
use ww_tape::{
    Block, BlockKind, ChainOptions, ClockOrigin, ClockSpec, Definitions, EventsDef, FLAG_GROUP,
    FLAG_GROUP_START, FLAG_REPEAT, FLAG_TWICE, FLAG_WITH_NEXT, GroupPut, Items, Magnitude,
    MemPieces, OwnerId, PieceChain, PieceTapes, Segment, SyncPolicy, TapeReader, TrackKind,
    TrackNum, TrackRepeat, TrackSpec, WriterOptions, clock_key, track_key,
};

/// The root of the controller's API: every track and clock key starts from it.
const API_ROOT: u64 = 0x5e77_0001;
/// The chain's owner.
const OWNER: OwnerId = OwnerId([0x5e; 32]);
/// Bytes of the firmware image the tool flashes.
pub const FIRMWARE_LEN: u64 = 200_000;
/// Bytes of each part of a value but its last.
pub const PART_LEN: usize = 64 << 10;

/// The numbers the session's definitions gave its tracks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Numbers {
    pub params: TrackNum,
    pub profile: TrackNum,
    pub firmware: TrackNum,
}

/// The session's definitions: one clock and its three tracks.
pub fn definitions() -> Result<(Definitions, Numbers), Box<dyn Error>> {
    let mut defs = Definitions::new();
    let clock = defs.define_clock(ClockSpec {
        key: clock_key(API_ROOT, &[Segment::Name("tool")], 1),
        rate: Magnitude::pow10(3).into(),
        resolution: Magnitude::pow10(-3).into(),
        origin: ClockOrigin::Boot,
        label: "tool ms since start",
    })?;
    let track = |name, repeat, kind| TrackSpec {
        key: track_key(API_ROOT, &[Segment::Name(name)]),
        clock,
        dimensions: RefVec::Slice { slice: &[] },
        items: Items::RawBytes,
        label: name,
        repeat,
        kind,
        relations: RefVec::Slice { slice: &[] },
        owner: [0; 32],
        defined_twice: false,
    };
    let events = TrackKind::Events(EventsDef::INSTANTS);
    // --8<-- [start:define]
    let numbers = Numbers {
        // one slot per key: each parameter stays in force until it changes or is deleted
        params: defs.define_track(track("params", TrackRepeat::KEYED, events.clone()))?,
        // one slot: the profile in force
        profile: defs.define_track(track("profile", TrackRepeat::STATE, events))?,
        // values written as parts and read whole
        firmware: defs.define_track(track("firmware", TrackRepeat::Stream, TrackKind::Bytes))?,
    };
    // --8<-- [end:define]
    Ok((defs, numbers))
}

/// What one put wrote: its time, what it put and whether it wrote a record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Put {
    pub ms: i64,
    pub what: String,
    pub written: bool,
}

/// A record of one parameter, as its history lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    pub ms: i64,
    /// The value, None for a deletion.
    pub value: Option<String>,
}

/// What [run] wrote and read back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    /// Every put, in order, the group's as one.
    pub puts: Vec<Put>,
    /// The firmware image as the writer sealed it.
    pub firmware: ValueSeal,
    /// The pieces of the chain.
    pub pieces: usize,
    /// The profile in force at the end of the last piece, read from it alone.
    pub profile: Option<String>,
    /// Each live parameter at the end of the last piece, read from it alone, in key order.
    pub params: Vec<(String, String)>,
    /// Whether the last piece holds every live key ([ww_tape::reader::KeysAt::is_whole]).
    pub params_whole: bool,
    /// The records of `pid/kp` in the first piece, newest first.
    pub kp_history: Vec<Change>,
    /// The firmware image read back from the chain: its bytes, their hash checked.
    pub firmware_read: Vec<u8>,
    /// The blocks of the first piece, in order.
    pub blocks: Vec<BlockLine>,
}

/// One block of a piece as [blocks] lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockLine {
    /// Where it starts in the piece.
    pub offset: usize,
    pub len: usize,
    pub kind: BlockKind,
    /// Its records, or the items of a chunk.
    pub count: u32,
    /// The names of its header bits that are set.
    pub marks: Vec<&'static str>,
}

/// The header bits [blocks] names.
const MARKS: [(u16, &str); 5] = [
    (FLAG_REPEAT, "put"),
    (FLAG_WITH_NEXT, "with next"),
    (FLAG_TWICE, "twice"),
    (FLAG_GROUP, "group"),
    (FLAG_GROUP_START, "group start"),
];

// --8<-- [start:blocks]
/// The blocks of `piece`, back to back from its first byte: each one read and checked alone.
pub fn blocks(piece: &[u8]) -> Result<Vec<BlockLine>, ww_tape::BlockError> {
    let mut lines = Vec::new();
    let mut offset = 0;
    while let Some(rest) = piece.get(offset..).filter(|r| !r.is_empty()) {
        let block = Block::read(rest)?;
        let marks = MARKS.iter().filter(|(bit, _)| block.flags & bit != 0);
        lines.push(BlockLine {
            offset,
            len: block.len(),
            kind: block.kind,
            count: block.record_count,
            marks: marks.map(|(_, name)| *name).collect(),
        });
        offset += block.len();
    }
    Ok(lines)
}
// --8<-- [end:blocks]

/// The firmware image: `FIRMWARE_LEN` bytes that differ from part to part.
pub fn firmware_image() -> Vec<u8> {
    (0..FIRMWARE_LEN)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Write the tuning session into a chain of two pieces in memory, then read it back.
pub fn run() -> Result<Report, Box<dyn Error>> {
    let (defs, n) = definitions()?;
    let mem = MemPieces::new();
    let options = ChainOptions {
        tapes: PieceTapes::Each,
        writer: WriterOptions {
            sync: SyncPolicy::None,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut chain = PieceChain::create_on(mem.clone(), OWNER, 0, &[], options)?;
    let mut puts = Vec::new();
    let mut log = |ms, what: String, written| puts.push(Put { ms, what, written });

    // --8<-- [start:keys]
    // The tool sets each parameter by its path: one record per key.
    for (key, value) in [
        ("limit/current_ma", "12000"),
        ("pid/kp", "0.80"),
        ("pid/ki", "0.05"),
        ("debug/trace", "on"),
    ] {
        let written = chain.put_key(&defs, n.params, 0, key.as_bytes(), value.as_bytes())?;
        log(0, format!("{key} = {value}"), written);
    }
    let written = chain.put(&defs, n.profile, 0, b"gentle")?;
    log(0, "profile = gentle".to_owned(), written);
    // The same value again lies close behind: the put writes nothing.
    let written = chain.put_key(&defs, n.params, 1_000, b"pid/kp", b"0.80")?;
    log(1_000, "pid/kp = 0.80".to_owned(), written);
    // A deletion is a record too: the key is no longer in force.
    let written = chain.delete_key(&defs, n.params, 2_000, b"debug/trace")?;
    log(2_000, "delete debug/trace".to_owned(), written);
    // --8<-- [end:keys]

    // --8<-- [start:group]
    // The fast profile and its gains go together: a reader takes all three or none.
    let group = [
        GroupPut::state(n.profile, 3_000, b"fast"),
        GroupPut::key(n.params, 3_000, b"pid/kp", b"1.20"),
        GroupPut::key(n.params, 3_000, b"pid/ki", b"0.09"),
    ];
    let written = chain.put_group(&defs, &group)?;
    // --8<-- [end:group]
    log(
        3_000,
        "group: profile = fast, pid/kp = 1.20, pid/ki = 0.09".to_owned(),
        written.iter().all(|w| *w),
    );

    // --8<-- [start:firmware]
    // The image goes out in parts of 64 KiB and ends in a seal: its length and BLAKE3 hash.
    let next = chain.next_item(&defs, n.firmware).ok_or("a held track")?;
    let part_len = BytesOptions {
        part_len: PART_LEN,
        ..Default::default()
    };
    let mut parts = BytesWriter::new(&defs, n.firmware, next, part_len)?;
    let image = firmware_image();
    let firmware = parts.write_from(4_000, &image[..], |p: BytesPart<'_>| {
        chain.write_chunk(&defs, &p.head, p.columns()).map(drop)
    })?;
    // --8<-- [end:firmware]

    // The next piece carries every live key and the profile.
    chain.cut_now(&defs)?;
    let written = chain.put_key(&defs, n.params, 5_000, b"limit/current_ma", b"15000")?;
    log(5_000, "limit/current_ma = 15000".to_owned(), written);
    let closed = chain.close()?;

    let mut tapes = Vec::new();
    for piece in &closed {
        let tape = mem
            .get(&piece.tape)?
            .ok_or("a closed piece is in the store")?;
        tapes.push(tape.to_vec()?);
    }
    let first = TapeReader::open(tapes.first().ok_or("a first piece")?.as_slice())?;
    let last = TapeReader::open(tapes.last().ok_or("a last piece")?.as_slice())?;

    // --8<-- [start:read_keys]
    // The last piece alone: every live key is carried into it, deletions left out.
    let end = last.pieces().first().ok_or("one piece")?.end;
    let live = last.keys_at(0, n.params, b"", end)?;
    let params = live
        .items
        .iter()
        .filter_map(|(key, item)| Some((text(key), text(item.keyed()?.value?))))
        .collect();
    let profile = last.state_at(0, ww_tape::Slot::State { track: n.profile }, end)?;
    // The first piece: every record of pid/kp, newest first.
    let first_end = first.pieces().first().ok_or("one piece")?.end;
    let history = first.key_history(0, n.params, b"pid/kp", first_end)?;
    // --8<-- [end:read_keys]
    let kp_history = history
        .items
        .iter()
        .map(|item| Change {
            ms: item.at.map_or(0, |at| at.ticks),
            value: item.keyed().and_then(|k| k.value).map(text),
        })
        .collect();

    // --8<-- [start:read_value]
    // The image is read whole, from every piece in order, its hash checked against the seal.
    let readers = tapes
        .iter()
        .map(|t| TapeReader::open(t.as_slice()))
        .collect::<Result<Vec<_>, _>>()?;
    let readers: Vec<_> = readers.iter().collect();
    let firmware_id = defs.track_identity(n.firmware).ok_or("held")?.0;
    let mut firmware_read = Vec::new();
    ww_tape::bytes_track::read_value(&readers, firmware_id, firmware.id, |bytes| {
        firmware_read.extend_from_slice(bytes);
        Ok::<(), core::convert::Infallible>(())
    })?;
    // --8<-- [end:read_value]

    Ok(Report {
        puts,
        firmware,
        pieces: closed.len(),
        profile: profile.item.map(|s| text(&s.item)),
        params,
        params_whole: live.is_whole(),
        kp_history,
        firmware_read,
        blocks: blocks(tapes.first().ok_or("a first piece")?)?,
    })
}

impl Report {
    /// The puts, one line each, and the firmware's seal.
    pub fn puts_text(&self) -> Result<String, fmt::Error> {
        let mut out = String::new();
        for p in &self.puts {
            let written = if p.written { "written" } else { "nothing new" };
            writeln!(out, "{:>5} ms  {:<52} {written}", p.ms, p.what)?;
        }
        Ok(out)
    }

    /// The firmware as written and as read back.
    pub fn firmware_text(&self) -> Result<String, fmt::Error> {
        let ValueSeal {
            id,
            len,
            hash,
            parts,
        } = &self.firmware;
        let mut out = String::new();
        write!(
            out,
            "written: value {}, {len} bytes in {parts} parts with the seal, BLAKE3 ",
            id.0
        )?;
        for b in hash.iter().take(8) {
            write!(out, "{b:02x}")?;
        }
        writeln!(
            out,
            "...\nread back from {} pieces: {} bytes, hash checked",
            self.pieces,
            self.firmware_read.len()
        )?;
        Ok(out)
    }

    /// The blocks of the first piece, one line each.
    pub fn blocks_text(&self) -> Result<String, fmt::Error> {
        let mut out = String::from("offset    bytes  kind          count  marks\n");
        for b in &self.blocks {
            let kind = format!("{:?}", b.kind);
            let marks = b.marks.join(", ");
            let line = format!(
                "{:>6} {:>8}  {kind:<13} {:>5}  {marks}",
                b.offset, b.len, b.count
            );
            out += line.trim_end();
            out.push('\n');
        }
        Ok(out)
    }

    /// What the last piece alone and the first piece say of the keys.
    pub fn keys_text(&self) -> Result<String, fmt::Error> {
        let mut out = String::from("the last piece alone:\n");
        let profile = self.profile.as_deref().unwrap_or("none");
        writeln!(out, "  profile = {profile}")?;
        for (key, value) in &self.params {
            writeln!(out, "  {key} = {value}")?;
        }
        writeln!(out, "  every live key: {}", self.params_whole)?;
        out.push_str("pid/kp in the first piece, newest first:\n");
        for c in &self.kp_history {
            let value = c.value.as_deref().unwrap_or("deleted");
            writeln!(out, "  {:>5} ms  {value}", c.ms)?;
        }
        Ok(out)
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "puts:\n{}", self.puts_text()?)?;
        write!(f, "firmware:\n{}", self.firmware_text()?)?;
        write!(
            f,
            "the first piece, block by block:\n{}",
            self.blocks_text()?
        )?;
        write!(f, "{}", self.keys_text()?)
    }
}
