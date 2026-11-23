---
level: basic
---

# A first tape

A recorder writes a tape through a few calls of a [[writer]], and a [[reader]] opens what it wrote. The shortest
useful program writes items into a file, [[closes|close]] the file and reads the items back. It shows each call a
recorder makes and what the file holds afterwards. The program here writes the drive's first minute.

## Running the program

The program of this chapter lives in the crate `example_drive`, beside the drive's recording. Run it from a
checkout of ww_tape:

```sh
cargo run --release -p example_drive --bin first_tape
```

It writes the drive's first minute into a file `drive.wwt` in a directory of the system's temporary directory, and
names the file. Then it opens the file and prints one line for each [[track]]:

```text
--8<-- "example_drive/tests/first_tape.out"
```

The counts follow from the drive's rates, which [What a tape holds](tape.md) lists. A second holds three log lines,
one temperature, 1 000 frames and 25 000 [[samples]]. The mode is idle, because the drive starts to run only after its first minute. A test checks that the program prints these lines, so they
change with the code that makes them.

## The definitions come first

A [[writer]] refuses an [[item]] of a [[track]] it does not know. Every write therefore takes the [[definitions]] of
the recording along: what each track holds and the [[clock]] its times count on. The program gets them from
[`Drive::definitions`] as a [`struct@Definitions`] container, which holds eight of them:

- the board's clock, in microseconds since its boot;
- the host's clock, in nanoseconds since 1970;
- a [[clock map]] that turns board time into host time;
- the five tracks `log`, `mode`, `can0`, `phase_a` and `temp`.

Defining a clock or a track gives it a small number, which every [[record]] and [[chunk]] in the file names it by. The
container picks the number, and [`Drive::definitions`] keeps them in a [`DriveNumbers`] for the program to write with.

The application never writes a definition itself. The writer [[puts]] each one into the file just before the first
[[block]] that needs it. It puts it again wherever the last one lies too far behind. [Tracks and items](tracks.md)
shows how to define a track of your own.

## Writing the minute

[`writer::create`] makes a new file at a path and returns a [`TapeWriter`] on it. The program then hands the
[[writer]] the minute, one act at a time. [`Drive::acts`] makes the acts from the drive's signals, in the order the
host receives them, and four calls of the writer take them. A fifth call [[closes|close]] the file:

```rs
--8<-- "example_drive/src/first_tape.rs:write"
```

??? example "What the example uses here: `Drive`, `Drive::acts`, `Drive::definitions`, `Act`, `records_block`, `DriveNumbers`, `Mode`"

    ```rs
    --8<-- "example_drive/src/lib.rs:drive_item"

    --8<-- "example_drive/src/record.rs:drive_acts_item"

    --8<-- "example_drive/src/record.rs:drive_definitions_item"

    --8<-- "example_drive/src/record.rs:act_item"

    --8<-- "example_drive/src/record.rs:records_block_item"

    --8<-- "example_drive/src/defs.rs:ids_item"

    --8<-- "example_drive/src/signals.rs:mode_type_item"
    ```

The program drives the writer from one thread, which then waits for every write. A recorder with several sources, or
a thread that must not wait for the disk, hands the acts to a [[mux]] instead. A [`TapeMux`] owns the writer and its
definitions on a thread of its own, the [[writing thread]].

A [[handle]] of the mux takes each kind of act: the mode's puts, the records blocks, the chunks of frames and the
samples. The drive's own recorder works so. It defines the clocks and tracks on the mux, then makes the handles:

```rs
--8<-- "example_drive/src/record.rs:create"

--8<-- "example_drive/src/record.rs:handles"
```

The recorder gives each act to its handle, in the order the host receives them, and waits until the mux has placed
it:

```rs
--8<-- "example_drive/src/record.rs:apply"
```

The recorder never passes a definition. The summaries of `phase_a` come from the mux: the handle of samples builds
them as it takes the samples in. The mux cuts to a new piece by the chain's policy. Driven by one thread, it writes the
same bytes every run, and a crash at any byte resumes to the same bytes.

