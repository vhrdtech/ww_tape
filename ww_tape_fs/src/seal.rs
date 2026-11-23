//! The sink a seal writes a file through in place ([crate::writer::seal]).

use ww_tape::sink::TapeSink;

use crate::sink::{FileSink, FsError};

/// `n` as a `u64`.
pub(crate) fn wide(n: usize) -> u64 {
    const { assert!(usize::BITS <= u64::BITS, "a usize fits a u64") };
    u64::try_from(n).unwrap_or(u64::MAX)
}

/// A [FileSink] whose writes go over the piece's own bytes from its start, as a seal writes them
/// ([ww_tape::TapeWriter::seal_on]): every block in its place, the piece head held back until [Self::finish].
pub(crate) struct InPlace {
    sink: FileSink,
    /// Where the next write goes, from the start of the piece
    at: u64,
    /// The piece's first write, its head: written last
    head: Option<Vec<u8>>,
}

impl InPlace {
    pub(crate) fn new(sink: FileSink) -> Self {
        Self {
            sink,
            at: 0,
            head: None,
        }
    }

    /// Sync the piece written, then write its head and sync again: the head, which names the piece's level, changes
    /// only once every other byte of the piece is on the disk.
    pub(crate) fn finish(&mut self) -> Result<(), FsError> {
        self.sink.sync()?;
        if let Some(head) = self.head.take() {
            self.sink.write_at(0, &head)?;
            self.sink.sync()?;
        }
        Ok(())
    }
}

impl TapeSink for InPlace {
    type Error = FsError;

    fn write_all(&mut self, bytes: &[u8]) -> Result<(), FsError> {
        if self.at == 0 && self.head.is_none() {
            self.head = Some(bytes.to_vec());
        } else {
            self.sink.write_at(self.at, bytes)?;
        }
        self.at = self.at.saturating_add(wide(bytes.len()));
        Ok(())
    }

    fn sync(&mut self) -> Result<(), FsError> {
        self.sink.sync()
    }

    /// Nothing: [Self::finish] ends a piece.
    fn close(&mut self) -> Result<(), FsError> {
        Ok(())
    }

    fn size(&self) -> Result<u64, FsError> {
        self.sink.size()
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), FsError> {
        self.sink.read_at(offset, buf)
    }

    /// Refused: a seal keeps every byte of the file.
    fn truncate(&mut self, _len: u64) -> Result<(), FsError> {
        Err(FsError::Io {
            what: "truncating a file being sealed",
            path: self.sink.path().to_path_buf(),
            error: std::io::ErrorKind::Unsupported.into(),
        })
    }

    fn base(&self) -> u64 {
        self.sink.base()
    }

    fn holds_pieces(&self) -> bool {
        self.sink.holds_pieces()
    }

    /// The next piece starts at `base`, the piece before [finished](Self::finish): written from its start, its head
    /// held back again.
    fn rebase(&mut self, base: u64) -> Result<(), FsError> {
        self.finish()?;
        self.sink.rebase(base)?;
        self.at = 0;
        Ok(())
    }
}
