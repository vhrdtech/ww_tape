//! The book pages against the code: the code they show runs as written there, and `docs/format.md`'s layouts and
//! tables are those of the module docs and of the blocks this crate writes.

use std::path::Path;

use ww_tape::chunk::{ChunkHead, Codec, ColumnKind, ColumnSpec};
use ww_tape::{
    Block, BlockId, BlockKind, BlockWriter, ChainOptions, ClockTime, Definitions, EndMode,
    Finished, Magnitude, MemSink, OwnerId, PieceChain, PieceFiles, Ratio64, SeriesGrid, SessionTag,
    SyncPolicy, TapeWriter, WriteError,
};

use crate::definitions::{CLOCK, clock, state, track};

const MODE: u32 = 5;

/// The lines of "What the application does with its definitions", between the setup and the checks.
fn example(writer: &mut TapeWriter<MemSink>) -> Result<(), WriteError> {
    let (clock, temperature_track, mode_track) = (clock(CLOCK), track(1, CLOCK), state(MODE));
    let ticks = 1_000;
    let mode_bytes = b"mode: run".to_vec();
    let mut buf = [0u8; 128];
    let mut b = BlockWriter::new(&mut buf, BlockKind::Records, writer.next_id(), &[CLOCK]).unwrap();
    b.push(1, ClockTime::new(CLOCK, ticks), &[&21i16.to_le_bytes()])
        .unwrap();
    let n = b.finish();
    let readings = buf[..n].to_vec();

    // as on the pages
    let mut defs = Definitions::new();
    defs.define_all(&[clock, temperature_track, mode_track])?;
    writer.put(&defs, MODE, ticks, &mode_bytes)?; // a state item, at its use
    writer.write_block(&defs, &readings)?; // the definitions it needs go first
    // end of the pages' lines
    Ok(())
}

#[test]
fn book_definitions_example() {
    let mut writer = TapeWriter::create_on(MemSink::new(Path::new("book")), SyncPolicy::None);
    example(&mut writer).unwrap();
    let tape = writer.sink().tape();
    writer.finish().unwrap();
    let bytes = tape.to_vec().unwrap();
    let r = ww_tape::TapeReader::open(&bytes[..]).unwrap();
    let defs = r.definitions_at(0, u64::MAX).unwrap();
    assert!(defs.is_whole(), "every definition reads: {defs:?}");
}

/// The lines of "Writing a chain into one file", between the setup and the checks: the finished pieces of the first
/// session and those of the second.
fn one_file_example(
    path: &Path,
    owner: OwnerId,
    defs: &Definitions,
    head: &ChunkHead,
    columns: &[(ColumnSpec, &[u8])],
) -> Result<(Vec<Finished>, Vec<Finished>), WriteError> {
    // as on the pieces page
    let options = ChainOptions {
        files: PieceFiles::One,
        ..ChainOptions::default()
    };
    let mut chain = PieceChain::create(path, owner, 0, &[], options.clone())?;
    chain.write_chunk(defs, head, columns)?;
    let first = chain.finish()?;
    // opened again: a new piece behind the finished one
    let (mut chain, _) = PieceChain::resume(path, owner, 0, options)?;
    chain.write_chunk(defs, head, columns)?;
    let second = chain.finish()?;
    // end of the pieces page's lines
    Ok((first, second))
}

#[test]
fn book_one_file_example() {
    let dir = crate::pieces::scratch("book_one_file");
    let path = dir.join("run.wwt");
    let (head, starts) = crate::pieces::chunk(0, 0);
    let (first, second) = one_file_example(
        &path,
        OwnerId([7; 32]),
        crate::definitions::base_defs(),
        &head,
        &crate::pieces::column(&starts),
    )
    .unwrap();
    assert_eq!((first.len(), second.len()), (1, 2));
    assert_eq!(second[0], first[0]);
    let bytes = std::fs::read(&path).unwrap();
    let r = ww_tape::TapeReader::open(&bytes[..]).unwrap();
    assert_eq!(r.pieces().len(), 2);
    let chain = r.chain();
    assert!(chain.is_whole(), "{chain:?}");
    std::fs::remove_dir_all(&dir).unwrap();
}

