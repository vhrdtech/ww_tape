//! The seed files of the targets, in `seeds/<target>/`.
//!
//! They are valid blocks from ww_tape's golden files, the inputs behind ww_tape's review fixes (named by the fix's
//! commit), inputs of the hostile streams and the inputs of the known failures.
//!
//! The same code gives the same files; `ww_tape_fuzz_seeds write` writes them.

use crate::hostile::CODECS;
use shrink_wrap::prelude::*;
use std::path::Path;
use ww_tape::index::{IndexEntry, IndexHead, IndexKind, index_len, write_index};
use ww_tape::lz4::LZ4_DECODED_MAX;
use ww_tape::piece::{
    Close, CloseReason, ContentHash, OwnerId, PieceHead, PieceId, PieceIdent, TAIL_LEN, Tail,
    close_len, piece_head_len, write_close, write_piece_head,
};
use ww_tape::times::EncodedTimes;
use ww_tape::{
    Block, BlockId, BlockKind, BlockWriter, ClockTime, ColumnKind, Describe, Element, FLAG_REPEAT,
    GAP_MAX_MAX, GAP_PROBES_MAX, Layout, Producer, SessionTag, TrackKind,
};
use ww_tape::{ClockNum, TrackNum};
use ww_tape::{TrackId, TrackKey};

/// Longest seed file.
pub const SEED_LEN_MAX: usize = 64 << 10;
/// Most seed files of one target.
pub const SEEDS_PER_TARGET_MAX: usize = 64;
/// Inputs of a hostile stream kept as seeds per hostile target.
const STREAM_SEEDS: usize = 24;
/// Bytes of a hostile stream input.
const STREAM_INPUT_LEN: usize = 96;

/// One seed file.
pub struct Seed {
    pub target: &'static str,
    pub name: String,
    pub bytes: Vec<u8>,
}

/// The inputs of the known failures, minimised (`ww_tape_fuzz_seeds minimize`): target, name, input in hex.
pub const KNOWN_INPUTS: &[(&str, &str, &str)] = &[];

/// Inputs these tests and the fuzz targets found that failed until a fix: target, name (seed `fixed-<name>`), hex.
pub const FIXED_INPUTS: &[(&str, &str, &str)] = &[
    ("tape-index", "empty-index-reads-back", "0a"),
    (
        "tape-index",
        "final-index-overlapping-blocks",
        "ff050100000000000007050500000000000000000a02020202002e00000000000000000000020200000000070505000007050500000000000000000a020202020000000000000000000000020000000000b7000000022f02020202",
    ),
    ("tape-resume", "closed-last-piece-damaged", "070030c80101"),
    (
        "tape-reclose",
        "empty-append-has-its-own-final-index",
        "0801000c",
    ),
    (
        "tape-reclose",
        "damaged-append-head-keeps-the-close",
        "00000003000000000000000000000000000000000030",
    ),
    (
        "tape-reclose",
        "append-head-damaged-to-a-close-keeps-the-close",
        "0000ff13000000000000000000030405",
    ),
    (
        "tape-reclose",
        "append-head-length-rotted-past-the-end-refuses",
        "311153f13812a04bfbdebbcd005294adbc7be1ea0f0f6d4d257fbb7ab45500047f78bb7ab40d31a4064b6b09f3d400",
    ),
    (
        "tape-resume",
        "early-length-claim-spends-hash-walk",
        "2a2a5d7e2a3d",
    ),
    (
        "tape-resume",
        "close-first-cuts-taken-blocks",
        "894f545760575da9cf3a552e050002895d7851",
    ),
    (
        "tape-resume",
        "damaged-blocks-in-one-raw-stretch",
        "51578934aaa50221d2",
    ),
    (
        "tape-resume",
        "damaged-block-past-a-raw-start",
        "00666761016667016108",
    ),
    (
        "tape-resume",
        "resume-and-verify-walk-alike",
        "0200302f7f3bc1183cc654fc31cb447af413878e0bb34c10c82a18362a1836101e1654fc31cb447bf4ed788e0bb34c10c877717131711d95f2c877717032711d57f5ec80808080808080808080808080808018011d57f5ec003000d425",
    ),
    (
        "tape-times-hostile",
        "chunk-count-unbounded",
        "80f70000000000000000000000000000000000000000002703a94d75",
    ),
    (
        "tape-times-hostile",
        "implicit-chunk-of-millions-read-item-by-item",
        "00000000000100000001a7ffffffffffffff80aaa66666666666ff80aaa666666666666666a7a713a3a7a700a76900000051000000d400000051000000a78724",
    ),
    (
        "tape-reader-hostile",
        "salvage-walk-reads-inside-a-gap",
        "eb9543000000a10000000000008e3d00002600000000000000000000d34f00000000000000000000000000008900d6",
    ),
    (
        "tape-index-hostile",
        "reader-entries-past-the-piece",
        "00ef86000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000b6",
    ),
    (
        "tape-reader-hostile",
        "reader-entries-past-the-piece",
        "000011000000000000000000000000000000a10000003300000000000000000072",
    ),
    (
        "tape-summary-hostile",
        "node-items-overflow",
        "00000000000000000000000000000000000000000000006b003b",
    ),
    (
        "tape-reader-hostile",
        "reader-entries-past-the-piece-2",
        "c07a113987eed2e835e80f8c7db504afa55aa13c2341339c50117a5b4efb0be472b186398442d67290a057c7d0c48c2bd12b8110c47f7451c6fd1c660edf6e17bd752d8944c9c2d616e0f9257a475697",
    ),
    (
        "tape-reader",
        "salvage-misses-a-prefix-block",
        "57575450010a00009c00000000000000000000000000000002000d60000000000202020202020202020202020202020201010101010101010101010101010101010101010101010101010101010101010000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000060000000000000008e297a4457575450010100004500000002000000000000000000000002000d6000000000000008006d6564696105060014600000000000100000000200000000000201052aa15eba1357575450010100005100000001000000010000000000000000000000000000000000000000000000000000000000000000000000000060000000000000007d6ad47257575450010106004000000002000000000000000000000013d3c00b00000000000014600000000000000000010000000000000201052a0003000001dec8d0315757545001010600000000030000000000000002000d6000000000010109062803030303030303030303030303030303030303030303030303030303030303030303030303030303624adc215700000000000013d304040000000100000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000057570000000000000000000057575450010100005100000001000000030000000000000002000d6000000000010109062803030303030303030303030303030303030303030303030303030303030303030303030303030303624adc215700000000000013d3",
    ),
    (
        "tape-grid",
        "overview-layout-tile-cells-past-u64",
        "1087dcfffffe7fdc24ff03684001e280000000005940780000bc3561b2",
    ),
    (
        "tape-index",
        "events-entry-with-a-level",
        "01030000010100004700010001000057000100000000010109001e000000000000000000000000000000000000a50000000000002600000000d02649ef",
    ),
];

