//! Typed items the mux puts on track 0 ([TapeMux::put_item](crate::TapeMux::put_item)) and puts again in each piece
//! a cut starts, while they are held: a cut carries the definitions of tracks, not typed items.

use std::collections::BTreeSet;

use shrink_wrap::DeserializeShrinkWrap;
use ww_tape::{
    Block, Definitions, Describe, DescribeError, ItemHash, OwnerId, PieceChain, PieceStore,
    RecordTrack, TrackNum, WriteError,
};

/// Which pieces a typed item put through the mux is in.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ItemCarry {
    /// The piece it is put in.
    Once,
    /// That piece and each piece a later cut starts, until released ([TapeMux::release_item](crate::TapeMux::release_item)).
    Every,
    /// That piece and each later piece that carries or writes a track of this owner ([ww_tape::TrackDef::owner],
    /// all zeros the chain's own), until released: a run's description, in every piece that holds the run's
    /// tracks. In a piece whose head carries none, it follows the owner's first data there.
    Owner(OwnerId),
}

/// The items held for later pieces, in the order they were put; the piece the chain writes and those of them in it.
pub(crate) struct Held {
    seq: u64,
    in_piece: BTreeSet<ItemHash>,
    items: Vec<(ItemHash, Vec<u8>, ItemCarry)>,
}

impl Held {
    pub(crate) fn new(seq: u64) -> Self {
        Self {
            seq,
            in_piece: BTreeSet::new(),
            items: Vec::new(),
        }
    }

    /// Follow the chain into the piece it writes: none of the held items is in a piece just started.
    fn follow<T: PieceStore>(&mut self, chain: &PieceChain<T>) -> bool {
        if chain.seq() == self.seq {
            return false;
        }
        self.seq = chain.seq();
        self.in_piece.clear();
        true
    }

    /// Whether [Self::carry] asks which tracks a unit wrote: an item of an owner is held and not yet in the piece.
    pub(crate) fn wants_tracks(&self) -> bool {
        self.items
            .iter()
            .any(|(h, _, c)| matches!(c, ItemCarry::Owner(_)) && !self.in_piece.contains(h))
    }

    /// Put the item `bytes` encode (a [Describe::Item]), and hold it as `carry` says. One held already under the
    /// same hash takes the new carry.
    pub(crate) fn put<T: PieceStore>(
        &mut self,
        chain: &mut PieceChain<T>,
        defs: &Definitions,
        bytes: Vec<u8>,
        carry: ItemCarry,
    ) -> Result<ItemHash, WriteError<T::Error>> {
        put(chain, defs, &bytes)?;
        let hash = ItemHash::of(&bytes);
        self.follow(chain);
        self.in_piece.insert(hash);
        self.items.retain(|(h, _, _)| *h != hash);
        if carry != ItemCarry::Once {
            self.items.push((hash, bytes, carry));
        }
        Ok(hash)
    }

    /// Hold item `hash` no more; whether it was held.
    pub(crate) fn release(&mut self, hash: ItemHash) -> bool {
        let before = self.items.len();
        self.items.retain(|(h, _, _)| *h != hash);
        self.items.len() != before
    }

    /// After a unit that wrote on `wrote`: put the held items not yet in the piece the chain writes. After a cut,
    /// those carried into every piece and those of an owner of a track the piece head carries; at any unit, those
    /// of an owner of a track in `wrote`.
    pub(crate) fn carry<T: PieceStore>(
        &mut self,
        chain: &mut PieceChain<T>,
        defs: &Definitions,
        wrote: &[TrackNum],
    ) -> Result<(), WriteError<T::Error>> {
        let cut = self.follow(chain);
        if self.items.is_empty() || (!cut && wrote.is_empty()) {
            return Ok(());
        }
        let w = chain.writer()?;
        let mut owners = if cut {
            w.carried_owners(defs)
        } else {
            Vec::new()
        };
        owners.extend(wrote.iter().filter_map(|n| w.track_owner(defs, *n)));
        for (hash, bytes, carry) in &self.items {
            let due = match carry {
                ItemCarry::Owner(o) => owners.contains(o),
                ItemCarry::Every | ItemCarry::Once => cut,
            };
            if due && !self.in_piece.contains(hash) {
                put(chain, defs, bytes)?;
                self.in_piece.insert(*hash);
            }
        }
        // a put that cut again starts another piece: its items go in on the next call
        Ok(())
    }
}

/// The data tracks the block `block` writes on: a chunk's track, or those of its records; none of a block it
/// cannot read, which the chain refused.
pub(crate) fn block_tracks(block: &[u8]) -> Vec<TrackNum> {
    let Ok(b) = Block::read(block) else {
        return Vec::new();
    };
    if let Some(c) = b.chunk() {
        return vec![c.head().track_number];
    }
    let mut buf = Vec::new();
    let Ok(b) = b.decode(&mut buf) else {
        return Vec::new();
    };
    let tracks: BTreeSet<TrackNum> = b
        .records()
        .filter_map(|r| match r.ok()?.track {
            RecordTrack::Data(n) => Some(n),
            RecordTrack::Definitions => None,
        })
        .collect();
    tracks.into_iter().collect()
}

/// Put the typed item `bytes` encode.
fn put<T: PieceStore>(
    chain: &mut PieceChain<T>,
    defs: &Definitions,
    bytes: &[u8],
) -> Result<(), WriteError<T::Error>> {
    match Describe::from_ww_bytes(bytes).map_err(DescribeError::Decode)? {
        Describe::Item(item) => chain.put_item(defs, &item).map(|_| ()),
        _ => Err(DescribeError::Unknown.into()),
    }
}