/// The lines of `tests/it/book.rs` between `start` and `end`, unindented once.
fn lines_between(start: &str, end: &str) -> String {
    let source = read("tests/it/book.rs");
    let from = source.find(start).unwrap();
    let to = source.find(end).unwrap();
    source[from..to]
        .lines()
        .skip(1)
        .map(|l| format!("{}\n", l.strip_prefix("    ").unwrap_or(l)))
        .collect()
}

fn read(path: &str) -> String {
    std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(path)).unwrap()
}

/// The fenced blocks of a Markdown text, as (language, lines). A fence is a line of its own: a fence line that
/// carries text after its closing backticks fails, as it would run the block on into the prose.
fn fenced(text: &str, what: &str) -> Vec<(String, String)> {
    let mut blocks = Vec::new();
    let mut open: Option<(String, String)> = None;
    for (n, line) in text.lines().enumerate() {
        let trimmed = line.trim_start();
        match open.take() {
            None => {
                if let Some(lang) = trimmed.strip_prefix("```") {
                    assert!(
                        !lang.contains('`'),
                        "{what}:{}: a fence line with more on it: {line}",
                        n + 1
                    );
                    open = Some((lang.trim().to_owned(), String::new()));
                }
            }
            Some((lang, mut body)) => {
                if trimmed.starts_with("```") {
                    assert_eq!(
                        trimmed,
                        "```",
                        "{what}:{}: a closing fence carries text",
                        n + 1
                    );
                    blocks.push((lang, body));
                } else {
                    body.push_str(line);
                    body.push('\n');
                    open = Some((lang, body));
                }
            }
        }
    }
    assert!(open.is_none(), "{what}: a fence is never closed");
    blocks
}

#[test]
fn pages_show_the_example_the_test_runs() {
    let lines = lines_between(
        "    // as on the pages\n",
        "    // end of the pages' lines\n",
    );
    for page in ["../docs/index.md", "../docs/writing.md"] {
        let blocks = fenced(&read(page), page);
        assert!(
            blocks
                .iter()
                .any(|(lang, body)| lang == "rust" && *body == lines),
            "{page} shows the lines `book_definitions_example` runs:\n{lines}"
        );
    }
}

#[test]
fn pieces_page_shows_the_example_the_test_runs() {
    let lines = lines_between(
        "    // as on the pieces page\n",
        "    // end of the pieces page's lines\n",
    );
    let page = "../docs/pieces_and_files.md";
    let blocks = fenced(&read(page), page);
    assert!(
        blocks
            .iter()
            .any(|(lang, body)| lang == "rust" && *body == lines),
        "{page} shows the lines `book_one_file_example` runs:\n{lines}"
    );
}

/// The module docs of every source file under `dir`, `//!` stripped, one after another.
fn module_docs(dir: &Path, out: &mut String) {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            module_docs(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            for line in std::fs::read_to_string(&path).unwrap().lines() {
                if let Some(doc) = line.strip_prefix("//!") {
                    out.push_str(doc.strip_prefix(' ').unwrap_or(doc));
                    out.push('\n');
                }
            }
        }
    }
}

/// The `text` blocks of `docs/format.md`.
fn format_layouts() -> Vec<String> {
    fenced(&read("../docs/format.md"), "docs/format.md")
        .into_iter()
        .filter(|(lang, _)| lang == "text")
        .map(|(_, body)| body)
        .collect()
}