fn seed(target: &'static str, name: impl Into<String>, bytes: Vec<u8>) -> Seed {
    Seed {
        target,
        name: name.into(),
        bytes,
    }
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).expect("hex"))
        .collect()
}

/// The blocks of a golden file: one per line, in hex.
fn golden(text: &str) -> Vec<Vec<u8>> {
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| unhex(l.trim()))
        .collect()
}

const GOLDEN_V1: &str = include_str!("../../ww_tape/golden/v1.hex");
const GOLDEN_CHUNKS: &str = include_str!("../../ww_tape/golden/v1-chunks.hex");
const GOLDEN_INDEX: &str = include_str!("../../ww_tape/golden/v1-index.hex");
const GOLDEN_PAYLOADS: &str = include_str!("../../ww_tape/golden/v1-payloads.hex");
const GOLDEN_SAMPLES: &str = include_str!("../../ww_tape/golden/v1-samples.hex");
const GOLDEN_SERIES: &str = include_str!("../../ww_tape/golden/v1-series.hex");
const GOLDEN_SUMMARY: &str = include_str!("../../ww_tape_summary/golden/v1-summary.hex");
const GOLDEN_GRID_SUMMARY: &str = include_str!("../../ww_tape_summary/golden/v1-grid-summary.hex");
const GOLDEN_TIMES: &str = include_str!("../../ww_tape/golden/v1-times.hex");
const GOLDEN_CRC_ONLY: &str = include_str!("../../ww_tape/golden/v1-level-crc-only.hex");
const GOLDEN_NONE: &str = include_str!("../../ww_tape/golden/v1-level-none.hex");
/// The golden grids by their count of fixed axes.
const GOLDEN_GRIDS: [(u32, &str); 4] = [
    (1, include_str!("../../ww_tape/golden/v1-grid-1.hex")),
    (2, include_str!("../../ww_tape/golden/v1-grid-2.hex")),
    (5, include_str!("../../ww_tape/golden/v1-grid-5.hex")),
    (31, include_str!("../../ww_tape/golden/v1-grid-31.hex")),
];

/// Each block of a golden file as a seed of `target`.
fn golden_blocks(target: &'static str, file: &str, text: &str) -> Vec<Seed> {
    golden(text)
        .into_iter()
        .enumerate()
        .map(|(i, b)| seed(target, format!("golden-{file}-{i:02}"), b))
        .collect()
}

/// The golden sample columns as `tape-samples` inputs: `[codec][element][channels - 1][count u16][from u16][column]`.
fn golden_samples() -> Vec<Seed> {
    let mut out = vec![];
    for (i, line) in GOLDEN_SAMPLES.lines().enumerate() {
        let Some((head, hex)) = line.split_once(':') else {
            continue;
        };
        let f: Vec<&str> = head.split_whitespace().collect();
        let [codec, element, channels, count] = f[..] else {
            continue;
        };
        let codec: u8 = codec.parse().expect("a codec id");
        let codec = CODECS
            .iter()
            .position(|c| c.0 == codec)
            .expect("a known codec") as u8;
        let element = Element::ALL
            .iter()
            .position(|e| format!("{e:?}") == element)
            .expect("a known element") as u8;
        let channels: u8 = channels.parse().expect("channels");
        let count: u16 = count.parse().expect("a count");
        let mut b = vec![codec, element, channels - 1];
        b.extend_from_slice(&count.to_le_bytes());
        b.extend_from_slice(&0u16.to_le_bytes());
        b.extend(unhex(hex.trim()));
        out.push(seed("tape-samples", format!("golden-v1-samples-{i:02}"), b));
    }
    out
}

