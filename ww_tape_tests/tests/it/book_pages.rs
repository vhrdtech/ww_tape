//! The code the book's pages show, each a marked section (`// --8<-- [start:NAME]`) that a page includes, so every
//! `rust` block of the book compiles and runs here.

/// `docs/tracks.md`, section `track_ids`.
#[test]
fn track_ids() {
    // --8<-- [start:track_ids]
    use example_drive::{definitions, key};
    use ww_tape::{Dimension, track_id};

    // The key of can0 is the hash of its path in the drive's API.
    let can0 = key("can0");
    assert_eq!(can0.get(), 0x0158_4c57_693f_d3f2);
    // Without dimensions, the id is the key.
    assert_eq!(track_id(can0, []), can0.id());
    // A dimension gives the same resource another id.
    let bench2 = track_id(
        can0,
        [Dimension {
            name: "bench",
            value: "2",
        }],
    );
    assert_eq!(bench2.get(), 0x4d71_7cfb_2248_cb86);
    // Inside the drive's files, can0 is track 3: the number its definition was given.
    let (_, numbers) = definitions(25_000, true, 32 << 20).expect("the drive's definitions");
    assert_eq!(numbers.can0.get(), 3);
    // --8<-- [end:track_ids]
}

/// `docs/tracks.md`, section `emergency_frame`.
#[test]
fn emergency_frame() {
    // --8<-- [start:emergency_frame]
    // can0 runs at 1 kHz: a frame is due every 1000 µs.
    assert_eq!(example_drive::emergency_frame(1000), 3_120_000);
    // --8<-- [end:emergency_frame]
}

/// `docs/tracks.md`, section `drift_hour`.
#[test]
fn drift_hour() {
    // --8<-- [start:drift_hour]
    use example_drive::{BOOT_UNIX_NS, definitions};
    use shrink_wrap::prelude::*;
    use ww_tape::{Describe, Rounding, Slot};

    let (defs, numbers) = definitions(25_000, true, 32 << 20).expect("the drive's definitions");
    let map = Slot::Map {
        from: numbers.board_clock,
        to: numbers.host_clock,
    };
    let bytes = defs.get(&map).expect("the drive's map");
    let Describe::Map(map) = Describe::from_ww_bytes(bytes).expect("a definition") else {
        panic!("the slot of a map holds a map");
    };
    let segment = map
        .segments
        .iter()
        .next()
        .expect("one segment")
        .expect("it reads");
    // One hour on the board's timer, 90 000 000 samples at 40 µs.
    let hour_us = 3_600 * 1_000_000;
    assert_eq!(hour_us / 40, 90_000_000);
    let host_ns = segment.map(hour_us, Rounding::Floor).expect("mapped") - BOOT_UNIX_NS;
    // On the host's clock the hour of samples lasts 72 ms less.
    assert_eq!(hour_us * 1_000 - host_ns, 72_000_000);
    // --8<-- [end:drift_hour]
}

/// `docs/blocks.md`, section `records_block`.
#[test]
fn records_block() {
    // --8<-- [start:records_block]
    use example_drive::definitions;
    use ww_tape::{Block, BlockId, BlockKind, BlockWriter, ClockTime, SessionTag};

    // The numbers the drive's definitions give its board clock and its tracks.
    let (_, numbers) = definitions(25_000, true, 32 << 20).expect("the drive's definitions");
    let (board, log, temp) = (numbers.board_clock, numbers.log, numbers.temp);
    let mut buf = [0u8; 256];
    let id = BlockId::new(SessionTag::writable(1), 0);
    let mut w = BlockWriter::new(&mut buf, BlockKind::Records, id, &[board]).expect("room");
    // A log line at 1 s and the temperature, 32.15 °C, 250 µs later.
    w.push(log, ClockTime::new(board, 1_000_000), &[b"motor: run"])
        .expect("room");
    w.push(
        temp,
        ClockTime::new(board, 1_000_250),
        &[&3_215i16.to_le_bytes()],
    )
    .expect("room");
    let len = w.finish();
    assert_eq!(len, 59);

    let block = Block::read(&buf[..len]).expect("a good block");
    assert_eq!((block.kind, block.record_count), (BlockKind::Records, 2));
    let tracks: Vec<_> = block.records().map(|r| r.expect("reads").track).collect();
    assert_eq!(tracks, [log.into(), temp.into()]);

    // One changed bit fails the CRC-32C.
    buf[40] ^= 1;
    assert!(Block::read(&buf[..len]).is_err());
    // --8<-- [end:records_block]
}