/// Every byte layout `docs/format.md` shows is a whole layout block of the module docs, word for word.
#[test]
fn format_page_layouts_are_the_module_docs() {
    let mut docs = String::new();
    module_docs(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut docs,
    );
    let doc_blocks: Vec<String> = fenced(&docs, "module docs")
        .into_iter()
        .map(|(_, body)| body)
        .collect();
    let layouts = format_layouts();
    for block in &layouts {
        assert!(
            doc_blocks.contains(block),
            "docs/format.md shows a layout that is no whole block of the module docs:\n{block}"
        );
    }
    assert!(
        layouts.len() >= 8,
        "the layouts of every block kind are on the page"
    );
}

/// The layout whose first field line starts with `first`, as (offset, size, field) per line; offsets that are not
/// numbers (`len-4`, `...`) are left out.
fn layout(first: &str) -> Vec<(usize, String, String)> {
    let block = format_layouts()
        .into_iter()
        .find(|b| {
            b.lines()
                .nth(1)
                .is_some_and(|l| l.trim_start().starts_with(first))
        })
        .unwrap_or_else(|| panic!("docs/format.md has a layout starting {first:?}"));
    block
        .lines()
        .filter_map(|l| {
            let mut words = l.split_whitespace();
            let offset = words.next()?.parse().ok()?;
            let size = words.next()?.to_owned();
            let field = words.collect::<Vec<_>>().join(" ");
            Some((offset, size, field))
        })
        .collect()
}

/// The number at the field of `layout` starting with `name`, read from `bytes` at the page's offset and size.
fn field(layout: &[(usize, String, String)], bytes: &[u8], name: &str) -> u64 {
    let (offset, size, _) = layout
        .iter()
        .find(|(_, _, f)| f.starts_with(name))
        .unwrap_or_else(|| panic!("the layout has a field {name:?}"));
    let len = match size.as_str() {
        "u8" => 1,
        "u16" => 2,
        "u32" => 4,
        "u64" | "i64" => 8,
        other => other.parse().unwrap(),
    };
    let mut le = [0u8; 8];
    le[..len].copy_from_slice(&bytes[*offset..offset + len]);
    u64::from_le_bytes(le)
}

/// The header layout on the page, read from a records block `BlockWriter` wrote.
#[test]
fn format_page_header_offsets_read_a_block() {
    let header = layout("0   4  magic");
    let session = SessionTag::from_u32(0x5e55_10a7);
    let mut buf = [0u8; 128];
    let mut b = BlockWriter::new(
        &mut buf,
        BlockKind::Records,
        BlockId::new(session, 3),
        &[CLOCK],
    )
    .unwrap();
    b.push(2, ClockTime::new(CLOCK, 1_000_000), &[b"ok"])
        .unwrap();
    b.push(3, ClockTime::new(CLOCK, 1_000_250), &[&[0x9c]])
        .unwrap();
    let n = b.finish();
    let bytes = &buf[..n];
    Block::read(bytes).unwrap();
    assert_eq!(
        field(&header, bytes, "magic").to_le_bytes()[..4],
        ww_tape::MAGIC
    );
    assert_eq!(
        field(&header, bytes, "version"),
        u64::from(ww_tape::VERSION)
    );
    assert_eq!(field(&header, bytes, "kind"), 1);
    assert_eq!(field(&header, bytes, "flags"), 0);
    assert_eq!(field(&header, bytes, "len"), n as u64);
    assert_eq!(field(&header, bytes, "records"), 2);
    assert_eq!(field(&header, bytes, "seq"), 3);
    assert_eq!(field(&header, bytes, "session tag"), 0x5e55_10a7);
    assert_eq!(field(&header, bytes, "head CRC"), 0, "0 in a records block");
}