/// The describe records of series tracks in the golden series blocks.
fn golden_series() -> Vec<Seed> {
    let mut out = vec![];
    for b in golden(GOLDEN_SERIES) {
        let Ok(block) = Block::read(&b) else { continue };
        let mut plain = Vec::new();
        let Ok(decoded) = block.decode(&mut plain) else {
            continue;
        };
        for r in decoded.records().flatten() {
            let name = match Describe::from_ww_bytes(r.item) {
                Ok(Describe::Track(t)) if matches!(t.kind, TrackKind::Series(_)) => "track",
                _ => continue,
            };
            let n = out.len();
            out.push(seed(
                "tape-series",
                format!("golden-v1-series-{name}-{n:02}"),
                r.item.to_vec(),
            ));
        }
    }
    out
}

/// The golden grids: each piece as a `tape-reader` input, its grid definition as a `tape-grid` input and its chunks
/// (tile coordinates after the scale) as `tape-chunk` inputs.
fn golden_grids() -> Vec<Seed> {
    let mut out = vec![];
    for (n, text) in GOLDEN_GRIDS {
        let blocks = golden(text);
        out.push(seed(
            "tape-reader",
            format!("golden-v1-grid-{n}"),
            blocks.concat(),
        ));
        for (i, b) in blocks.iter().enumerate() {
            let Ok(block) = Block::read(b) else { continue };
            if block.chunk().is_some() {
                out.push(seed(
                    "tape-chunk",
                    format!("golden-v1-grid-{n}-{i:02}"),
                    b.clone(),
                ));
                continue;
            }
            let mut plain = Vec::new();
            let Ok(decoded) = block.decode(&mut plain) else {
                continue;
            };
            for r in decoded.records().flatten() {
                if let Ok(Describe::Track(t)) = Describe::from_ww_bytes(r.item)
                    && let TrackKind::Series(g) = &t.kind
                {
                    let mut buf = vec![0u8; SEED_LEN_MAX];
                    let def = g.to_ww_bytes(&mut buf).expect("a grid fits a seed");
                    out.push(seed(
                        "tape-grid",
                        format!("golden-v1-grid-{n}-def"),
                        def.to_vec(),
                    ));
                }
            }
        }
    }
    out
}

/// The flag runs columns of the golden series chunks as `tape-flags` inputs: `[count u32][column]`.
fn golden_flags() -> Vec<Seed> {
    let mut out = vec![];
    for b in golden(GOLDEN_SERIES) {
        let Some(c) = Block::read(&b).ok().and_then(|b| b.chunk()) else {
            continue;
        };
        let Some(col) = c.find(ColumnKind::Flags, 0) else {
            continue;
        };
        let mut input = c.head().count.to_le_bytes().to_vec();
        input.extend_from_slice(col.bytes);
        let n = out.len();
        out.push(seed(
            "tape-flags",
            format!("golden-v1-series-runs-{n:02}"),
            input,
        ));
    }
    out
}

/// Describe records of events tracks of plain bytes, a media type, a named format with a schema and numbers, and the
/// schema itself (TRACK-4), for the definition decoder.
fn encoding_seeds() -> Vec<Seed> {
    const SCHEMA: &[u8] = b"syntax = \"proto3\"; message Frame { uint32 id = 1; bytes data = 2; }";
    let hash = *blake3::hash(SCHEMA).as_bytes();
    let schema = ww_tape::Schema {
        hash: RefVec::Slice { slice: &hash },
        media_type: "text/x-protobuf",
        bytes: RefVec::Slice { slice: SCHEMA },
    };
    let encodings = [
        ("bytes", ww_tape::Items::RawBytes),
        (
            "pcap",
            ww_tape::Items::Foreign(ww_tape::Foreign {
                name: "",
                media_type: "application/vnd.tcpdump.pcap",
                version: "",
                schema: RefVec::Slice { slice: &[] },
            }),
        ),
        (
            "named",
            ww_tape::Items::Foreign(ww_tape::Foreign {
                name: "com.example.frame",
                media_type: "",
                version: "3",
                schema: RefVec::Slice { slice: &hash },
            }),
        ),
        (
            "numbers",
            ww_tape::Items::Numbers {
                element: ww_tape::ElementDesc {
                    bits: 8,
                    exponent_bits: 0,
                    signed: false,
                    msb_first: false,
                    parts: UNib32(2),
                    zero: ww_tape::Ratio64::new(255, 2),
                },
                unit: None,
            },
        ),
    ];
    let mut out: Vec<Seed> = encodings
        .into_iter()
        .map(|(name, items)| {
            let t = ww_tape::TrackDef {
                number: TrackNum::new(1),
                key: TrackKey::new(0x7261_7774),
                clock: ClockNum::new(1),
                dimensions: RefVec::Slice { slice: &[] },
                items,
                label: "raw",
                repeat: ww_tape::TrackRepeat::Stream,
                kind: TrackKind::Events(ww_tape::EventsDef::INSTANTS),
                relations: RefVec::Slice { slice: &[] },
                owner: [0; 32],
            };
            seed(
                "tape-series",
                format!("encoding-{name}"),
                definition(&Describe::Track(t)),
            )
        })
        .collect();
    out.push(seed(
        "tape-series",
        "encoding-schema",
        definition(&Describe::Schema(schema)),
    ));
    out
}

