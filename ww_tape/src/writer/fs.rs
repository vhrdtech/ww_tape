//! Writers of files named by a path.

use std::path::Path;

use super::{Finished, ResumeError, Resumed, SyncPolicy, TapeWriter, WriteError, WriterOptions};
use crate::sink::{FileSink, Files, FsError, sink_err};

impl TapeWriter<FileSink> {
    /// A new file at `path`, written as `path.partial` until [Self::finish]; fails if `path` is there or a
    /// `path.partial` holds bytes ([FileSink::create]).
    pub fn create(path: &Path, policy: SyncPolicy) -> Result<Self, WriteError<FsError>> {
        let sink = FileSink::create(path).map_err(sink_err("creating the file"))?;
        Ok(Self::create_on(sink, policy))
    }

    /// Append to the file at `path` in place, created if it isn't there. The blocks of another producer session start
    /// with its definitions, which a reader takes as the next part of the file.
    pub fn append(path: &Path, policy: SyncPolicy) -> Result<Self, WriteError<FsError>> {
        let sink = FileSink::append(path).map_err(sink_err("opening the file"))?;
        Self::append_on(sink, policy)
    }

    /// Reopen the unfinished piece `path` (written as `path.partial` by [Self::create] and cut off by a crash) to go
    /// on writing it, see [Self::resume_on].
    pub fn resume(
        path: &Path,
        options: WriterOptions,
    ) -> Result<(Self, Resumed), WriteError<FsError>> {
        let what = "opening the unfinished file";
        match FileSink::reopen(path).map_err(sink_err(what))? {
            Some(sink) => Self::resume_on(sink, options),
            None if path.exists() => Err(WriteError::Sink {
                what,
                error: FsError::Exists(path.to_path_buf()),
            }),
            None => Err(WriteError::Resume(ResumeError::NoPiece)),
        }
    }

    /// The name of the file while it is written (`NAME.partial` for a new file).
    pub fn current_path(&self) -> &Path {
        self.sink.current_path()
    }

    /// End this piece as [FinishState::Continued](crate::FinishState::Continued) and start the next piece of the chain in a new file at
    /// `next_path`, see [Self::cut_on].
    pub fn cut(self, next_path: &Path) -> Result<(Finished, Self), WriteError<FsError>> {
        self.cut_with(&mut Files::file(next_path), &0)
    }
}