/// The chunk head layout on the page, read from a series chunk `chunk::write_chunk` wrote.
#[test]
fn format_page_chunk_head_offsets_read_a_chunk() {
    let head_layout = layout("24 u32  session tag");
    let period = Ratio64::new(62_500, 3).unwrap();
    let grid = SeriesGrid::start(96, 5_000, Magnitude::pow10(-3));
    let (first_tick, last_tick) = grid.chunk_ticks(period, 100, 4).unwrap();
    let head = ChunkHead {
        kind: BlockKind::Series,
        track_id: 0x1122_3344_5566_7788,
        track_number: 9,
        first_item: 100,
        count: 4,
        first_tick,
        last_tick,
        latest_end: last_tick,
        time_mode: 0,
        end_mode: EndMode::Absent,
        grid: Some(grid),
    };
    let samples: Vec<u8> = [1i16, 2, 3, 4]
        .iter()
        .flat_map(|v| v.to_le_bytes())
        .collect();
    let spec = ColumnSpec {
        kind: ColumnKind::Samples,
        codec: Codec::RAW,
        param: 0,
        len: samples.len(),
    };
    let mut buf = vec![0u8; ww_tape::chunk::chunk_len(&head, &[spec], 0)];
    let id = BlockId::new(SessionTag::from_u32(0xc0ffee), 7);
    let n = ww_tape::chunk::write_chunk(&mut buf, id, &head, &[(spec, &samples)], 0).unwrap();
    let bytes = &buf[..n];
    let chunk = Block::read(bytes).unwrap();
    let chunk = chunk.chunk().unwrap();
    assert_eq!(ChunkHead::read_head(bytes).unwrap(), head);
    let f = |name| field(&head_layout, bytes, name);
    assert_eq!(f("session tag"), 0xc0ffee);
    assert_eq!(f("payload offset"), chunk.payload_offset() as u64);
    assert_eq!(f("track number"), 9);
    assert_eq!(f("track id"), 0x1122_3344_5566_7788);
    assert_eq!(f("first item number"), 100);
    assert_eq!(f("first tick") as i64, first_tick);
    assert_eq!(f("last tick") as i64, last_tick);
    assert_eq!(f("latest end") as i64, last_tick);
    assert_eq!(f("segment shift"), 16);
    assert_eq!(f("columns n"), 1);
    assert_eq!(f("time mode"), 0);
    assert_eq!(f("end mode"), 0);
    assert_eq!(f("grid index"), 96);
    assert_eq!(f("grid tick") as i64, 5_000);
    assert_eq!(f("scale length"), 2);
    let scale_at = head_layout
        .iter()
        .find(|(_, _, f)| f.starts_with("scale:"))
        .unwrap()
        .0;
    assert_eq!(bytes[scale_at..scale_at + 2], [0xfa, 0x00], "10⁻³ is fa 00");
}

/// The rows of the Markdown table whose header row starts with `header`, as cells.
fn table(page: &str, header: &str) -> Vec<Vec<String>> {
    let text = read(page);
    let mut lines = text.lines().skip_while(|l| !l.starts_with(header));
    assert!(lines.next().is_some(), "{page} has a table {header:?}");
    lines
        .skip(1)
        .take_while(|l| l.starts_with('|'))
        .map(|l| {
            l.trim_matches('|')
                .split('|')
                .map(|c| c.trim().trim_matches('`').to_owned())
                .collect()
        })
        .collect()
}

/// The block kinds table on the page against `BlockKind`.
#[test]
fn format_page_kinds_are_block_kinds() {
    let rows = table("../docs/format.md", "| Kind | Block |");
    let kinds = [
        (1, Some(BlockKind::Records), "records"),
        (2, None, "reserved"),
        (3, Some(BlockKind::Events), "events chunk"),
        (4, Some(BlockKind::Series), "series chunk"),
        (5, Some(BlockKind::Parity), "parity"),
        (6, Some(BlockKind::Summary), "summary"),
        (7, Some(BlockKind::Index), "index"),
        (8, Some(BlockKind::Finish), "finish"),
        (9, Some(BlockKind::Tail), "tail"),
        (10, Some(BlockKind::PieceHead), "piece head"),
        (11, Some(BlockKind::POSTINGS), "postings"),
        (12, Some(BlockKind::DICTIONARY), "dictionary"),
    ];
    assert_eq!(rows.len(), kinds.len());
    for (row, (n, kind, name)) in rows.iter().zip(kinds) {
        assert_eq!(row[0], n.to_string());
        assert!(row[1].starts_with(name), "kind {n} is {name}: {row:?}");
        match kind {
            Some(kind) => assert_eq!(BlockKind::from_u8(n), kind),
            None => assert_eq!(n, ww_tape::RESERVED_KIND),
        }
    }
    assert!(matches!(BlockKind::from_u8(11), BlockKind::Other(11)));
}