/// A chunk block of `cols` with `head`.
fn chunk_block(enc: &EncodedTimes) -> Vec<u8> {
    let cols: Vec<_> = enc.columns().collect();
    let specs: Vec<_> = cols.iter().map(|c| c.0).collect();
    let head = enc
        .head
        .chunk_head(TrackId::new(1), TrackNum::new(1), ww_tape::ItemNum::new(0));
    let mut buf = vec![0u8; ww_tape::chunk::chunk_len(&head, &specs, 0)];
    let n = ww_tape::chunk::write_chunk(
        &mut buf,
        BlockId::new(SessionTag::writable(1), 1),
        &head,
        &cols,
        0,
    )
    .expect("a valid chunk");
    buf.truncate(n);
    buf
}

/// The block offset of the first byte of column `kind`.
fn column_start(b: &[u8], kind: ColumnKind) -> usize {
    let block = Block::read(b).expect("written good");
    let chunk = block.chunk().expect("a chunk");
    let col = chunk
        .columns()
        .find(|c| c.kind == kind)
        .expect("the column");
    chunk.payload_offset() + col.offset
}

fn le(b: &[u8], at: usize, n: usize) -> usize {
    let mut v = [0u8; 8];
    v[..n].copy_from_slice(&b[at..at + n]);
    usize::from_le_bytes(v)
}

/// fix 32f75a289 and c1d25a45b: times whose group records or values were damaged after the CRCs, read by salvage.
fn times_fixes() -> Vec<Seed> {
    let mut out = vec![];
    // 33 groups of linear starts with jitter
    let s: Vec<i64> = (0..33 * 128i64).map(|i| i * 1000 + (i * i) % 17).collect();
    let good = chunk_block(&EncodedTimes::encode(&s, None, None).expect("valid times"));
    let p = column_start(&good, ColumnKind::Starts);
    let rec = |g: usize, at: usize| p + g * 25 + at;
    let residuals = |g: usize| p + 33 * 25 + le(&good, rec(g, 21), 4);
    let w31 = usize::from(good[rec(31, 20)]);
    let top = 64 * w31 + w31 - 1;
    let flips = [
        ("residual-minimum", rec(5, 16), 0x01),
        ("item-0-residual", residuals(5), 0x01),
        ("first-tick", rec(5, 7), 0x01),
        ("first-tick-order", rec(5, 3), 0x80),
        ("slope", rec(5, 14), 0x01),
        (
            "residual-past-last-tick",
            residuals(31) + top / 8,
            1 << (top % 8),
        ),
    ];
    for (what, at, bit) in flips {
        let mut b = good.clone();
        b[at] ^= bit;
        out.push(seed("tape-times", format!("fix-32f75a289-{what}"), b));
    }
    // durations and flags kept in bounds
    let s: Vec<i64> = (0..300i64).map(|i| i * 1000 + (i * i) % 17).collect();
    let spans: Vec<i64> = s.iter().zip(0..).map(|(x, i)| x + i % 100).collect();
    let flags: Vec<u32> = (0..300).map(|i| u32::from(i % 4 == 0) * 2).collect();
    let good = chunk_block(&EncodedTimes::encode(&s, Some(&spans), Some(&flags)).expect("valid"));
    let mut b = good.clone();
    let e = column_start(&good, ColumnKind::Ends);
    b[e + 2 * 13 + 2] ^= 0x04;
    let f = column_start(&good, ColumnKind::Flags);
    let f_off = le(&good, f + 8 + 4, 2);
    b[f + 3 * 8 + f_off + 64 * 2 / 8] ^= 1;
    out.push(seed("tape-times", "fix-32f75a289-duration-and-flag", b));
    // a group's values past the column: group 3's offset one past the column's end
    let mut b = good.clone();
    let col_len = good.len() - 4 - 16 - p;
    b[rec(3, 21)..rec(3, 25)].copy_from_slice(&(col_len as u32 + 1).to_le_bytes());
    out.push(seed("tape-times", "fix-c1d25a45b-offset-past-column", b));
    // gaps of 2^63 ticks and more
    let wide = EncodedTimes::encode(&[i64::MIN, -1, 0, i64::MAX], None, None).expect("valid");
    out.push(seed(
        "tape-times",
        "fix-60e5d677c-gaps-past-i64",
        chunk_block(&wide),
    ));
    out
}