/// `docs/definitions.md`, section `puts`.
#[test]
fn puts() {
    // --8<-- [start:puts]
    use example_drive::{Mode, definitions};
    use ww_tape::{MemSink, SyncPolicy, TapeWriter};

    // The definitions, and the numbers they gave the drive's clocks and tracks.
    let (mut defs, numbers) = definitions(25_000, true, 32 << 20).expect("the drive's definitions");
    let (can0, mode) = (numbers.can0, numbers.mode);
    assert_eq!((defs.len(), defs.bytes()), (8, 469));

    let mut w = TapeWriter::create_on(MemSink::new(), SyncPolicy::None);
    // The first put writes the mode, with the definitions it needs.
    assert!(w.put(&defs, mode, 0, Mode::Run.item()).expect("put"));
    // The same mode 1 ms later lies close behind: the put writes nothing.
    assert!(!w.put(&defs, mode, 1_000, Mode::Run.item()).expect("put"));
    // A new mode is written.
    assert!(w.put(&defs, mode, 2_000, Mode::Fault.item()).expect("put"));

    // Closing can0 drops its definition; the clocks stay, since other tracks name them.
    assert!(defs.close(can0));
    assert_eq!(defs.len(), 7);
    // --8<-- [end:puts]
}

/// `docs/anatomy.md`, section `closed_piece`.
#[test]
fn closed_piece() {
    // --8<-- [start:closed_piece]
    use example_drive::Drive;
    use ww_tape::{PIECE_ALIGN, TapeReader};

    let drive = Drive::scaled(1000).expect("1000 divides the rates");
    let recording = drive.record().expect("the run records");
    let piece = &recording.pieces[0];

    let reader = TapeReader::open(&piece[..]).expect("a closed piece opens");
    let info = &reader.pieces()[0];
    let tail = info.tail.as_ref().expect("a closed piece has a tail");
    assert_eq!(tail.piece_len, piece.len() as u64);
    assert_eq!(piece.len() % PIECE_ALIGN as usize, 0);
    assert!(info.close.is_some());

    // The same run records to the same bytes.
    assert_eq!(
        drive.record().expect("the run records").pieces,
        recording.pieces
    );
    // --8<-- [end:closed_piece]
}

/// `docs/damage.md`, section `flipped_bit`.
#[test]
fn flipped_bit() {
    // --8<-- [start:flipped_bit]
    use ww_tape::{FileBlocks, Recovery};

    let drive = example_drive::Drive::scaled(1000).expect("1000 divides the rates");
    let recording = drive.record().expect("the run records");
    let (piece, byte) = recording.story_flip().expect("the run reaches minute 50");
    let damaged = recording
        .flip(piece, byte)
        .expect("the byte lies in the piece");

    let mut walk = FileBlocks::with_recovery(&damaged.bytes[..], Recovery::Resync);
    let (mut good, mut bad_segments, mut reports) = (0, 0, 0);
    while let Some(block) = walk.next_block() {
        match block {
            Ok(b) if b.is_intact() => good += 1,
            Ok(b) => bad_segments += b.chunk().map_or(0, |c| c.bad_segments().count()),
            Err(_) => reports += 1,
        }
    }
    // Every block reads but one chunk, which has one bad segment.
    assert_eq!((bad_segments, reports), (1, 0));
    assert!(good > 50);
    // --8<-- [end:flipped_bit]
}

/// `docs/clocks.md`, section `exact_rate`.
#[test]
fn exact_rate() {
    // --8<-- [start:exact_rate]
    use ww_tape::{Ratio64, Rounding};

    let period = Ratio64::new(62_500, 3).expect("a positive fraction");
    let tick = |i: i64| period.apply_rounded(i, Rounding::Floor).expect("in i64");
    assert_eq!([tick(1), tick(2), tick(3)], [20_833, 41_666, 62_500]);
    // One hour of samples ends exactly one hour on.
    let hour = 48_000 * 3_600;
    assert_eq!(tick(hour), 3_600_000_000_000);
    // A period of 20833 ns would end 57.6 ms early.
    assert_eq!(tick(hour) - 20_833 * hour, 57_600_000);
    // --8<-- [end:exact_rate]
}

/// `docs/clocks.md`, section `map_minute_37`.
#[test]
fn map_minute_37() {
    // --8<-- [start:map_minute_37]
    use example_drive::{BOOT_UNIX_NS, definitions};
    use shrink_wrap::prelude::*;
    use ww_tape::{Describe, Rounding, Slot};

    let (defs, numbers) = definitions(25_000, true, 32 << 20).expect("the drive's definitions");
    let slot = Slot::Map {
        from: numbers.board_clock,
        to: numbers.host_clock,
    };
    let bytes = defs
        .get(&slot)
        .expect("the drive maps the board's clock to the host's");
    let Describe::Map(map) = Describe::from_ww_bytes(bytes).expect("a definition") else {
        panic!("the slot of a map holds a map");
    };
    let segment = map
        .segments
        .iter()
        .next()
        .expect("one segment")
        .expect("it reads");
    let minute_37_us = 37 * 60 * 1_000_000;
    let host_ns = segment.map(minute_37_us, Rounding::Floor).expect("mapped");
    // 2219.9556 s after the boot at 09:00:00 UTC: 44.4 ms less than the board's count.
    assert_eq!(host_ns - BOOT_UNIX_NS, 2_219_955_600_000);
    // --8<-- [end:map_minute_37]
}