/// A cap's value as the page writes it: `64 MiB`, `64 MiB - 36`, `16 Mi`, `2^55`, `14562`.
fn amount(text: &str) -> u64 {
    let (text, minus) = match text.split_once(" - ") {
        Some((a, b)) => (a, b.parse::<u64>().unwrap()),
        None => (text, 0),
    };
    let value = if let Some((base, exp)) = text.split_once('^') {
        base.parse::<u64>().unwrap().pow(exp.parse().unwrap())
    } else {
        let (n, unit) = text.split_once(' ').unwrap_or((text, ""));
        let shift = match unit {
            "" => 0,
            "Ki" | "KiB" => 10,
            "Mi" | "MiB" => 20,
            "GiB" => 30,
            other => panic!("unit {other}"),
        };
        n.parse::<u64>().unwrap() << shift
    };
    value - minus
}

/// The table of caps on the page holds every cap, at the constant's value.
#[test]
fn format_page_caps_are_the_constants() {
    let caps: &[(&str, u64)] = &[
        ("MAX_BLOCK_LEN", ww_tape::MAX_BLOCK_LEN as u64),
        ("DECODED_BODY_MAX", ww_tape::body::DECODED_BODY_MAX as u64),
        ("LZ4_DECODED_MAX", ww_tape::lz4::LZ4_DECODED_MAX as u64),
        ("MAX_BLOCK_CLOCKS", ww_tape::MAX_BLOCK_CLOCKS as u64),
        ("MAX_RECORD_HEAD", ww_tape::MAX_RECORD_HEAD as u64),
        ("MAX_CLOCK_TABLE", ww_tape::MAX_CLOCK_TABLE as u64),
        ("MAX_COLUMNS", ww_tape::chunk::MAX_COLUMNS as u64),
        (
            "SEGMENT_SHIFT_MAX",
            ww_tape::chunk::SEGMENT_SHIFT_MAX.into(),
        ),
        ("MAX_SEGMENTS", ww_tape::chunk::MAX_SEGMENTS as u64),
        ("Codec::SERIES_ITEMS_MAX", Codec::SERIES_ITEMS_MAX.into()),
        ("WIDTH_MAX", ww_tape::bitpack::WIDTH_MAX.into()),
        (
            "KERNEL_WIDTH_MAX",
            ww_tape::bitpack::KERNEL_WIDTH_MAX.into(),
        ),
        ("SLOPE_MAX", ww_tape::times::SLOPE_MAX as u64),
        ("ITEMS_MAX", ww_tape::times::ITEMS_MAX as u64),
        ("INDEX_LEN_MAX", ww_tape::index::INDEX_LEN_MAX as u64),
        (
            "CHECKPOINT_ENTRIES_MAX",
            ww_tape::index::CHECKPOINT_ENTRIES_MAX as u64,
        ),
        ("NODE_SHIFT_MAX", ww_tape::summary::NODE_SHIFT_MAX.into()),
        ("LEVEL_MAX", ww_tape::summary::LEVEL_MAX.into()),
        ("SCALE_LEN_MAX", ww_tape::series::SCALE_LEN_MAX as u64),
        ("FACTOR_MAX", ww_tape::summary::FACTOR_MAX),
        (
            "SeriesSummarySpec::BASE_SHIFT_MAX",
            ww_tape::summary::SeriesSummarySpec::BASE_SHIFT_MAX.into(),
        ),
        (
            "SERIES_CHANNELS_MAX",
            ww_tape::series::SERIES_CHANNELS_MAX.into(),
        ),
        ("GAP_MAX_MAX", ww_tape::GAP_MAX_MAX.into()),
        ("GAP_PROBES_MAX", ww_tape::GAP_PROBES_MAX.into()),
        ("GAP_CHECKS_MAX", ww_tape::GAP_CHECKS_MAX.into()),
        ("STATE_READ_MAX", ww_tape::reader::STATE_READ_MAX),
        (
            "DEFERRED_BYTES_MAX",
            ww_tape::writer::DEFERRED_BYTES_MAX as u64,
        ),
    ];
    let rows = table("../docs/format.md", "| Cap | Value | Bounds |");
    let names: Vec<&str> = rows.iter().map(|r| r[0].as_str()).collect();
    let expected: Vec<&str> = caps.iter().map(|(n, _)| *n).collect();
    assert_eq!(names, expected, "the page lists every cap, in order");
    for (row, (name, value)) in rows.iter().zip(caps) {
        assert_eq!(amount(&row[1]), *value, "{name} is {}", row[1]);
    }
}

