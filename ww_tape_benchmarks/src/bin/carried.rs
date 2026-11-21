//! The bytes a piece spends on the definitions of the tracks its head carries (WRITE-9): a piece of a chain whose head
//! carries `n` tracks, records blocks on one of them until the piece holds 1 or 8 MiB, closed, as a cut by bytes
//! closes it. The same piece with the definitions of that one track alone held is the baseline: the data it holds
//! more is what carrying the quiet tracks costs. Prints a Markdown table for 100 and 1 000 tracks, then the bytes of a
//! piece of one records block at the track caps ([TRACKS_PER_PIECE_MAX], [HEAD_TRACKS_MAX]) against the baseline.
#![allow(
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::cast_precision_loss,
    clippy::indexing_slicing,
    clippy::print_stdout,
    reason = "a bench reads no file of others: it computes sizes and rates freely, fails by panicking and prints its table"
)]

use shrink_wrap::prelude::*;
use ww_tape::piece::{
    ContentHash, HEAD_TRACKS_MAX, OwnerId, PieceHead, PieceId, PieceIdent, TRACKS_PER_PIECE_MAX,
    TrackStart,
};
use ww_tape::{
    BlockId, BlockKind, BlockWriter, ClockDef, ClockOrigin, ClockTime, Definitions, Describe,
    EventsDef, FullVersion, Magnitude, MemSink, Segment, SessionTag, SyncPolicy, TapeWriter,
    TrackDef, TrackHash, TrackKind, Version, clock_key, track_key,
};

const CLOCK: u32 = 1;

/// The clock of every track.
fn clock() -> Describe<'static> {
    Describe::Clock(ClockDef {
        number: UNib32(CLOCK),
        key: clock_key(0xb8, &[], 1),
        rate: Magnitude::pow10(9).into(),
        resolution: Magnitude::pow10(-9).into(),
        origin: ClockOrigin::Boot,
        label: "bench",
    })
}

/// Events track `number`, labeled with its number as a sensor channel's name might be.
fn track(number: u32, label: &str) -> TrackDef<'_> {
    TrackDef {
        number: UNib32(number),
        key: track_key(0xb8, &[Segment::Index(number)]),
        clock: UNib32(CLOCK),
        dimensions: RefVec::Slice { slice: &[] },
        api: FullVersion::new("bench_api", Version::new(0, 1, 0)),
        bundle: RefVec::Slice { slice: &[] },
        path: RefVec::Slice {
            slice: &[UNib32(1)],
        },
        keys: RefVec::Slice { slice: &[] },
        item: None,
        label,
        repeat: ww_tape::TrackRepeat::Stream,
        kind: TrackKind::Events(EventsDef::INSTANTS),
        relations: RefVec::Slice { slice: &[] },
        owner: [0; 32],
    }
}

/// Bytes of the item of each records block.
const ITEM_LEN: usize = 4000;

/// A records block of one record of `len` bytes on track 1.
fn records(id: BlockId, n: i64, len: usize) -> anyhow::Result<Vec<u8>> {
    let mut buf = vec![0u8; len + 64];
    let item = vec![0x5a; len];
    let mut b = BlockWriter::new(&mut buf, BlockKind::Records, id, &[CLOCK])
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    b.push(1, ClockTime::new(CLOCK, n), &[&item])
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let len = b.finish();
    buf.truncate(len);
    Ok(buf)
}

/// The definitions of `n` tracks and the head rows of a piece that carries them all.
fn tracks(n: u32) -> anyhow::Result<(Definitions, Definitions, Vec<TrackStart>)> {
    let labels: Vec<String> = (1..=n).map(|k| format!("channel {k:07}")).collect();
    let defs: Vec<TrackDef<'_>> = (1..=n).map(|k| track(k, &labels[k as usize - 1])).collect();
    let rows = defs
        .iter()
        .map(|t| TrackStart {
            track_id: t.id(),
            track_number: t.number.0,
            next_item: 1,
            from_tick: 0,
            owner: OwnerId(t.owner),
            hash: TrackHash::START,
            incomplete: false,
            not_carried: false,
        })
        .collect();
    let mut all = vec![clock()];
    all.extend(defs.iter().cloned().map(Describe::Track));
    let mut every = Definitions::new();
    every
        .define_all(&all)
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let mut one = Definitions::new();
    one.define_all(&all[..2])
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    Ok((every, one, rows))
}