/// fix 1cc795107: lz4 columns stating decoded lengths past what their bytes can give, as `tape-samples` inputs.
fn lz4_fixes() -> Vec<Seed> {
    let max = (LZ4_DECODED_MAX as u32).to_le_bytes();
    let columns: [(&str, [u8; 5]); 3] = [
        ("max-length-in-5-bytes", [max[0], max[1], max[2], max[3], 0]),
        ("bound-271", [15, 1, 0, 0, 0]),
        ("bound-272", [16, 1, 0, 0, 0]),
    ];
    columns
        .into_iter()
        .map(|(what, col)| {
            // raw u8 samples, no count: the bytes go to the lz4 decoder
            let mut b = vec![0, 4, 0, 0, 0, 0, 0];
            b.extend_from_slice(&col);
            seed("tape-samples", format!("fix-1cc795107-{what}"), b)
        })
        .collect()
}

const SESSION: SessionTag = SessionTag::writable(0x600d_0002);
const OLD: SessionTag = SessionTag::writable(0x0bad_0001);

fn tail(piece_len: u64, close_offset: u64, close_len: u32) -> Vec<u8> {
    let t = Tail {
        piece_len,
        close_offset,
        close_len,
        reason: CloseReason::Closed,
        hash: ContentHash([0; 32]),
        ident: None,
        next: None,
    };
    let mut b = vec![0u8; TAIL_LEN];
    t.write(&mut b, BlockId::new(SESSION, 0));
    b
}

/// A close naming `final_index`, then its tail.
fn close_only(final_index: u64, piece_len: Option<u64>) -> Vec<u8> {
    let close = Close {
        reason: CloseReason::Closed,
        hash: ContentHash([0; 32]),
        checkpoint: None,
        final_index: Some(final_index),
        prev: None,
        summary: None,
        ident: None,
        next: None,
    };
    let mut b = vec![0u8; close_len(0, 0)];
    let n = write_close(&mut b, BlockId::new(SESSION, 0), &close, (&[], &[]), 0).expect("a close");
    b.truncate(n);
    let len = piece_len.unwrap_or((n + TAIL_LEN) as u64);
    b.extend(tail(len, 0, n as u32));
    b
}

/// fix baf02dd6d, 208b9b90f, 5c6fa63a7: tails and closes whose offsets point past the piece or wrap.
fn index_fixes() -> Vec<Seed> {
    let mut out = vec![seed(
        "tape-index",
        "fix-baf02dd6d-tail-offsets-wrap",
        tail(TAIL_LEN as u64, u64::MAX - 343, 180),
    )];
    for (what, at) in [
        ("max-10", u64::MAX - 10),
        ("max-40", u64::MAX - 40),
        ("1-tib", 1 << 40),
        ("8", 8),
    ] {
        out.push(seed(
            "tape-index",
            format!("fix-208b9b90f-final-index-at-{what}"),
            close_only(at, None),
        ));
    }
    out.push(seed(
        "tape-index",
        "fix-5c6fa63a7-piece-of-5-gib",
        close_only(0, Some(5 << 30)),
    ));
    out
}

fn records(id: BlockId) -> Vec<u8> {
    let mut buf = [0u8; 256];
    let mut b =
        BlockWriter::new(&mut buf, BlockKind::Records, id, &[ClockNum::new(1)]).expect("a block");
    b.push(
        TrackNum::new(9),
        ClockTime::new(ClockNum::new(1), id.seq as i64),
        &[&[id.seq as u8; 40]],
    )
    .expect("fits");
    let len = b.finish();
    buf[..len].to_vec()
}

/// A track-0 item: `d` encoded.
fn definition(d: &Describe<'_>) -> Vec<u8> {
    ww_tape::open::encode(d).expect("encodes")
}

/// A block of definitions: a producer and `layout`; with `put`, a block of puts (FLAG_REPEAT).
fn describe(id: BlockId, layout: Layout, put: bool) -> Vec<u8> {
    let mut buf = [0u8; 256];
    let mut w = BlockWriter::new(&mut buf, BlockKind::Records, id, &[]).expect("a block");
    if put {
        w.set_flags(FLAG_REPEAT);
    }
    let producer = definition(&Describe::Producer(Producer { name: "media" }));
    w.push_describe(&[&producer]).expect("fits");
    w.push_describe(&[&definition(&Describe::Layout(layout))])
        .expect("fits");
    let len = w.finish();
    buf[..len].to_vec()
}

fn layout(gap_max: u32, resume_align: u32) -> Layout {
    Layout {
        gap_max: exact_units::Bytes::new(gap_max),
        resume_align: exact_units::Bytes::new(resume_align),
        ..Layout::UNALIGNED
    }
}

