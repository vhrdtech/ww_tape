# Sinks, files and locks

The material of this page is now in the chapters of "Writing". Each section below names where it went.

## Writing to files

[Files and syncing](files.md#opening-a-file) lists the calls that open a writer on a path.

## One writer per file

[Files and syncing](files.md#one-writer-per-file) explains the lock.

### Who holds the lock

[Files and syncing](files.md#one-writer-per-file) says who holds the lock, and [Locks](format.md#locks) gives the
rules per system.

### Read-only file systems

[Files and syncing](files.md#one-writer-per-file) covers files on a read-only mount.

### One lock per file

[Locks](format.md#locks) covers links, devices and network file systems.

### Errors name the step and the file

[Files and syncing](files.md#opening-a-file) shows what an error states.

## Writing to memory

[Live feeds and relays](live_feeds.md#reading-while-the-writer-writes) covers a sink in memory, and [Pieces and
chains](pieces.md#deleting-old-pieces) removing pieces from a store in memory.

## Without the standard library

[Live feeds and relays](live_feeds.md#without-the-standard-library) explains a writer without the standard library.