/// `docs/clocks.md`, section `tick_range`.
#[test]
fn tick_range() {
    // --8<-- [start:tick_range]
    // i64 ticks at 1 ps last a little over 106 days.
    assert_eq!(i64::MAX / 1_000_000_000_000 / 86_400, 106);
    // At 1 ns they last 292 years of 365.25 days.
    assert_eq!(i64::MAX / 1_000_000_000 / 31_557_600, 292);
    // --8<-- [end:tick_range]
}

/// `docs/grids.md`, section `tile_box`.
#[test]
fn tile_box() {
    // --8<-- [start:tile_box]
    use example_thermal::{BOX_COLUMNS, BOX_ROWS, Thermal};
    let layout = Thermal::plate().layout().expect("a valid layout");
    // the box of the figure meets tile row 1 and tile columns 0 and 1
    let tiles = layout.tile_box(&[BOX_ROWS, BOX_COLUMNS]).expect("two axes");
    assert_eq!(tiles.ranges(), [(1, 2), (0, 2)]);
    // --8<-- [end:tile_box]
}

/// `docs/grids.md`, section `overview_level`.
#[test]
fn overview_level() {
    // --8<-- [start:overview_level]
    use example_thermal::Thermal;
    let layout = Thermal::plate().layout().expect("a valid layout");
    assert_eq!(layout.overview_levels(), 5);
    // a pixel of 4 cells takes level 2, whose cells are 4 cells wide
    assert_eq!(layout.level_for(&[4.0, 4.0]), 2);
    // --8<-- [end:overview_level]
}

/// `docs/grids.md`, section `tile_sizes`.
#[test]
fn tile_sizes() {
    // --8<-- [start:tile_sizes]
    use example_images::Images;
    let sixty = Images::frames()
        .tiles(60, 60)
        .layout()
        .expect("a valid layout");
    let sixty_four = Images::frames().layout().expect("a valid layout");
    assert_eq!(
        (sixty.overview_levels(), sixty_four.overview_levels()),
        (2, 6)
    );
    // --8<-- [end:tile_sizes]
}

/// `docs/raw_media.md`, section `stale_card`.
#[test]
fn stale_card() {
    // --8<-- [start:stale_card]
    use example_drive::{Drive, EARLIER_CHAIN};
    use ww_tape::{Medium, OpenOptions, TapeReader};

    let today = Drive::scaled(1000)
        .expect("1000 divides the rates")
        .on_sd_card();
    let earlier = today
        .chain(EARLIER_CHAIN)
        .record()
        .expect("the earlier run records");
    let run = today.record().expect("the run records");
    let crashed = today
        .crash(run.story_crash_byte().expect("minute 37"))
        .expect("the crash");
    let card = crashed
        .on_card(&earlier)
        .expect("the earlier run fills the card");

    let raw = OpenOptions {
        medium: Medium::Raw,
        ..OpenOptions::default()
    };
    let on_card = TapeReader::open_with(&card.bytes[..], raw).expect("the card opens");
    // Today's two closed pieces and the torn one; the earlier run is stale data.
    assert_eq!(on_card.pieces().len(), 3);

    let as_file = TapeReader::open(&card.bytes[..]).expect("the card opens as a file");
    // Read as a file, the earlier run's last piece passes for the newest.
    assert_eq!(as_file.pieces().len(), 4);
    // --8<-- [end:stale_card]
}

/// `docs/writers.md`, section `story`.
#[test]
fn story() {
    // --8<-- [start:story]
    // The drive's hour at 1/1000 of its data, as the figures draw it.
    let drive = example_drive::Drive::scaled(1000).expect("1000 divides the rates");
    let story = drive.story().expect("the run records");
    // The crash at minute 37 resumes to the bytes of the run without it.
    assert!(story.same_bytes);
    // --8<-- [end:story]
}

/// `docs/writers.md`, section `piece_align`.
#[test]
fn piece_align() {
    // --8<-- [start:piece_align]
    // A piece ends on a multiple of 4 KiB.
    assert_eq!(ww_tape::PIECE_ALIGN, 4096);
    // --8<-- [end:piece_align]
}
