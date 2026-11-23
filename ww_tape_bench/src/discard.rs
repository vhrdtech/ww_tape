//! A store whose tapes keep no bytes ([Discard]): a bench measures the encoding and the writer's work, not a disk.

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use ww_tape::reader::ByteSource;
use ww_tape::{PieceStore, TapeSink};

/// A store whose tapes keep no bytes, counting what is written into all of them.
#[derive(Clone, Default)]
pub struct Discard {
    pub written: Arc<AtomicU64>,
}

/// A tape of [Discard].
pub struct DiscardSink {
    written: Arc<AtomicU64>,
    bytes: u64,
}

/// A closed tape of [Discard]: there is none, so none is read.
pub struct NoSource;

fn kept_none() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "a discarding store keeps no bytes",
    )
}

impl ByteSource for NoSource {
    type Error = io::Error;

    fn size(&self) -> io::Result<u64> {
        Err(kept_none())
    }

    fn read_at(&self, _: u64, _: &mut [u8]) -> io::Result<()> {
        Err(kept_none())
    }
}

impl TapeSink for DiscardSink {
    type Error = io::Error;

    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.bytes += bytes.len() as u64;
        self.written
            .fetch_add(bytes.len() as u64, Ordering::Relaxed);
        Ok(())
    }

    fn sync(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn close(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn size(&self) -> io::Result<u64> {
        Ok(self.bytes)
    }

    fn read_at(&self, _: u64, _: &mut [u8]) -> io::Result<()> {
        Err(kept_none())
    }

    fn truncate(&mut self, _: u64) -> io::Result<()> {
        Err(kept_none())
    }

    fn rebase(&mut self, base: u64) -> io::Result<()> {
        if base == 0 { Ok(()) } else { Err(kept_none()) }
    }
}

impl PieceStore for Discard {
    type Key = u64;
    type Error = io::Error;
    type Sink = DiscardSink;
    type Source = NoSource;

    fn key_of(&self, seq: u64) -> u64 {
        seq
    }

    fn create(&mut self, _: &u64) -> io::Result<DiscardSink> {
        Ok(DiscardSink {
            written: self.written.clone(),
            bytes: 0,
        })
    }

    fn reopen(&mut self, _: &u64) -> io::Result<Option<DiscardSink>> {
        Ok(None)
    }

    fn closed(&self, _: &u64) -> io::Result<Option<NoSource>> {
        Ok(None)
    }

    fn create_whole(&mut self, key: &u64) -> io::Result<DiscardSink> {
        self.create(key)
    }

    fn open_whole(&mut self, _: &u64) -> io::Result<Option<DiscardSink>> {
        Ok(None)
    }
}
