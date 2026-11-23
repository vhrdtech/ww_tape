//! [check_chain]: how the pieces at hand of one recording link up.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;

use super::{CloseReason, ContentHash, Integrity, PieceId, PieceIdent, RemovedPiece};
use crate::index::IndexError;

/// What a reader knows of one piece: its head and, when it was closed, its end.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PieceSummary {
    pub ident: PieceIdent,
    /// The previous piece's content hash and id, from the head; None for the first piece.
    pub prev: Option<(ContentHash, PieceId)>,
    /// Reason, content hash and next id from the last close record or tail; None for a piece never closed.
    pub end: Option<(CloseReason, ContentHash, Option<PieceId>)>,
    /// The piece has bytes after its last close record and tail, or its last close has no whole tail: written on
    /// after that close, the writer still writing or stopped by a crash.
    pub unclosed: bool,
    /// How the head read.
    pub head: HeadStatus,
    /// The pieces of the chain its close in force names as removed on purpose ([RemovedPiece]), ascending by sequence
    /// number.
    pub removed: Vec<RemovedPiece>,
    /// The piece's level, from its head; None when the head didn't read. Below [Integrity::Full] its content hash is
    /// absent: a link from it is by id and sequence number only ([ChainLink::Unproven]).
    pub integrity: Option<Integrity>,
}

/// How a piece's head read, for [PieceSummary::head].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum HeadStatus {
    /// Read whole: ident and previous piece from it.
    Read,
    /// Lost (damaged): ident from the close, previous piece unknown (None).
    Lost,
    /// A head of an integrity level this build doesn't know (the error, [IndexError::UnknownIntegrity]): ident and
    /// previous piece from it, the piece's contents unread.
    Refused(IndexError),
}

/// How two pieces next to each other by sequence number relate.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ChainLink {
    /// The later head names the earlier piece by hash and id, and the earlier piece's last close, a cut, names the
    /// later id.
    Linked,
    /// As [Self::Linked] by id, but the earlier piece is below [Integrity::Full]: it has no content hash, so the link
    /// is not proven by hash.
    Unproven,
    /// Pieces between them are missing.
    Gap { missing: u64 },
    /// Adjacent sequence numbers, but the hashes or ids don't match, or the earlier piece's last close is no cut: not
    /// the same chain, or a piece was rewritten.
    Broken,
    /// The earlier piece doesn't end at a close record: it was cut off, or written on after its close, and the chain
    /// went on regardless.
    Unclosed,
    /// The earlier piece's end names the later one's id, but the later piece's head is lost ([HeadStatus::Lost]):
    /// linked one way only.
    HeadLost,
    /// The `pieces` between them were removed on purpose: a close at hand names each of them ([RemovedPiece]), the
    /// earlier piece's last close, a cut, names the first one's id, and the later head names the last one's hash and
    /// id.
    Removed { pieces: u64 },
}

/// How the chain ends.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ChainEnd {
    /// The last piece ends at a close record, for the reason it says.
    Closed { reason: CloseReason },
    /// The last piece says the recording goes on in a piece that isn't here.
    TailCut,
    /// The last piece has bytes after its last close record, or none: being written, or cut off by a crash.
    Unclosed,
}

/// [check_chain]'s answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChainReport {
    /// Indices into the pieces given, by sequence number.
    pub order: Vec<usize>,
    /// The first piece at hand has a previous piece: the chain's start was deleted (retention), not damaged. False when
    /// the pieces before it were removed on purpose ([Self::head_removed]).
    pub head_cut: bool,
    /// Pieces before the first at hand removed on purpose: a close at hand names each of them from sequence number 0
    /// on ([RemovedPiece]), and the first head at hand names the last one's hash and id. 0 for none.
    pub head_removed: u64,
    /// `links[i]` is between `order[i]` and `order[i + 1]`.
    pub links: Vec<ChainLink>,
    pub end: ChainEnd,
    /// Two pieces with one sequence number or of another owner than the first: left out of `order`.
    pub strays: Vec<usize>,
    /// Pieces whose head is of an integrity level this build doesn't know ([HeadStatus::Refused]): linked as their
    /// heads say, their contents unread.
    pub refused: Vec<usize>,
    /// The pieces the closes at hand name as removed, ascending by sequence number.
    ///
    /// A piece named by two closes with another id or hash is left out, and links over it read as a gap. A piece still
    /// at hand is left out too: a row naming it plays no part in a link. A writer naming more removed pieces in a
    /// later piece gives these again ([TapeWriter::record_removed](crate::TapeWriter::record_removed)), so they
    /// outlive the piece naming them.
    pub removed: Vec<RemovedPiece>,
}

impl ChainReport {
    /// [Self::is_whole], each link proven by hash: no piece but the last below [Integrity::Full].
    pub fn is_proven(&self) -> bool {
        self.is_whole() && !self.links.contains(&ChainLink::Unproven)
    }

    /// Every piece there or removed on purpose, linked, from the first to a close record of the last; a link by id
    /// alone ([ChainLink::Unproven]) counts.
    pub fn is_whole(&self) -> bool {
        !self.head_cut
            && self.strays.is_empty()
            && self.refused.is_empty()
            && matches!(self.end, ChainEnd::Closed { .. })
            && self.links.iter().all(|l| {
                matches!(
                    l,
                    ChainLink::Linked | ChainLink::Unproven | ChainLink::Removed { .. }
                )
            })
    }
}