/// A closed piece, second of its chain, whose head carries `rows`, written with `defs`: `blocks` records blocks on
/// track 1, or as many as reach `fill` bytes when `blocks` is None. Its length and the blocks written.
fn piece(
    defs: &Definitions,
    rows: &[TrackStart],
    fill: u64,
    blocks: Option<u64>,
) -> anyhow::Result<(u64, u64)> {
    let e = |e: ww_tape::WriteError<_>| anyhow::anyhow!("{e:?}");
    let mut w = TapeWriter::create_on(MemSink::new(), SyncPolicy::None);
    w.set_producer("bench");
    w.set_session(SessionTag::writable(0x6361_7272))
        .map_err(e)?;
    let owner = OwnerId([8; 32]);
    let head = PieceHead {
        ident: PieceIdent {
            id: PieceId([2; 16]),
            owner,
            seq: 1,
        },
        prev: Some((ContentHash([1; 32]), PieceId::first(&owner))),
        prev_session: SessionTag::NONE,
        integrity: ww_tape::Integrity::Full,
    };
    w.begin_piece(&head, rows).map_err(e)?;
    let mut n = 0u64;
    while blocks.map_or(n == 0 || w.bytes() < fill, |b| n < b) {
        let id = w.next_id();
        w.write_block(defs, &records(id, n.cast_signed(), ITEM_LEN)?)
            .map_err(e)?;
        n += 1;
    }
    Ok((w.close().map_err(e)?.len, n))
}

/// Pieces whose heads carry `n` tracks, written until `fill` bytes as a cut by bytes counts them (the close's final
/// index not counted): the length and records blocks of one with the definitions of every track, then of one without
/// them (that of track 1 alone).
fn displaced(n: u32, fill: u64) -> anyhow::Result<((u64, u64), (u64, u64))> {
    let (every, one, rows) = tracks(n)?;
    let base = piece(&one, &rows, fill, None)?;
    let with = piece(&every, &rows, fill, None)?;
    Ok((with, base))
}

/// The bytes of a piece of one records block whose head carries `n` tracks, with the definitions of every track
/// and without them.
fn carried(n: u32) -> anyhow::Result<(u64, u64)> {
    let (every, one, rows) = tracks(n)?;
    let (base, _) = piece(&one, &rows, 0, Some(1))?;
    let (with, _) = piece(&every, &rows, 0, Some(1))?;
    Ok((with, base))
}

fn main() -> anyhow::Result<()> {
    let mib = |m: u64| m << 20;
    println!(
        "Pieces cut by bytes whose heads carry the tracks, with their definitions and without, records blocks of {ITEM_LEN} bytes on one of them:"
    );
    println!();
    println!(
        "| tracks | cut at | records blocks | without carried | data less | piece (bytes) | without carried | piece more |"
    );
    println!("|---|---|---|---|---|---|---|---|");
    for n in [100, 1000] {
        for m in [1, 8] {
            let ((len, with), (base_len, base)) = displaced(n, mib(m))?;
            println!(
                "| {n} | {m} MiB | {with} | {base} | {:.1} % | {len} | {base_len} | {:.1} % |",
                (base - with) as f64 * 100.0 / base as f64,
                (len - base_len) as f64 * 100.0 / base_len as f64
            );
        }
    }
    println!();
    println!("At the caps, a piece of one records block:");
    println!();
    println!("| tracks | carried (bytes) | per track (bytes) |");
    println!("|---|---|---|");
    for n in [TRACKS_PER_PIECE_MAX, HEAD_TRACKS_MAX] {
        let n32 = u32::try_from(n)?;
        let (with, base) = carried(n32)?;
        let extra = with - base;
        println!("| {n} | {extra} | {} |", extra / n as u64);
    }
    Ok(())
}
