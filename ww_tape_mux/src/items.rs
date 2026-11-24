//! Typed items the mux puts on track 0 ([TapeMux::put_item](crate::TapeMux::put_item)) and puts again in each piece
//! a cut starts, while they are held: a cut carries the definitions of tracks, not typed items.

use shrink_wrap::DeserializeShrinkWrap;
use ww_tape::{
    Definitions, Describe, DescribeError, ItemHash, OwnerId, PieceChain, PieceStore, WriteError,
};

/// Which pieces a typed item put through the mux is in.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ItemCarry {
    /// The piece it is put in.
    Once,
    /// That piece and each piece a later cut starts, until released ([TapeMux::release_item](crate::TapeMux::release_item)).
    Every,
    /// That piece and each piece a later cut starts that carries a track of this owner ([ww_tape::TrackDef::owner],
    /// all zeros the chain's own), until released: a run's description, in every piece that carries the run's
    /// tracks.
    Owner(OwnerId),
}

/// The items held for later pieces, in the order they were put, and the piece the mux last put them in.
pub(crate) struct Held {
    seq: u64,
    items: Vec<(ItemHash, Vec<u8>, ItemCarry)>,
}

impl Held {
    pub(crate) fn new(seq: u64) -> Self {
        Self {
            seq,
            items: Vec::new(),
        }
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

    /// After a cut: put the held items into the piece it started, those of an owner when the piece carries a track
    /// of it. Nothing while the chain writes the piece they were put in last.
    pub(crate) fn carry<T: PieceStore>(
        &mut self,
        chain: &mut PieceChain<T>,
        defs: &Definitions,
    ) -> Result<(), WriteError<T::Error>> {
        if chain.seq() == self.seq {
            return Ok(());
        }
        self.seq = chain.seq();
        if self.items.is_empty() {
            return Ok(());
        }
        let owners = chain.writer()?.carried_owners(defs);
        for (_, bytes, carry) in &self.items {
            let due = match carry {
                ItemCarry::Owner(o) => owners.contains(o),
                ItemCarry::Every | ItemCarry::Once => true,
            };
            if due {
                put(chain, defs, bytes)?;
            }
        }
        // a put that cut again starts another piece: its items go in on the next call
        Ok(())
    }
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