/// The piece head and finish rows on the page, read from a chain of two pieces: owner, track hash, records and the
/// row lengths at the page's offsets, the hash a fold of the track's records.
#[test]
fn format_page_piece_rows_read_a_chain() {
    use ww_tape::{ChainOptions, MemTapes, OwnerId, PieceChain, TrackHash};
    let pieces = layout("32 [16] piece id");
    let at = |name: &str| {
        pieces
            .iter()
            .filter(|(_, _, f)| f.starts_with(name))
            .map(|(o, _, _)| *o)
            .collect::<Vec<_>>()
    };
    let tapes = MemTapes::new();
    let dir = Path::new("rows");
    let mut chain = PieceChain::create_on(
        tapes.clone(),
        dir,
        OwnerId([7; 32]),
        0,
        &[],
        ChainOptions::default(),
    )
    .unwrap();
    let defs = crate::definitions::base_defs();
    for n in 0..3 {
        let block = crate::pieces::records(chain.writer().unwrap().next_id(), n);
        chain.write_block(defs, &block).unwrap();
    }
    chain.cut_now().unwrap();
    let block = crate::pieces::records(chain.writer().unwrap().next_id(), 3);
    chain.write_block(defs, &block).unwrap();
    let done = chain.finish().unwrap();
    let fold = |n: i64| (0..n).fold(TrackHash::START, |h, k| h.record(k, &[k as u8; 30]));

    let second = tapes.get(&done[1].path).unwrap().unwrap().to_vec().unwrap();
    // the piece head is the first block, one row: the track of records with its hash after piece 0
    assert_eq!(at("track row length"), [144, 34]);
    assert_eq!(u16::from_le_bytes([second[144], second[145]]), 96);
    let head_row = 32 + 120;
    let owner_at = at("owner (TrackDef::owner; zeros")[0];
    let hash_before = at("track hash before")[0];
    assert_eq!(second[head_row + owner_at..][..32], [0; 32]);
    assert_eq!(second[head_row + hash_before..][..32], fold(3).0);

    let finish_at = (done[1].len as usize) - ww_tape::TAIL_LEN;
    let tail = ww_tape::Tail::read(&second[finish_at..]).unwrap();
    let f = &second[tail.finish_offset as usize..];
    assert_eq!(u16::from_le_bytes([f[34], f[35]]), 128);
    let finish_row = 176;
    let records = at("records: the track's")[0];
    let hash_after = at("track hash after")[0];
    assert_eq!(
        u64::from_le_bytes(f[finish_row + records..][..8].try_into().unwrap()),
        1
    );
    assert_eq!(f[finish_row + hash_after..][..32], fold(4).0);
    assert_eq!(done[1].tracks[0].hash, fold(4));
}
