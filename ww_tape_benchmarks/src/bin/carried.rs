//! The bytes a piece spends on the definitions of the tracks its head carries (TRACK-5): a piece of a chain whose head
//! carries `n` tracks, records blocks on one of them until the piece holds 1 or 8 MiB, closed, as a cut by bytes
//! closes it. The same piece with the definitions of that one track alone held is the baseline: the data it holds
//! more is what carrying the quiet tracks costs. Prints a Markdown table for 100 and 1 000 tracks, then the bytes of a
//! piece of one records block at the track caps ([TRACKS_PER_PIECE_MAX], [HEAD_TRACKS_MAX]) against the baseline, then
//! the most tracks a cut carries whole (none left out) under the default index policy and lower ones, on one clock and
//! each on its own clock. Last, what a cut spends to carry the newest item of each state track besides its
//! definition: the next piece of a chain whose first piece put `n` state tracks once, against the same with `n`
//! stream tracks that gave data. Every number is a size: the load doesn't change it.
#![allow(
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::cast_precision_loss,
    clippy::indexing_slicing,
    clippy::print_stdout,
    reason = "a bench reads no file of others: it computes sizes and rates freely, fails by panicking and prints its table"
)]

use clap::Parser;
use exact_units::Count;
use exact_units::units::Entry;
use shrink_wrap::prelude::*;
use ww_tape::index::STATE_ENTRIES_MAX;
use ww_tape::piece::{
    ContentHash, HEAD_TRACKS_MAX, OwnerId, PieceHead, PieceId, PieceIdent, TRACKS_PER_PIECE_MAX,
    TrackStart,
};
use ww_tape::{
    BlockId, BlockKind, BlockWriter, ClockOrigin, ClockSpec, ClockTime, Definitions, EventsDef,
    FullVersion, IndexPolicy, Magnitude, MemSink, Segment, SessionTag, SyncPolicy, TapeWriter,
    TrackHash, TrackKind, TrackSpec, Version, clock_key, track_key,
};
use ww_tape::{ClockNum, TrackNum};
use ww_tape_bench::{Bench, Cell, Common};

#[derive(Parser)]
#[command(
    name = "carried",
    about = "ww_tape: the definitions a piece carries for the tracks of its head, per piece",
    version = vc_build::version!(),
    after_long_help = "Examples:
  just bench-carried
  carried --json carried.json"
)]
struct Args {
    #[command(flatten)]
    common: Common,
}

/// Clock `index`, keyed by its index.
fn clock(index: u32) -> ClockSpec<'static> {
    ClockSpec {
        key: clock_key(0xb8, &[], u64::from(index)),
        rate: Magnitude::pow10(9).into(),
        resolution: Magnitude::pow10(-9).into(),
        origin: ClockOrigin::Boot,
        label: "bench",
    }
}

/// Events track `index` on clock `clock`, keyed by its index and labeled as a sensor channel's name might be.
fn track(index: u32, clock: ClockNum, label: &str) -> TrackSpec<'_> {
    TrackSpec {
        key: track_key(0xb8, &[Segment::Index(index)]),
        clock,
        dimensions: RefVec::Slice { slice: &[] },
        items: ww_tape::Items::WireWeaver {
            api: FullVersion::new("bench_api", Version::new(0, 1, 0)),
            bundle: RefVec::Slice { slice: &[] },
            path: RefVec::Slice {
                slice: &[UNib32(1)],
            },
            keys: RefVec::Slice { slice: &[] },
            item: None,
        },
        label,
        repeat: ww_tape::TrackRepeat::Stream,
        kind: TrackKind::Events(EventsDef::INSTANTS),
        relations: RefVec::Slice { slice: &[] },
        owner: [0; 32],
        defined_twice: false,
    }
}

/// Bytes of the item of each records block.
const ITEM_LEN: usize = 4000;

