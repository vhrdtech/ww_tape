//! [check_chain]: how the pieces at hand of one recording link up.

use alloc::vec::Vec;

use super::{CloseReason, ContentHash, PieceId, PieceIdent};
use crate::index::IndexError;

/// What a reader knows of one piece: its head and, when it was closed, its end.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
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
}

/// How a piece's head read, for [PieceSummary::head].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum HeadStatus {
    /// Read whole: ident and previous piece from it.
    Read,
    /// Lost (damaged): ident from the close, previous piece unknown (None).
    Lost,
    /// A head of an integrity level this build refuses (the error, [IndexError::UnhandledIntegrity] or
    /// [IndexError::UnknownIntegrity]): ident and previous piece from it, the piece's contents unread.
    Refused(IndexError),
}

/// How two pieces next to each other by sequence number relate.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ChainLink {
    /// The later head names the earlier piece by hash and id, and the earlier piece's last close, a cut, names the
    /// later id.
    Linked,
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
    /// The first piece at hand has a previous piece: the chain's start was deleted (retention), not damaged.
    pub head_cut: bool,
    /// `links[i]` is between `order[i]` and `order[i + 1]`.
    pub links: Vec<ChainLink>,
    pub end: ChainEnd,
    /// Two pieces with one sequence number or of another owner than the first: left out of `order`.
    pub strays: Vec<usize>,
    /// Pieces whose head is of an integrity level this build refuses ([HeadStatus::Refused]): linked as their heads
    /// say, their contents unread.
    pub refused: Vec<usize>,
}

impl ChainReport {
    /// Every piece there, linked, from the first to a close record of the last.
    pub fn is_whole(&self) -> bool {
        !self.head_cut
            && self.strays.is_empty()
            && self.refused.is_empty()
            && matches!(self.end, ChainEnd::Closed { .. })
            && self.links.iter().all(|l| *l == ChainLink::Linked)
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
    let links = order
        .iter()
        .zip(order.iter().skip(1))
        .map(|((_, a), (_, b))| link(a, b))
        .collect();
    // a first piece whose head is lost was the chain's first only at sequence number 0
    let head_cut = order
        .first()
        .is_some_and(|(_, f)| f.prev.is_some() || (f.head == HeadStatus::Lost && f.ident.seq > 0));
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
    ChainReport {
        order: order.into_iter().map(|(i, _)| i).collect(),
        head_cut,
        links,
        end,
        strays,
        refused,
    }
}

fn link(a: &PieceSummary, b: &PieceSummary) -> ChainLink {
    // `order` holds distinct seqs, ascending: b's is past a's
    let missing = b.ident.seq.abs_diff(a.ident.seq).saturating_sub(1);
    if missing > 0 {
        return ChainLink::Gap { missing };
    }
    let Some((_, hash, next)) = a.end.filter(|_| !a.unclosed) else {
        return ChainLink::Unclosed;
    };
    // only a close by a cut names a piece after it
    match (next == Some(b.ident.id), b.head) {
        (true, HeadStatus::Lost) => ChainLink::HeadLost,
        (true, _) if b.prev == Some((hash, a.ident.id)) => ChainLink::Linked,
        _ => ChainLink::Broken,
    }
}
