//! The one rule by which a reader and a resume decide which blocks of puts make a group of puts
//! ([TapeWriter::put_group](crate::TapeWriter::put_group)): the marks of two blocks of puts one right after the other.
//!
//! A group is a block marked [FLAG_GROUP] and [FLAG_GROUP_START], then blocks marked [FLAG_GROUP] without a start
//! right after it, each block but the last marked [FLAG_WITH_NEXT]. A group ends whole at its first block not marked
//! [FLAG_WITH_NEXT], or where a block of its run follows that is no block of puts (a chunk the run belongs with). A
//! block of puts right after a marked block of a group that doesn't go on the group breaks it, and so does a block of
//! a group without a start right after a block that opens no group: a writer makes neither, so the bytes lie, and the
//! group is dropped as after damage.

use crate::{FLAG_GROUP, FLAG_GROUP_START, FLAG_REPEAT, FLAG_WITH_NEXT};

/// The marks of a block of puts ([FLAG_REPEAT]) that say which group it is of.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct Marks {
    /// Marked [FLAG_GROUP]: a block of a group
    pub(crate) group: bool,
    /// Marked [FLAG_GROUP_START]: a group's first block
    pub(crate) start: bool,
    /// Marked [FLAG_WITH_NEXT]: a block of the run goes on after it
    pub(crate) with_next: bool,
}

impl Marks {
    /// The marks of a block with header flags `flags`.
    pub(crate) fn of(flags: u16) -> Self {
        Self {
            group: flags & FLAG_GROUP != 0,
            start: flags & FLAG_GROUP_START != 0,
            with_next: flags & FLAG_WITH_NEXT != 0,
        }
    }

    /// The marks of a records block with header flags `flags`, None for a block of no puts (no [FLAG_REPEAT]).
    pub(crate) fn of_puts(flags: u16) -> Option<Self> {
        (flags & FLAG_REPEAT != 0).then(|| Self::of(flags))
    }

    /// A group's block after which the group goes on.
    fn opens_on(self) -> bool {
        self.group && self.with_next
    }

    /// A block that goes on the group of the block before it.
    fn goes_on(self) -> bool {
        self.group && !self.start
    }
}

/// How the block `next` right after the block `prev` relates to `prev`'s group: each the marks of a block of puts,
/// None for any other block or none.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum Link {
    /// `next` goes on `prev`'s group.
    Within,
    /// The bytes break the group: `prev`'s group open, `next` a block of puts that doesn't go on it, or `next` a
    /// block of a group without a start after a block that opens none. Both groups are dropped.
    Breaks,
    /// `next` is no block of `prev`'s group: a group `prev` leaves open ends whole before it.
    Apart,
}

/// How block `next` right after block `prev` relates to `prev`'s group ([Link]).
pub(crate) fn link(prev: Option<Marks>, next: Option<Marks>) -> Link {
    let open = prev.is_some_and(Marks::opens_on);
    match (open, next) {
        (true, Some(n)) if n.goes_on() => Link::Within,
        (true, Some(_)) => Link::Breaks,
        (false, Some(n)) if n.goes_on() => Link::Breaks,
        _ => Link::Apart,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIRST: Marks = Marks {
        group: true,
        start: true,
        with_next: true,
    };
    const MIDDLE: Marks = Marks {
        group: true,
        start: false,
        with_next: true,
    };
    const LAST: Marks = Marks {
        group: true,
        start: false,
        with_next: false,
    };
    const PUTS: Marks = Marks {
        group: false,
        start: false,
        with_next: false,
    };

    #[test]
    fn a_group_links_its_blocks() {
        assert_eq!(link(Some(FIRST), Some(MIDDLE)), Link::Within);
        assert_eq!(link(Some(MIDDLE), Some(LAST)), Link::Within);
        assert_eq!(link(Some(LAST), Some(PUTS)), Link::Apart);
        assert_eq!(link(Some(LAST), Some(FIRST)), Link::Apart);
        // a chunk after a marked block ends the run, and the group whole
        assert_eq!(link(Some(MIDDLE), None), Link::Apart);
    }

    #[test]
    fn forged_marks_break_a_group() {
        assert_eq!(link(Some(FIRST), Some(PUTS)), Link::Breaks);
        assert_eq!(link(Some(MIDDLE), Some(FIRST)), Link::Breaks);
        assert_eq!(link(Some(PUTS), Some(LAST)), Link::Breaks);
        assert_eq!(link(None, Some(MIDDLE)), Link::Breaks);
        let marked_puts = Marks {
            with_next: true,
            ..PUTS
        };
        assert_eq!(link(Some(marked_puts), Some(LAST)), Link::Breaks);
    }

    #[test]
    fn marks_read_from_flags() {
        assert_eq!(Marks::of_puts(0), None);
        let all = FLAG_REPEAT | FLAG_GROUP | FLAG_GROUP_START | FLAG_WITH_NEXT;
        assert_eq!(Marks::of_puts(all), Some(FIRST));
    }
}
