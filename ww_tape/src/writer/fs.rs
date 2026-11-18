//! Writers of files named by a path.

use std::io;
use std::path::Path;

use super::{Finished, Resumed, SyncPolicy, TapeWriter, WriteError, WriterOptions};
use crate::sink::{FileSink, Files, TapeSink, io_err};

impl TapeWriter<FileSink> {
    /// A new file at `path`, written as `path.partial` until [Self::finish]; fails if `path` is there or a
    /// `path.partial` holds bytes ([FileSink::create]).
    pub fn create(path: &Path, policy: SyncPolicy) -> Result<Self, WriteError> {
        Ok(Self::create_on(FileSink::create(path)?, policy))
    }

    /// Append to the file at `path` in place, created if it isn't there. The blocks of another producer session start
    /// with its definitions, which a reader takes as the next part of the file.
    pub fn append(path: &Path, policy: SyncPolicy) -> Result<Self, WriteError> {
        let sink = FileSink::append(path)?;
        let start = sink.size().map_err(io_err("reading the file's length"))?;
        let mut w = Self::create_on(sink, policy);
        w.start = start;
        Ok(w)
    }

    /// Reopen the unfinished piece `path` (written as `path.partial` by [Self::create] and cut off by a crash) to go
    /// on writing it, see [Self::resume_on].
    pub fn resume(path: &Path, options: WriterOptions) -> Result<(Self, Resumed), WriteError> {
        match FileSink::reopen(path)? {
            Some(sink) => Self::resume_on(sink, options),
            None if path.exists() => Err(WriteError::Exists(path.to_path_buf())),
            None => Err(WriteError::Io {
                what: "opening the unfinished file",
                error: io::ErrorKind::NotFound.into(),
            }),
        }
    }

    /// End this piece as [FinishState::Continued](crate::FinishState::Continued) and start the next piece of the chain in a new file at
    /// `next_path`, see [Self::cut_on].
    pub fn cut(self, next_path: &Path) -> Result<(Finished, Self), WriteError> {
        self.cut_with(&mut Files, next_path)
    }
}