/// A records block of one record of `len` bytes on the first track of `rows`, on the first clock.
fn records(id: BlockId, rows: &[TrackStart], n: i64, len: usize) -> anyhow::Result<Vec<u8>> {
    let first = rows.first().map(|r| r.track_number);
    let first = first.ok_or_else(|| anyhow::anyhow!("a track"))?;
    records_on(id, FIRST_CLOCK, first, n, len)
}

/// The number of the first clock defined: track 1's.
const FIRST_CLOCK: ClockNum = ClockNum::new(1);

/// A records block of one record of `len` bytes on track `track` of clock `clock`.
fn records_on(
    id: BlockId,
    clock: ClockNum,
    track: TrackNum,
    n: i64,
    len: usize,
) -> anyhow::Result<Vec<u8>> {
    let mut buf = vec![0u8; len + 64];
    let item = vec![0x5a; len];
    let mut b = BlockWriter::new(&mut buf, BlockKind::Records, id, &[clock])
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    b.push(track, ClockTime::new(clock, n), &[&item])
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let len = b.finish();
    buf.truncate(len);
    Ok(buf)
}

/// The definitions of `n` tracks on one clock, those of track 1 alone, and the head rows of a piece that carries them
/// all.
fn tracks(n: u32) -> anyhow::Result<(Definitions, Definitions, Vec<TrackStart>)> {
    tracks_on(n, false)
}

/// The definitions of `n` tracks, on one clock or each on its own (`own_clocks`), those of track 1 alone, and the
/// head rows of a piece that carries them all.
fn tracks_on(
    n: u32,
    own_clocks: bool,
) -> anyhow::Result<(Definitions, Definitions, Vec<TrackStart>)> {
    let d = |e: ww_tape::DescribeError| anyhow::anyhow!("{e:?}");
    let labels: Vec<String> = (1..=n).map(|k| format!("channel {k:07}")).collect();
    // track 1's clock first, then its track: `one` holds those two
    let (mut every, mut one) = (Definitions::new(), Definitions::new());
    let first = every.define_clock(clock(1)).map_err(d)?;
    one.define_clock(clock(1)).map_err(d)?;
    let mut clocks = vec![first];
    for k in 2..=n {
        clocks.push(if own_clocks {
            every.define_clock(clock(k)).map_err(d)?
        } else {
            first
        });
    }
    let mut rows = Vec::new();
    for k in 1..=n {
        let t = track(k, clocks[k as usize - 1], &labels[k as usize - 1]);
        if k == 1 {
            one.define_track(t.clone()).map_err(d)?;
        }
        rows.push(TrackStart {
            track_id: t.id(),
            track_number: every.define_track(t.clone()).map_err(d)?,
            next_item: ww_tape::ItemNum::new(1),
            from_tick: 0,
            owner: OwnerId(t.owner),
            hash: TrackHash::START,
            incomplete: false,
            not_carried: false,
            keys_carried: false,
        });
    }
    Ok((every, one, rows))
}