/// A session on a medium: a piece head, a block of definitions with `layout`, then `n` records blocks.
fn session(layout: Layout, n: u64) -> Vec<u8> {
    let mut out = vec![0u8; piece_head_len(0)];
    let head = PieceHead {
        ident: PieceIdent {
            id: PieceId([2; 16]),
            owner: OwnerId([1; 32]),
            seq: 0,
        },
        prev: None,
        prev_session: SessionTag::NONE,
        integrity: ww_tape::Integrity::Full,
    };
    let len = write_piece_head(&mut out, BlockId::new(SESSION, 0), &head, &[]).expect("a head");
    out.truncate(len);
    out.extend(describe(BlockId::new(SESSION, 0), layout, false));
    for seq in 1..=n {
        out.extend(records(BlockId::new(SESSION, seq)));
    }
    out
}

/// fix 74e52ec33, 23ceb1731, 4205a405e and the definitions and gap fixes: media with gaps, erased space, stale
/// blocks and layouts past the limits.
fn media_fixes() -> Vec<Seed> {
    let mut out = vec![];
    // layouts a reader refuses, then one more byte
    for (what, l) in [
        ("gap-max-u32", layout(u32::MAX, 1)),
        ("gap-past-max", layout(GAP_MAX_MAX + 512, 512)),
        ("probes-past-max", layout(GAP_PROBES_MAX + 1, 1)),
        ("resume-align-0", layout(512, 0)),
    ] {
        let mut b = describe(BlockId::new(SESSION, 0), l, false);
        b.push(0x41);
        out.push(seed(
            "tape-reader",
            format!("fix-74e52ec33-layout-{what}"),
            b,
        ));
    }
    // a bad block, then the header of the session's next block, 1 MiB long by its header, on every sector
    let card = session(layout(32 << 10, 512), 6);
    let mut header = records(BlockId::new(SESSION, 100))[..32].to_vec();
    header[8..12].copy_from_slice(&(1u32 << 20).to_le_bytes());
    let mut b = card.clone();
    b.extend_from_slice(&[0x5a; 100]);
    while b.len() < card.len() + (40 << 10) {
        if b.len().is_multiple_of(512) {
            b.extend_from_slice(&header);
        } else {
            b.push(0x5a);
        }
    }
    out.push(seed("tape-reader", "fix-74e52ec33-a-header-per-sector", b));
    // erased space longer than the gap, then a stale block of an older session
    let mut b = session(layout(4096, 512), 6);
    let erased = b.len() + (16 << 10);
    b.resize(erased, 0xff);
    b.extend(records(BlockId::new(OLD, 9)));
    out.push(seed("tape-reader", "fix-23ceb1731-erased-past-the-gap", b));
    // a checkpoint, then a blank sector before the next records block
    let mut b = session(layout(4096, 512), 20);
    let mut entries = vec![];
    let mut at = 0;
    while let Ok(len) = ww_tape::block_len(&b[at..]) {
        let block = Block::read(&b[at..at + len]).expect("written good");
        entries.extend(IndexEntry::of_block(at as u64, &block));
        at += len;
    }
    let mut ix = vec![0u8; index_len(entries.len(), 0)];
    let head = IndexHead {
        kind: IndexKind::Checkpoint,
        self_offset: b.len() as u64,
        prev: None,
    };
    let n = write_index(&mut ix, BlockId::new(SESSION, 20), &head, &entries).expect("an index");
    b.extend_from_slice(&ix[..n]);
    b.extend_from_slice(&[0; 512]);
    for seq in 21..26 {
        b.extend(records(BlockId::new(SESSION, seq)));
    }
    out.push(seed(
        "tape-reader",
        "fix-4205a405e-gap-after-checkpoint",
        b.clone(),
    ));
    out.push(seed("tape-index", "fix-4205a405e-gap-after-checkpoint", b));
    // the layout only in a block of puts, before the first counted block
    let mut b = describe(BlockId::new(SESSION, 0), layout(4096, 512), true);
    b.extend(records(BlockId::new(SESSION, 0)));
    out.push(seed("tape-reader", "layout-put-before-data", b));
    // a block written again after a gap
    let mut b = session(layout(4096, 512), 3);
    b.resize(b.len().next_multiple_of(512) + 512, 0);
    b.extend(records(BlockId::new(SESSION, 3)));
    b.extend(records(BlockId::new(SESSION, 4)));
    out.push(seed(
        "tape-reader",
        "fix-67e392294-duplicate-after-a-gap",
        b,
    ));
    out
}

/// Inputs of `tape-resume`: each template whole, its last piece cut halfway, and cut with a flipped byte in the last
/// piece's head definitions; then a stream.
fn resume_seeds() -> Vec<Seed> {
    let mut out = vec![];
    for mode in 0..8u8 {
        out.push(seed("tape-resume", format!("mode{mode}-whole"), vec![mode]));
        out.push(seed(
            "tape-resume",
            format!("mode{mode}-half"),
            vec![mode, 0x00, 0x30],
        ));
        out.push(seed(
            "tape-resume",
            format!("mode{mode}-head-definitions-flipped"),
            vec![mode, 0x00, 0x30, 0xc8, 0x00, 0x01],
        ));
    }
    // cut halfway at CrcOnly (+8) and None (+16): a piece with a head, aligned or not, and a chain, open or closed
    for mode in [8, 9, 12, 14, 16, 17, 20, 22u8] {
        out.push(seed(
            "tape-resume",
            format!("mode{mode}-half"),
            vec![mode, 0x00, 0x30],
        ));
    }
    out.extend(stream("tape-resume", 0x5eed_0000_7e50_0001));
    out
}

