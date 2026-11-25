//! The numbers of the book's Part III, writing, from the drive's full hour (`docs/generated/writing.txt`, and the two
//! tables the pages include, `writing_calls.txt` and `writing_blocks.txt`): the calls and the blocks
//! they make, the aligned file, the columns per codec, what the crash loses per sync policy, the cut, the chain with
//! its first piece deleted, and the relay.
#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    reason = "tests fail by panicking"
)]

use example_drive::Drive;
use example_drive::writing::{Writing, round_bytes, round_percent};
use ww_tape::SyncPolicy;

#[test]
fn book_output_part_iii_prints_what_the_chapters_quote() {
    let recording = Drive::full().record().expect("the hour records");
    // the story's crash and resume on a thread of its own, beside the numbers
    let (w, story) = std::thread::scope(|s| {
        let story = s.spawn(|| recording.story().expect("the story"));
        let w = Writing::of(&recording).expect("the numbers");
        (w, story.join().expect("the story's thread"))
    });
    ww_tape_book_outputs::check("writing.txt", &w.to_string());
    ww_tape_book_outputs::check("writing_calls.txt", &w.calls_table());
    ww_tape_book_outputs::check("writing_blocks.txt", &w.blocks_table());
    rounded_on_the_pages(&w);

    let every_block = w
        .syncs
        .iter()
        .find(|s| s.policy == SyncPolicy::EveryBlocks(1))
        .expect("EveryBlocks(1) is compared");
    assert_eq!(
        every_block.bytes,
        story.crash.offset - story.kept_bytes,
        "a sync after every block loses only what the resume cuts off"
    );
    let none = w.syncs.last().expect("None is compared");
    assert_eq!(
        (none.policy, none.bytes),
        (SyncPolicy::None, story.crash.offset),
        "without syncs the crash may lose the whole piece it tears"
    );
    for (len, _) in &w.cut.pieces {
        assert_eq!(len % 4096, 0, "a closed piece ends on 4 KiB");
    }
    assert!(
        w.retention.head_cut && !w.retention.whole,
        "a chain without its first piece reads as head cut"
    );
    assert_eq!(w.retention.linked, w.retention.pieces - 1);
    assert!(
        w.relay.same_data,
        "the relay writes the board's data blocks as they are"
    );
    assert!(
        w.relay.relay_puts < w.relay.board_puts,
        "the relay drops the board's puts and places its own by its near"
    );
}

/// A book page with its line breaks as spaces.
fn page(name: &str) -> String {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/").to_owned() + name;
    let text = std::fs::read_to_string(&path).expect("a book page");
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The prose around the included tables gives their numbers rounded (docs/writers.md, "Numbers").
fn rounded_on_the_pages(w: &Writing) {
    let (writer, total) = (w.writer_bytes(), w.total_bytes());
    let puts: Vec<_> = w
        .blocks
        .iter()
        .filter(|b| b.by_writer && b.part.contains("puts"))
        .collect();
    let (put_blocks, put_bytes): (u64, u64) = (
        puts.iter().map(|b| b.count).sum(),
        puts.iter().map(|b| b.bytes).sum(),
    );
    for (name, phrase) in [
        (
            "writing.md",
            format!(
                "Only {} of the {} puts wrote a record",
                w.calls.puts_wrote, w.calls.puts
            ),
        ),
        (
            "writer_adds.md",
            format!(
                "take {} of the hour's {}, {}",
                round_bytes(writer),
                round_bytes(total),
                round_percent(writer, total)
            ),
        ),
        (
            "writer_adds.md",
            format!(
                "The {put_blocks} blocks of puts hold every definition and mode change of four pieces, with their \
                 copies, in {}.",
                round_bytes(put_bytes)
            ),
        ),
        (
            "damaged_pieces.md",
            format!(
                "The drive's hour spends {} of its {} on all its puts and copies",
                round_bytes(put_bytes),
                round_bytes(total)
            ),
        ),
    ] {
        assert!(page(name).contains(&phrase), "docs/{name} says {phrase:?}");
    }
}
