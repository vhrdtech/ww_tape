//! Values of a bytes track written from a producer's thread: each part goes through the queue as a chunk.

use crate::handle::{ChunkSender, Sent};
use crate::{MuxError, TapeMux};
use ww_tape::bytes_track::{BytesError, BytesOptions, BytesPart, BytesWriter, ValueId, ValueSeal};
use ww_tape::{ItemNum, PieceStore, TrackNum};

/// Writes the values of one bytes track ([ww_tape::bytes_track]): [Self::begin] opens a value, [Self::write] takes
/// its bytes, [Self::finish] seals it.
///
/// Each part goes through the mux's byte-bounded queue as a chunk, so a large value interleaves with other handles'
/// units and the handle holds one part's bytes at a time. It waits for room in the queue, so no part is dropped. A
/// call that fails leaves the open value torn and gives it up: it reads as absent.
pub struct BytesHandle<T: PieceStore> {
    chunks: ChunkSender<T>,
    writer: BytesWriter,
}

impl<T> TapeMux<T>
where
    T: PieceStore + Send + 'static,
    T::Sink: Send,
    T::Key: Send,
    T::Error: Send + Sync,
{
    /// A handle that writes the values of bytes track `track`, its next part numbered `next_part`: 0 on a new track,
    /// else the item number after the track's last part (a resumed chain's).
    pub fn bytes(
        &self,
        track: TrackNum,
        next_part: ItemNum,
        options: BytesOptions,
    ) -> Result<BytesHandle<T>, MuxError<T::Error>> {
        let defs = self.definitions()?;
        let writer = BytesWriter::new(&defs, track, next_part, options).map_err(MuxError::Bytes)?;
        let chunks = self.chunks(track)?;
        Ok(BytesHandle { chunks, writer })
    }
}

impl<T: PieceStore> BytesHandle<T> {
    /// The track the handle writes.
    pub fn track(&self) -> TrackNum {
        self.writer.track()
    }

    /// The number the next part gets.
    pub fn next_part(&self) -> ItemNum {
        self.writer.next_part()
    }

    /// Open a value at `ticks` of the track's clock; its id.
    pub fn begin(&mut self, ticks: i64) -> Result<ValueId, MuxError<T::Error>> {
        self.writer.begin(ticks).map_err(MuxError::Bytes)
    }

    /// Take `bytes` of the open value, each part that fills sent.
    pub fn write(&mut self, bytes: &[u8]) -> Result<(), MuxError<T::Error>> {
        let chunks = &self.chunks;
        let r = self.writer.write(bytes, |p| send(chunks, &p));
        self.give_up_on_err(r)
    }

    /// Seal the open value: its last part and its seal sent. Its id, length, hash and parts.
    pub fn finish(&mut self) -> Result<ValueSeal, MuxError<T::Error>> {
        let chunks = &self.chunks;
        let r = self.writer.finish(|p| send(chunks, &p));
        self.give_up_on_err(r)
    }

    /// Give up the open value: it reads as absent.
    pub fn abandon(&mut self) {
        self.writer.abandon();
    }

    /// Return once every part this handle sent is placed; a refusal of one of them.
    pub fn flush(&self) -> Result<(), MuxError<T::Error>> {
        self.chunks.flush()
    }

    /// Return once every part this handle sent is durable; a refusal of one of them.
    pub fn sync(&self) -> Result<(), MuxError<T::Error>> {
        self.chunks.sync()
    }

    fn give_up_on_err<V>(
        &mut self,
        r: Result<V, MuxError<T::Error>>,
    ) -> Result<V, MuxError<T::Error>> {
        if r.is_err() {
            self.writer.abandon();
        }
        r
    }
}

impl<E> From<BytesError> for MuxError<E> {
    fn from(e: BytesError) -> Self {
        Self::Bytes(e)
    }
}

/// Send part `p` through `chunks`, its columns copied into the mux's buffers.
fn send<T: PieceStore>(
    chunks: &ChunkSender<T>,
    p: &BytesPart<'_>,
) -> Result<(), MuxError<T::Error>> {
    let columns = p
        .columns()
        .iter()
        .map(|(spec, bytes)| {
            let mut b = chunks.buffer();
            b.extend_from_slice(bytes);
            (*spec, b)
        })
        .collect();
    let sent = chunks.send(p.head, columns)?;
    debug_assert_eq!(
        sent,
        Sent::Queued,
        "a bytes handle's sender waits for room: nothing is dropped"
    );
    Ok(())
}