/// Inputs of `tape-reclose`: each mode with appends of a few blocks whole; a few modes with the last append cut
/// halfway, with a flipped byte in its last close record, and with one in its first stretch; then a stream. The pieces closed more than
/// once, whole and torn in their last append, are inputs of `tape-reader` too.
fn reclose_seeds() -> Vec<Seed> {
    let mut out = vec![];
    for mode in 0..16u8 {
        let count = usize::from((mode >> 2) & 3) + 1;
        let appends: Vec<u8> = (0..count).map(|k| 3 + k as u8).collect();
        let input = |rest: &[u8]| [&[mode][..], &appends, rest].concat();
        out.push(seed(
            "tape-reclose",
            format!("mode{mode}-whole"),
            input(&[]),
        ));
        // cut and flipped: one append or four, aligned or not, flips after the closes or anywhere
        if ![0, 1, 2, 13].contains(&mode) {
            continue;
        }
        out.push(seed(
            "tape-reclose",
            format!("mode{mode}-half"),
            input(&[0x00, 0x04]),
        ));
        out.push(seed(
            "tape-reclose",
            format!("mode{mode}-last-close-flipped"),
            input(&[0xff, 0xff, 0x00, 0x05, 0x10]),
        ));
        out.push(seed(
            "tape-reclose",
            format!("mode{mode}-first-stretch-flipped"),
            input(&[0xff, 0xff, 0xc8, 0x00, 0x01]),
        ));
    }
    out.extend(stream("tape-reclose", 0x5eed_0000_7ec1_0001));
    let (whole, made) = crate::targets::reclosed(false, &[2, 0, 5]);
    let kept = made[made.len() - 2].end;
    out.push(seed("tape-reader", "closed-four-times", whole.clone()));
    out.push(seed(
        "tape-reader",
        "closed-four-times-last-append-torn",
        whole[..kept + (whole.len() - kept) / 2].to_vec(),
    ));
    out.push(seed(
        "tape-reader",
        "closed-four-times-append-head-torn",
        whole[..kept + 20].to_vec(),
    ));
    out.push(seed(
        "tape-reader",
        "closed-with-a-close-summary",
        crate::targets::summarized(),
    ));
    out
}

/// Inputs of the file readers (`tape-reader`, `tape-index`): a file of several pieces whole, its last piece torn
/// halfway, its last piece with a flipped byte in its head and one in its middle, and the file without its second
/// piece, its last piece naming that one removed.
fn chain_seeds() -> Vec<Seed> {
    let (whole, last) = crate::targets::chain_template(
        crate::targets::ChainTemplateMode {
            aligned: false,
            closed: true,
        },
        ww_tape::Integrity::Full,
    );
    let half = last + (whole.len() - last) / 2;
    let torn = whole[..half].to_vec();
    let mut damaged = whole.clone();
    damaged[last + 160] ^= 0x40;
    damaged[half] ^= 0x01;
    let mut out = vec![];
    for target in ["tape-reader", "tape-index"] {
        out.push(seed(target, "one-file-chain", whole.clone()));
        out.push(seed(target, "one-file-chain-last-torn", torn.clone()));
        out.push(seed(target, "one-file-chain-last-damaged", damaged.clone()));
        out.push(seed(
            target,
            "one-file-chain-a-piece-removed",
            crate::targets::removed_chain(),
        ));
    }
    out
}

/// xorshift64 inputs of a hostile target's stream.
fn stream(target: &'static str, mut x: u64) -> Vec<Seed> {
    let mut out = vec![seed(target, "zeros", vec![0; STREAM_INPUT_LEN])];
    for i in 0..STREAM_SEEDS {
        let bytes = (0..STREAM_INPUT_LEN)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect();
        out.push(seed(target, format!("stream-{i:02}"), bytes));
    }
    out
}

/// Grid files as `tape-grid` inputs: the first [GRID_FILE_SEEDS] hostile grid files of a stream with tile summaries a
/// region's timeline merges, then the first [GRID_FILE_SEEDS] with overview chunks whose columns read.
fn grid_files() -> Vec<Seed> {
    let mut x = 0x9e37_79b9_7f4a_7c15_u64;
    let (mut nodes, mut overviews) = (vec![], vec![]);
    for _ in 0..GRID_FILE_INPUTS {
        let input: Vec<u8> = (0..STREAM_INPUT_LEN)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect();
        let file = crate::hostile::grid(&input);
        let reach = crate::targets::grid_reach(&file);
        if reach.nodes > 0 && nodes.len() < GRID_FILE_SEEDS {
            let name = format!("grid-file-{:02}", nodes.len());
            nodes.push(seed("tape-grid", name, file));
        } else if reach.overviews > 0 && overviews.len() < GRID_FILE_SEEDS {
            let name = format!("grid-overview-{:02}", overviews.len());
            overviews.push(seed("tape-grid", name, file));
        }
        if nodes.len() == GRID_FILE_SEEDS && overviews.len() == GRID_FILE_SEEDS {
            break;
        }
    }
    nodes.extend(overviews);
    nodes
}

