//! Typed items the mux puts on track 0 ([TapeMux::put_item](crate::TapeMux::put_item)) and puts again in each piece
//! a cut starts, while they are held: a cut carries the definitions of tracks, not typed items.

use std::collections::BTreeSet;

use exact_units::Bytes;
use shrink_wrap::DeserializeShrinkWrap;
use ww_tape::{
    Block, Definitions, Describe, DescribeError, ItemHash, OwnerId, PieceChain, PieceStore,
    RecordTrack, TrackNum, TypedItem, WriteError,
};

use crate::error::CallError;

/// Items held for later pieces ([ItemCarry::Every], [ItemCarry::Owner]), at most: one more is refused.
pub const HELD_ITEMS_MAX: usize = 1024;

/// Bytes of the items held for later pieces, encoded, at most: an item past it is refused.
pub const HELD_ITEM_BYTES_MAX: usize = 4 << 20;

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
    ///
    /// A track of all-zero owner is the chain's owner's: give that owner for them. `Owner(OwnerId::NONE)` is due
    /// only on a chain whose own owner is [OwnerId::NONE].
    Owner(OwnerId),
}

/// The items held for later pieces, in the order they were put, and which of them are in the piece the chain writes.
pub(crate) struct Held {
    piece: InPiece,
    items: Vec<(ItemHash, Vec<u8>, ItemCarry)>,
}

/// The piece the chain writes, as the held items last saw it.
struct InPiece {
    seq: u64,
    /// Held items in it, put or refused there
    has: BTreeSet<ItemHash>,
    /// Started by a cut the held items are not yet carried after
    fresh: bool,
}

impl InPiece {
    /// Follow the chain into the piece it writes: none of the held items is in a piece just started.
    fn follow<T: PieceStore>(&mut self, chain: &PieceChain<T>) {
        if chain.seq() != self.seq {
            self.seq = chain.seq();
            self.has.clear();
            self.fresh = true;
        }
    }
}

impl Held {
    pub(crate) fn new(seq: u64) -> Self {
        Self {
            piece: InPiece {
                seq,
                has: BTreeSet::new(),
                fresh: false,
            },
            items: Vec::new(),
        }
    }

    /// Whether [Self::carry] asks which tracks a unit wrote: an item of an owner is held and not yet in the piece.
    pub(crate) fn wants_tracks(&self) -> bool {
        self.items
            .iter()
            .any(|(h, _, c)| matches!(c, ItemCarry::Owner(_)) && !self.piece.has.contains(h))
    }

    /// Refuse to hold the item `bytes` encode when holding it as `carry` says would pass [HELD_ITEMS_MAX] or
    /// [HELD_ITEM_BYTES_MAX]; one held already under the same hash takes no more room.
    pub(crate) fn admit(&self, bytes: &[u8], carry: ItemCarry) -> Result<(), CallError> {
        let hash = ItemHash::of(bytes);
        if carry == ItemCarry::Once || self.items.iter().any(|(h, _, _)| *h == hash) {
            return Ok(());
        }
        let held: usize = self.items.iter().map(|(_, b, _)| b.len()).sum();
        let fits = self.items.len() < HELD_ITEMS_MAX
            && held
                .checked_add(bytes.len())
                .is_some_and(|n| n <= HELD_ITEM_BYTES_MAX);
        if fits {
            return Ok(());
        }
        Err(CallError::HeldItems {
            items: self.items.len(),
            bytes: Bytes::new(u64::try_from(held).unwrap_or(u64::MAX)),
        })
    }

    /// Put the item `bytes` encode (a [Describe::Item]), and hold it as `carry` says ([Self::admit] said yes). One
    /// held already under the same hash takes the new carry.
    pub(crate) fn put<T: PieceStore>(
        &mut self,
        chain: &mut PieceChain<T>,
        defs: &Definitions,
        bytes: Vec<u8>,
        carry: ItemCarry,
    ) -> Result<ItemHash, WriteError<T::Error>> {
        let r = put(chain, defs, &bytes);
        // a put may cut before the item: the piece it starts is one [Self::carry] fills
        self.piece.follow(chain);
        r?;
        let hash = ItemHash::of(&bytes);
        self.piece.has.insert(hash);
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

    /// After a unit that wrote on `wrote`: put the held items not yet in the piece the chain writes; the puts
    /// refused. In a piece a cut started, those carried into every piece and those of an owner of a track the piece
    /// head carries; at any unit, those of an owner of a track in `wrote`. A carry never cuts: an item the piece has
    /// no room for is refused there, once, and put in the next piece. A failure ([crate::thread::stops]) ends it.
    pub(crate) fn carry<T: PieceStore>(
        &mut self,
        chain: &mut PieceChain<T>,
        defs: &Definitions,
        wrote: &[TrackNum],
    ) -> Result<u64, WriteError<T::Error>> {
        let piece = &mut self.piece;
        piece.follow(chain);
        let cut = core::mem::take(&mut piece.fresh);
        if self.items.is_empty() || (!cut && wrote.is_empty()) {
            return Ok(0);
        }
        let w = chain.writer()?;
        let mut owners = if cut {
            w.carried_owners(defs)
        } else {
            Vec::new()
        };
        owners.extend(wrote.iter().filter_map(|n| w.track_owner(defs, *n)));
        let mut refused = 0_u64;
        for (hash, bytes, carry) in &self.items {
            let due = match carry {
                ItemCarry::Owner(o) => owners.contains(o),
                ItemCarry::Every | ItemCarry::Once => cut,
            };
            if !due || piece.has.contains(hash) {
                continue;
            }
            match put_here(chain, defs, bytes) {
                Err(e) if crate::thread::stops(chain, &e) => return Err(e),
                Err(_) => refused = refused.saturating_add(1),
                Ok(()) => {}
            }
            piece.has.insert(*hash);
        }
        Ok(refused)
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

/// Put the typed item `bytes` encode, on a new piece when the piece has no room for it.
fn put<T: PieceStore>(
    chain: &mut PieceChain<T>,
    defs: &Definitions,
    bytes: &[u8],
) -> Result<(), WriteError<T::Error>> {
    let item = item_of::<T>(bytes)?;
    chain.put_item(defs, &item).map(|_| ())
}

/// Put the typed item `bytes` encode in the piece the chain writes, never cutting.
fn put_here<T: PieceStore>(
    chain: &mut PieceChain<T>,
    defs: &Definitions,
    bytes: &[u8],
) -> Result<(), WriteError<T::Error>> {
    let item = item_of::<T>(bytes)?;
    chain.writer()?.put_item(defs, &item).map(|_| ())
}

/// The typed item `bytes` encode, as [TapeMux::put_item](crate::TapeMux::put_item) encoded it.
fn item_of<T: PieceStore>(bytes: &[u8]) -> Result<TypedItem<'_>, WriteError<T::Error>> {
    match Describe::from_ww_bytes(bytes).map_err(DescribeError::Decode)? {
        Describe::Item(item) => Ok(item),
        _ => Err(DescribeError::Unknown.into()),
    }
}
