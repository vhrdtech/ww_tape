# Sinks, files and locks

[Writing a tape](writing.md#the-writer-and-its-sink) introduces sinks and stores. This page covers the sinks that the
crates provide: files with their locks, memory, and sinks without the standard library.

## Writing to files

Everything that names a path lives in `ww_tape_fs`. `ww_tape` itself therefore builds without a file system.

The module `ww_tape_fs::writer` opens a writer on a path:

- `writer::create(path, sync)` creates the file as `NAME.partial` and renames it to `NAME` at the close.
- `writer::append(path, sync)` appends to a file in a new session. It creates the file when it does not exist.
- `writer::resume(path, options)` reopens the unclosed piece of `path` ([When the power
  fails](crashes.md#resuming-the-file)). With neither `NAME` nor `NAME.partial` there, it fails with
  `FsError::NoPiece`.
- `writer::cut(writer, defs, next_path)` ends the piece and starts the next one in the new file `next_path`
  ([Cuts](syncing_and_cuts.md#cuts-ending-one-piece-and-starting-the-next)).

`writer::create` refuses three cases:

- a `NAME` that exists (`FsError::Exists`);
- a `NAME.partial` that a crash left with bytes in it (`FsError::Exists`);
- a `NAME.partial` that is renamed or replaced between its open and its lock (`FsError::Replaced`).

`create` never empties a file. Reopen such a piece with `writer::resume`, or remove it.

The module `ww_tape_fs::chain` opens a chain of pieces in a directory, or in one file, with `chain::create` and
`chain::resume`.

Each of these functions only makes the sink or the store and hands it to a constructor of `ww_tape`:

- `TapeWriter::create_on`, `append_on`, `resume_on` and `cut_with`;
- `PieceChain::create_on` and `resume_on`.

A sink of any other kind uses the same constructors.

## One writer per file

Two writers appending to one file would interleave their blocks into a file that reads as damaged. A file sink therefore
holds a lock on `NAME.lock`, a file beside `NAME`.

A second sink of the same file gets the error `FsError::Locked` from the lock, whether its writer runs in this process
or another.

A name of 251 bytes or more leaves no room for the `.lock` suffix. Its lock file is then `HASH.lock`, where `HASH` is
the hash of the name in hex, made with BLAKE3 (a cryptographic hash function).

### Who holds the lock

Only writers open the lock file. Readers open and close `NAME` as they like, and never release the lock by doing so.

On unix, the lock is a Portable Operating System Interface (POSIX) record lock, which a child process forked meanwhile
does not hold. A resume right after a writer is dropped is therefore never refused because of a child.

On unix, the writer removes the lock file when it lets go. On Windows the lock file stays, since removing it could race
the next holder.

A lock file left behind, by Windows or by a crash, holds no lock. The next writer takes it as it is.

### Read-only file systems

A file on a read-only mount or medium takes no writer. Each step that meets the refusal, creating or opening the file
or its lock file, fails with `FsError::ReadOnly` and the path.

Read such a file with `ww_tape_fs::FileSource`, which opens it for reading only and takes no lock.

### One lock per file

One file has one lock, whatever name reaches it. The lock file is named after the file's canonical name, with links
resolved and `.partial` dropped.

On unix, writers of one process are also kept apart by the file's device and inode. A second writer through a hard link
is therefore refused as well.

Two processes that write one file through two hard-linked names are not refused, and their blocks interleave.

A device, such as a raw SD card, is locked itself. A FIFO special file, a socket
or a directory is refused (`FsError::NotAFile`).

### Errors name the step and the file

An error states the step, the file and the kind of failure in its first line. For example:
`creating a writer: /data/x.wwt: locked by another writer`.

A log line of that text alone therefore says what went wrong.

The operating system may report the error, such as a full disk or a missing directory. The system's error is then the
source of that first error and is not repeated in its text.

## Writing to memory

A `MemSink` of a chain reserves the room of a whole piece when the piece starts: the chain's cut size
(`CutPolicy::bytes`) plus `CUT_SLACK` (1 MiB). A reader's `read_at` therefore never waits while the bytes are copied
into a bigger buffer.

`MemTapes::remove(key)` takes a closed piece out of the store, for example to keep a bounded window of a live chain. A
reader that already holds the piece's `MemTape` reads on unchanged.

The piece still being written is not removed (`MemError::Unclosed`). What remains of the chain reads as a head cut.

Each remaining piece still reads alone, since it holds the definitions and state it uses. A chain with removed pieces
cannot be resumed, since a resume starts from the first piece.

## Without the standard library

The writer, the chain and resume need only an allocator. Firmware or a browser can therefore write tapes on a sink of
its own.

Without the standard library, a new writer starts without a session and without a clock:

- A new writer has no session, because it has no random source. It refuses to write until it gets one.
- `TapeWriter::set_session` gives it a session, for example `SessionTag::from_seed` over a device id or a boot counter.
  A piece head gives it one too (`begin_piece`, and every piece of a chain).
- A new writer has no clock. No sync, checkpoint or cut falls due by time until `WriterOptions::clock` gives it one.