/// Grid files of each kind among the `tape-grid` seeds.
const GRID_FILE_SEEDS: usize = 8;
/// Most stream inputs drawn for the grid files: about one in a thousand reads overview chunks.
const GRID_FILE_INPUTS: usize = 1 << 15;

/// Inputs of `tape-stores`: each mode with a run of writes, a cut, a crash, a close and a removal in turn; then a
/// stream.
fn stores_seeds() -> Vec<Seed> {
    let mut out = vec![];
    for mode in 0..4u8 {
        let ops = [
            0x00, 0x80, 0x04, 0x00, 0x05, 0x80, 0x06, 0x00, 0x04, 0x80, 0x07, 0x00, 0x00,
        ];
        out.push(seed(
            "tape-stores",
            format!("mode{mode}-each-op"),
            [&[mode][..], &ops].concat(),
        ));
    }
    out.extend(stream("tape-stores", 0x5eed_0000_5707_0001));
    out
}

/// Inputs of `tape-seal`: each mode unmutated, each with a flip in the middle of the tape; then a stream.
fn seal_seeds() -> Vec<Seed> {
    let mut out = vec![];
    for mode in 0..16u8 {
        out.push(seed("tape-seal", format!("mode{mode:02}"), vec![mode]));
        out.push(seed(
            "tape-seal",
            format!("mode{mode:02}-flip"),
            vec![mode, 0x00, 0x08, 0x10],
        ));
    }
    out.extend(stream("tape-seal", 0x5eed_0000_5ea1_0001));
    out
}

/// Every seed file of every target.
pub fn all() -> Vec<Seed> {
    let mut out = vec![];
    let concat = |text: &str| golden(text).concat();
    out.push(seed("tape-reader", "golden-v1-file", concat(GOLDEN_V1)));
    out.push(seed(
        "tape-reader",
        "golden-v1-level-crc-only",
        concat(GOLDEN_CRC_ONLY),
    ));
    out.push(seed(
        "tape-reader",
        "golden-v1-level-none",
        concat(GOLDEN_NONE),
    ));
    out.push(seed(
        "tape-reader",
        "golden-v1-index-piece",
        concat(GOLDEN_INDEX),
    ));
    out.extend(media_fixes());
    out.extend(resume_seeds());
    out.extend(reclose_seeds());
    out.extend(stores_seeds());
    out.extend(seal_seeds());
    out.extend(chain_seeds());
    out.extend(golden_blocks("tape-chunk", "v1-chunks", GOLDEN_CHUNKS));
    out.extend(golden_blocks("tape-index", "v1-index", GOLDEN_INDEX));
    out.push(seed(
        "tape-index",
        "golden-v1-index-piece",
        concat(GOLDEN_INDEX),
    ));
    out.extend(index_fixes());
    out.extend(golden_blocks(
        "tape-payloads",
        "v1-payloads",
        GOLDEN_PAYLOADS,
    ));
    out.extend(golden_samples());
    out.extend(lz4_fixes());
    out.extend(golden_series());
    out.extend(golden_flags());
    out.extend(golden_grids());
    out.extend(encoding_seeds());
    out.extend(golden_blocks("tape-summary", "v1-summary", GOLDEN_SUMMARY));
    out.extend(golden_blocks(
        "tape-summary",
        "v1-grid-summary",
        GOLDEN_GRID_SUMMARY,
    ));
    out.extend(golden_blocks("tape-times", "v1-times", GOLDEN_TIMES));
    out.extend(times_fixes());
    out.extend(grid_files());
    for (i, (name, _)) in crate::targets::ALL.iter().enumerate() {
        if name.ends_with("-hostile") {
            out.extend(stream(name, 0x5eed_0000_0000_0001 + i as u64 * 0x9e37_79b9));
        }
    }
    for (inputs, prefix) in [(KNOWN_INPUTS, "known"), (FIXED_INPUTS, "fixed")] {
        for (target, name, hex) in inputs {
            let target = crate::targets::ALL
                .iter()
                .find(|(t, _)| t == target)
                .expect("a known target")
                .0;
            out.push(seed(target, format!("{prefix}-{name}"), unhex(hex)));
        }
    }
    out
}

/// Write every seed into `dir/<target>/<name>`, replacing the files there; returns how many.
pub fn write(dir: &Path) -> std::io::Result<usize> {
    let seeds = all();
    for (target, _) in crate::targets::ALL {
        let d = dir.join(target);
        if d.exists() {
            std::fs::remove_dir_all(&d)?;
        }
        std::fs::create_dir_all(&d)?;
    }
    for s in &seeds {
        std::fs::write(dir.join(s.target).join(&s.name), &s.bytes)?;
    }
    Ok(seeds.len())
}