[Writing from many threads](writing_thread.md) drives the same handles from a thread per source. The file the program
writes needs none of this and uses the writer's own calls.

??? example "What the example uses here: `Recorder`"

    ```rs
    --8<-- "example_drive/src/record.rs:recorder_item"

    --8<-- "example_drive/src/record.rs:series"
    ```

Each call of the writer does one job:

| Call | What it writes |
|---|---|
| [`TapeWriter::put`] | a [[state]] item, here the mode, given at each use; the writer writes it only when no record of it lies close behind |
| [`TapeWriter::write_block`] | a [[records block]] the application made, here a second of log lines and temperatures |
| [`TapeWriter::write_chunk`] | a [[chunk]] from columns the application encoded, here a second of frames or of samples |
| [`TapeWriter::defer_block`] | a [[summary]] block, placed where it fits |
| [`TapeWriter::close`] | the [[final index]], the [[close record]] and the [[tail]], then a sync to the disk |

The application makes a [[records block]] itself with a [`BlockWriter`]. It pushes each [[record]] with its
[[track]] and time, and the [`BlockWriter`] adds the [[block header]] and the checksum:

```rs
--8<-- "example_drive/src/record.rs:records"
```

??? example "What the example uses here: `DriveNumbers`"

    ```rs
    --8<-- "example_drive/src/defs.rs:ids_item"
    ```

A [[chunk]] takes more work, since its times and values go into encoded [[columns]]. [`Drive::acts`] does that work
here. [Event times and their codecs](columns.md) and [Samples and their codecs](sample_codecs.md) explain it.

## What the file holds

After [`TapeWriter::close`], the file holds one [[piece]]: a complete recording that a [[reader]] opens alone. Most
of its bytes are [[chunks]], and the end holds the index that lets a reader find any of them.

<figure class="ww-figure" markdown="0">
--8<-- "docs/plots/first_tape.svg"
<figcaption>The file the program writes. Its first KiB, enlarged, holds the puts that the first records block and the
first chunk of frames need, each right before its block. A second of the drive takes a records block, a chunk of
frames and a chunk of samples. At the end come the final index, the close padded to 4 KiB and the tail. Drawn by
<code>ww_tape_figures</code> from the file the program writes, at full rate.</figcaption>
</figure>

The file starts with [[puts]], each placed right before the first [[block]] that needs it. The [[layout]] and the
[[producer]] come first, then both [[clocks]], the map, the mode's [[track]] and the mode itself. The
[[definitions]] of `log` and `temp` precede the first [[records block]]. Those of `can0` and `phase_a` precede their
first chunks.

The minute ends with the [[summary blocks|summary]] still due, then the [[final index]]. That index lists every block
of the piece by track, so a reader finds the chunks of one track without reading the others. The [[close record]]
follows with the piece's [[content hash]], padded with zeros so that the [[tail]] ends on a 4 KiB boundary.

## Reading it back

A [[reader]] starts at the end of the [[piece]]. It finds the [[tail]] there, and from it the [[close record]] and the
[[final index]]. The program reads the whole file into memory and opens it with [`TapeReader::open`]:

```rs
--8<-- "example_drive/src/first_tape.rs:read"
```

??? example "What the example uses here: `Track, Items`"

    ```rs
    --8<-- "example_drive/src/first_tape.rs:tracks_types_item"
    ```

The close record holds a range for each [[track]]: its [[items]], its [[chunks]] and its times in the piece. The
[[definitions]] come from [`TapeReader::definitions_at`], which looks up every definition in force at a place in the
piece. Each is a shrink_wrap value of the type [`describe::Describe`], which the program decodes. It asks at the end
of the piece, so it sees all of them. The mode lies on a [[state track]], so the program asks
[`TapeReader::state_at`] for the mode in force there.

The program never needed the [[writer]]'s options or the drive's code to read the file. Everything it printed came
from the file: the names, the kinds and the counts. [Tracks and items](tracks.md) explains what a track is and how a
tape names it.