/// Order `pieces` of one recording by sequence number and check each link both ways.
pub fn check_chain(pieces: &[PieceSummary]) -> ChainReport {
    let mut idx: Vec<(usize, &PieceSummary)> = pieces.iter().enumerate().collect();
    idx.sort_by_key(|(i, p)| (p.ident.seq, *i));
    let mut order: Vec<(usize, &PieceSummary)> = Vec::with_capacity(idx.len());
    let mut strays = Vec::new();
    for (i, p) in idx {
        let same_seq = order
            .last()
            .is_some_and(|(_, l)| l.ident.seq == p.ident.seq);
        let other_owner = order
            .first()
            .is_some_and(|(_, f)| f.ident.owner != p.ident.owner);
        if same_seq || other_owner {
            strays.push(i);
        } else {
            order.push((i, p));
        }
    }
    let removed = Removed::of(order.iter().map(|(_, p)| *p));
    let links = order
        .iter()
        .zip(order.iter().skip(1))
        .map(|((_, a), (_, b))| link(a, b, &removed))
        .collect();
    let head_removed = order
        .first()
        .and_then(|(_, f)| {
            let last = removed.covering(0, f.ident.seq)?;
            (f.prev == Some((last.hash, last.id))).then_some(f.ident.seq)
        })
        .unwrap_or(0);
    // a first piece whose head is lost was the chain's first only at sequence number 0
    let head_cut = head_removed == 0
        && order.first().is_some_and(|(_, f)| {
            f.prev.is_some() || (f.head == HeadStatus::Lost && f.ident.seq > 0)
        });
    let end = match order
        .last()
        .map(|(_, l)| l)
        .and_then(|l| l.end.filter(|_| !l.unclosed))
    {
        None => ChainEnd::Unclosed,
        Some((_, _, Some(_))) => ChainEnd::TailCut,
        Some((reason, _, None)) => ChainEnd::Closed { reason },
    };
    let refused = pieces
        .iter()
        .enumerate()
        .filter(|(_, p)| matches!(p.head, HeadStatus::Refused(_)))
        .map(|(i, _)| i)
        .collect();
    let at_hand: BTreeSet<u64> = order.iter().map(|(_, p)| p.ident.seq).collect();
    let removed = removed
        .agreed()
        .filter(|r| !at_hand.contains(&r.seq))
        .collect();
    ChainReport {
        order: order.into_iter().map(|(i, _)| i).collect(),
        head_cut,
        head_removed,
        links,
        end,
        strays,
        refused,
        removed,
    }
}

/// The removed pieces the closes at hand name, by sequence number; None for one two closes name differently.
pub(crate) struct Removed(BTreeMap<u64, Option<RemovedPiece>>);

impl Removed {
    fn of<'p>(pieces: impl Iterator<Item = &'p PieceSummary>) -> Self {
        Self::of_rows(pieces.flat_map(|p| &p.removed))
    }

    /// The rows `rows` of the closes at hand.
    pub(crate) fn of_rows<'r>(rows: impl Iterator<Item = &'r RemovedPiece>) -> Self {
        let mut m: BTreeMap<u64, Option<RemovedPiece>> = BTreeMap::new();
        for r in rows {
            m.entry(r.seq)
                .and_modify(|e| {
                    if *e != Some(*r) {
                        *e = None;
                    }
                })
                .or_insert(Some(*r));
        }
        Self(m)
    }

    /// The first and the last of the pieces of sequence numbers `from..to` when every one of them is named, agreed.
    pub(crate) fn span(&self, from: u64, to: u64) -> Option<(RemovedPiece, RemovedPiece)> {
        let mut expect = from;
        let mut ends: Option<(RemovedPiece, RemovedPiece)> = None;
        // keys are distinct and ascending: each must be the next seq
        for (seq, r) in self.0.range(from..to) {
            let r = (*seq == expect).then_some(*r).flatten()?;
            ends = Some((ends.map_or(r, |(first, _)| first), r));
            expect = expect.checked_add(1)?;
        }
        ends.filter(|_| expect == to)
    }

    /// The last of the pieces of sequence numbers `from..to` when every one of them is named, agreed.
    fn covering(&self, from: u64, to: u64) -> Option<RemovedPiece> {
        self.span(from, to).map(|(_, last)| last)
    }

    fn agreed(&self) -> impl Iterator<Item = RemovedPiece> + '_ {
        self.0.values().filter_map(|r| *r)
    }
}

fn link(a: &PieceSummary, b: &PieceSummary, removed: &Removed) -> ChainLink {
    // `order` holds distinct seqs, ascending: b's is past a's
    let missing = b.ident.seq.abs_diff(a.ident.seq).saturating_sub(1);
    // the pieces removed between them, their first and last; pieces missing otherwise are a gap
    let span = match missing {
        0 => None,
        _ => match removed.span(a.ident.seq.saturating_add(1), b.ident.seq) {
            Some(span) => Some(span),
            None => return ChainLink::Gap { missing },
        },
    };
    let Some((_, hash, next)) = a.end.filter(|_| !a.unclosed) else {
        return ChainLink::Unclosed;
    };
    // what a's cut names, and what b's head names: each other, or the ends of the pieces removed between them
    let (named, before, linked) = match span {
        None => (b.ident.id, (hash, a.ident.id), ChainLink::Linked),
        Some((first, last)) => (
            first.id,
            (last.hash, last.id),
            ChainLink::Removed { pieces: missing },
        ),
    };
    // below Integrity::Full a's hash is absent: b's head is checked by the id it names, never by a zero hash
    let unhashed = span.is_none() && a.integrity.is_some_and(|l| l != Integrity::Full);
    // only a close by a cut names a piece after it
    match (next == Some(named), b.head) {
        (true, HeadStatus::Lost) => ChainLink::HeadLost,
        (true, _) if unhashed && b.prev.map(|(_, id)| id) == Some(before.1) => ChainLink::Unproven,
        (true, _) if !unhashed && b.prev == Some(before) => linked,
        _ => ChainLink::Broken,
    }
}