/// A writer on the second piece of a chain, its head carrying `rows`, under `policy` state entries.
fn start(rows: &[TrackStart], policy: Count<Entry, u64>) -> anyhow::Result<TapeWriter<MemSink>> {
    let e = |e: ww_tape::WriteError<_>| anyhow::anyhow!("{e:?}");
    let mut w = TapeWriter::create_on(MemSink::new(), SyncPolicy::None);
    w.set_index_policy(IndexPolicy {
        state_entries_max: policy,
        ..IndexPolicy::default()
    })
    .map_err(e)?;
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
        unlisted_keys_unknown: false,
    };
    w.begin_piece(&head, rows).map_err(e)?;
    Ok(w)
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
    let mut w = start(rows, STATE_ENTRIES_MAX)?;
    let mut n = 0u64;
    while blocks.map_or(n == 0 || w.bytes() < fill, |b| n < b) {
        let id = w.next_id();
        w.write_block(defs, &records(id, rows, n.cast_signed(), ITEM_LEN)?)
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

/// Whether a cut under `policy` state entries carries the definitions of all `n` tracks (on one clock, or each on
/// its own): a piece whose head carries their rows and holds one records block on track 1 is cut with the definitions
/// of every track, and the cut marks no row not carried.
fn carries_all(policy: Count<Entry, u64>, n: u32, own_clocks: bool) -> anyhow::Result<bool> {
    let e = |e: ww_tape::WriteError<_>| anyhow::anyhow!("{e:?}");
    let (every, one, rows) = tracks_on(n, own_clocks)?;
    let mut w = start(&rows, policy)?;
    let id = w.next_id();
    w.write_block(&one, &records(id, &rows, 0, ITEM_LEN)?)
        .map_err(e)?;
    let (closed, _) = w
        .cut_on(&every, MemSink::new())
        .map_err(|c| anyhow::anyhow!("{:?}", c.error))?;
    Ok(closed.not_carried() == 0)
}

/// The most tracks a cut carries whole under `policy` state entries, on one clock or each on its own, and whether
/// the head cap ([HEAD_TRACKS_MAX]) bounds them instead of the policy.
fn room_for(policy: Count<Entry, u64>, own_clocks: bool) -> anyhow::Result<(u32, bool)> {
    let cap = u32::try_from(HEAD_TRACKS_MAX)?;
    let mut hi = cap.min(u32::try_from(policy.get())?);
    if carries_all(policy, hi, own_clocks)? {
        return Ok((hi, hi == cap));
    }
    // lo carries all (none: trivially), hi doesn't
    let mut lo = 0u32;
    while hi - lo > 1 {
        let mid = lo + (hi - lo) / 2;
        if carries_all(policy, mid, own_clocks)? {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    Ok((lo, false))
}

/// The bytes of the piece after a cut, holding one records block on a stream track, when the first piece gave `n`
/// other tracks a record each: a put of an item of `item_len` bytes on each of `n` state tracks (`state`), or a
/// records block on each of `n` stream tracks.
fn after_cut(n: u32, item_len: usize, state: bool) -> anyhow::Result<u64> {
    let e = |e: ww_tape::WriteError<_>| anyhow::anyhow!("{e:?}");
    let d = |e: ww_tape::DescribeError| anyhow::anyhow!("{e:?}");
    let labels: Vec<String> = (1..=n + 1).map(|k| format!("channel {k:07}")).collect();
    let mut defs = Definitions::new();
    let c = defs.define_clock(clock(1)).map_err(d)?;
    let mut numbers = Vec::new();
    for k in 1..=n + 1 {
        let mut t = track(k, c, &labels[k as usize - 1]);
        if state && k <= n {
            t.repeat = ww_tape::TrackRepeat::State(ww_tape::StatePolicy::DEFAULT);
        }
        numbers.push(defs.define_track(t).map_err(d)?);
    }
    let (given, last) = numbers.split_at(n as usize);
    let mut w = TapeWriter::create_on(MemSink::new(), SyncPolicy::None);
    w.set_producer("bench");
    w.set_session(SessionTag::writable(0x7374_6174))
        .map_err(e)?;
    let owner = OwnerId([8; 32]);
    let head = PieceHead {
        ident: PieceIdent {
            id: PieceId::first(&owner),
            owner,
            seq: 0,
        },
        prev: None,
        prev_session: SessionTag::NONE,
        integrity: ww_tape::Integrity::Full,
        unlisted_keys_unknown: false,
    };
    w.begin_piece(&head, &[]).map_err(e)?;
    let item = vec![0x5a; item_len];
    for t in given {
        if state {
            w.put(&defs, *t, 0, &item).map_err(e)?;
        } else {
            let id = w.next_id();
            w.write_block(&defs, &records_on(id, c, *t, 0, item_len)?)
                .map_err(e)?;
        }
    }
    let (_, mut next) = w
        .cut_on(&defs, MemSink::new())
        .map_err(|c| anyhow::anyhow!("{:?}", c.error))?;
    let id = next.next_id();
    next.write_block(&defs, &records_on(id, c, last[0], 0, ITEM_LEN)?)
        .map_err(e)?;
    Ok(next.close().map_err(e)?.len)
}

fn main() -> anyhow::Result<()> {
    let args: Args = ww_tape_bench::args();
    let mut bench = Bench::start(
        "carried",
        vc_build::build_info!(),
        &args.common,
        vec![("item bytes", ITEM_LEN.to_string())],
    )?;
    println!("{}", bench.header());
    let mib = |m: u64| m << 20;
    let mut table = bench.table(
        "pieces",
        &format!(
            "Pieces cut by bytes whose heads carry the tracks, with their definitions and without, \
             records blocks of {ITEM_LEN} bytes on one of them."
        ),
        &[
            "Tracks",
            "Cut at",
            "Records blocks",
            "Records blocks without carried",
            "Data less (%)",
            "Piece (bytes)",
            "Piece without carried (bytes)",
            "Piece more (%)",
        ],
    );
    for n in [100, 1000] {
        for m in [1, 8] {
            let ((len, with), (base_len, base)) = displaced(n, mib(m))?;
            table.row(vec![
                Cell::count(n.into()),
                Cell::text(format!("{m} MiB")),
                Cell::count(with),
                Cell::count(base),
                Cell::num((base - with) as f64 * 100.0 / base as f64, 1),
                Cell::count(len),
                Cell::count(base_len),
                Cell::num((len - base_len) as f64 * 100.0 / base_len as f64, 1),
            ]);
        }
    }
    println!("{}", bench.push(table));
    let mut table = bench.table(
        "caps",
        "A piece of one records block at the track caps, the bytes its carried definitions add.",
        &["Cap", "Tracks", "Carried (bytes)", "Per track (bytes)"],
    );
    for (cap, n) in [
        ("`TRACKS_PER_PIECE_MAX`", TRACKS_PER_PIECE_MAX),
        ("`HEAD_TRACKS_MAX`", HEAD_TRACKS_MAX),
    ] {
        let (with, base) = carried(u32::try_from(n)?)?;
        let extra = with - base;
        table.row(vec![
            Cell::text(cap),
            Cell::count(n as u64),
            Cell::count(extra),
            Cell::count(extra / n as u64),
        ]);
    }
    println!("{}", bench.push(table));
    let mut table = bench.table(
        "room",
        "The most tracks a cut carries whole (none left out), by the index policy's state entries, \
         the tracks on one clock or each on its own.",
        &[
            "State entries",
            "Tracks on one clock",
            "Tracks on their own clocks",
        ],
    );
    for policy in [
        STATE_ENTRIES_MAX,
        Count::new(1 << 16),
        Count::new(1 << 12),
        Count::new(1 << 8),
    ] {
        let name = if policy == STATE_ENTRIES_MAX {
            format!("{} (default)", policy.get())
        } else {
            policy.get().to_string()
        };
        let cell = |(n, capped): (u32, bool)| {
            if capped {
                Cell::text(format!("{n} (the head cap)"))
            } else {
                Cell::count(n.into())
            }
        };
        table.row(vec![
            Cell::text(name),
            cell(room_for(policy, false)?),
            cell(room_for(policy, true)?),
        ]);
    }
    println!("{}", bench.push(table));
    let mut table = bench.table(
        "state",
        "The piece after a cut, one records block, when the piece before put each state track once (its newest item \
         carried) or gave each stream track data (its definition alone carried).",
        &[
            "Tracks",
            "Item (bytes)",
            "State tracks (bytes)",
            "Stream tracks (bytes)",
            "Per state (bytes)",
        ],
    );
    for n in [100, 1000] {
        for item_len in [8, 64] {
            let with = after_cut(n, item_len, true)?;
            let base = after_cut(n, item_len, false)?;
            table.row(vec![
                Cell::count(n.into()),
                Cell::count(item_len as u64),
                Cell::count(with),
                Cell::count(base),
                Cell::num((with as f64 - base as f64) / f64::from(n), 1),
            ]);
        }
    }
    println!("{}", bench.push(table));
    bench.finish()
}
