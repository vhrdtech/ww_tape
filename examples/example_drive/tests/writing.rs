//! The numbers of the book's Part III, writing, from the drive's full hour (`writing.out`): the calls and the blocks
//! they make, the aligned file, the columns per codec, what the crash loses per sync policy, the cut, the chain with
//! its first piece deleted, and the relay.
#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    reason = "tests fail by panicking"
)]

use std::path::Path;

use example_drive::Drive;
use example_drive::writing::Writing;
use ww_tape::SyncPolicy;

#[test]
fn part_iii_prints_what_the_chapters_quote() {
    let recording = Drive::full().record().expect("the hour records");
    // the story's crash and resume on a thread of its own, beside the numbers
    let (w, story) = std::thread::scope(|s| {
        let story = s.spawn(|| recording.story().expect("the story"));
        let w = Writing::of(&recording).expect("the numbers");
        (w, story.join().expect("the story's thread"))
    });
    let want =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/writing.out"))
            .expect("tests/writing.out");
    assert_eq!(
        w.to_string(),
        want,
        "tests/writing.out is what the program prints from `Writing the drive's hour` on: copy it from `cargo run -p example_drive`"
    );

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
