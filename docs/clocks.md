---
level: advanced
---

# Clocks and time

Every device counts time with its own counter, and no two counters agree exactly. A [[tape]] must keep each device's
own time exact and still place it on one shared time line. The drive, for example, stamps every [[item]] with its own
microsecond timer, which starts at 0 when the board boots. A reader of the recording wants wall time: the fault came
at 09:52, not 3 120 000 000 µs after boot. The board's crystal also runs 20 ppm fast, so its microseconds are not
quite the host's.

A tape defines each of its [[clocks]] with an exact rate and maps each device clock onto the host's. [[Epochs|epoch]]
keep a long capture in 64-bit ticks.

## Clocks and ticks

A [[clock]] is anything that counts time: the host's nanosecond clock, a device's microsecond timer, an audio codec's
[[sample]] clock. A [[tick]] is one count of a clock, and every time in a [[tape]] is a signed 64-bit count of ticks.
Every [[track]] names one clock, and its [[records]] count that clock's ticks.

A clock's [[definition]], a [`ClockDef`], states four facts. Its rate is the ticks per second, as an exact number. Its
resolution is the smallest step it really takes, which can be coarser than one tick. Its origin says what tick 0 is, and
its key names it across files as a [[track key]] names a track.

The origin is an identity, never a number. It is one of these:

- the device's boot, as for the drive's timer;
- the Unix epoch, as for the host's wall clock;
- the start of a media file or stream;
- the start of the [[producer]]'s session;
- nothing known, as for a free-running counter;
- the previous [[epoch]] of the same counter, for a long capture that [Boots and epochs](#boots-and-epochs) explains.

The drive defines two clocks. Clock 1 is the board's timer: a rate of 10⁶ ticks a second, origin the board's boot. Clock
2 is the host's wall clock: 10⁹ ticks a second, origin the Unix epoch. No [[block]] carries a wall time of its own,
because wall time is one more clock.

### Exact rates

A rate is exact, as a ratio or a power of ten, never a float. A [[clock]] that does not run at a whole number of hertz
therefore stays exact. A video at 60000/1001 frames a second lives on its stream's 60 000 Hz time base. Each frame lasts
1001 [[ticks]], and every frame time is exact.

A [[series]] gets the same care for its period. Suppose a recorder stamps everything on the host's nanosecond clock and
records 48 kHz audio. One [[sample]] lasts 1/48000 s, which is 20833⅓ ns and not a whole number of ticks. The [[track]]
keeps its period as the exact fraction 62500/3 ticks, and sample `i` sits at `⌊i · 62500/3⌋`:

| Sample | Exact tick | Stored tick |
|---:|---:|---:|
| 0 | 0 | 0 |
| 1 | 20833.33… | 20833 |
| 2 | 41666.66… | 41666 |
| 3 | 62500 | 62500 |
| 172 800 000 (one hour) | 3 600 000 000 000 | 3 600 000 000 000 |

Each sample rounds down on its own, so the rounding never adds up. After an hour the samples sit exactly one hour after
the first. A period rounded to 20833 ns would drift by 57.6 ms in that hour:

```rust
use ww_tape::{Ratio64, Rounding};

let period = Ratio64::new(62_500, 3).expect("a positive fraction");
let tick = |i: i64| period.apply_rounded(i, Rounding::Floor).expect("in i64");
assert_eq!([tick(1), tick(2), tick(3)], [20_833, 41_666, 62_500]);
// One hour of samples ends exactly one hour on.
let hour = 48_000 * 3_600;
assert_eq!(tick(hour), 3_600_000_000_000);
// A period of 20833 ns would end 57.6 ms early.
assert_eq!(tick(hour) - 20_833 * hour, 57_600_000);
```

## Clock maps

A [[clock map]] relates two [[clocks]], and nothing else in a [[tape]] does. A map is a list of [[map segments]], each
over a range of the first clock `a`. A segment says `b = b0 + (a - a0) · slope`, within an error bound in [[ticks]] of
the second clock `b`. The slope is a reduced fraction, so a fitted drift such as 1000000007/999999931 stays exact.

The drive's map takes the board's microseconds to the host's nanoseconds. Its one segment starts at the board's boot,
tick 0, which the host stamped at 09:00:00 UTC. Its slope is 49999/50 ns per µs, because
each microsecond of the fast crystal lasts 999.98 ns. Its error bound is 125 µs, the length of a USB high-speed
microframe, within which the host stamps what it receives.

<figure class="ww-figure" markdown="0">
--8<-- "docs/plots/clock_map.svg"
<figcaption>The drive's clock map. The board's count, read as host time, runs ahead by 20 ppm, so the host's time
falls 72 ms behind it in an hour: one straight segment. Had the host stepped its own clock forward, the map would go
on in a second segment. Drawn by <code>ww_tape_figures</code> from the drive's definitions; the step is drawn for
illustration.</figcaption>
</figure>

A [[reader]] turns any board time into host time with the segment. At minute 37 of the run, the board's timer reads 2
220 000 000 µs. The map places that at 09:36:59.9556 on the host's clock:

```rust
use shrink_wrap::prelude::*;
use ww_tape::{Describe, Rounding, Slot};
use example_drive::{BOOT_UNIX_NS, definitions};

let (defs, numbers) = definitions(25_000, true, 32 << 20).expect("the drive's definitions");
let slot = Slot::Map { from: numbers.board_clock, to: numbers.host_clock };
let bytes = defs.get(&slot).expect("the drive maps the board's clock to the host's");
let Describe::Map(map) = Describe::from_ww_bytes(bytes).expect("a definition") else {
    panic!("the slot of a map holds a map");
};
let segment = map.segments.iter().next().expect("one segment").expect("it reads");
let minute_37_us = 37 * 60 * 1_000_000;
let host_ns = segment.map(minute_37_us, Rounding::Floor).expect("mapped");
// 2219.9556 s after the boot at 09:00:00 UTC: 44.4 ms less than the board's count.
assert_eq!(host_ns - BOOT_UNIX_NS, 2_219_955_600_000);
```

A map must increase strictly across all its segments, or the order of [[items]] would flip between the two clocks. A
forward jump of the host's clock, such as a step from a time server, therefore starts a new segment. A backward step
cannot continue the map: the [[producer]] leaves an unmapped stretch or starts a new clock.

A map that the producer knows at the start goes into the [[definitions]], as the drive's does. A producer that estimates
drift as it runs writes its map as a [[track]] of its own instead, whose items are map segments. Each new estimate then
costs one [[record]], and the definitions never change.

## Boots and epochs

A device that restarts starts a new [[clock]]. The counter is the same, but the boot count is part of the new clock's
key. Only an estimated map relates two boots, because nothing on the board says how long it was off.

An [[epoch]] is different: it continues one counter. Signed 64-bit [[ticks]] cover about 292 years at 1 ns, but only
about 106 days at 1 ps.

A [[producer]] that needs more splits its capture into clock epochs. Each epoch is a new clock, of origin "epoch". An
exact map with slope 1 and error 0 links each epoch to the one before. Tick 0 of the new epoch is tick `b0` of the
previous one.

<figure class="ww-figure" markdown="0">
--8<-- "docs/figures/epochs.svg"
<figcaption>Boots against epochs. Each boot of the board is a clock of its own, placed on the host's time line by an
estimated map, with gaps of unknown length between. A long capture at fine ticks is one counter split into epochs,
linked exactly, which a reader joins into one time line.</figcaption>
</figure>

A [[reader]] joins the epochs into one time line, while the stored ticks stay 64-bit. [`Definitions::define_all`] checks
the link: an epoch clock must come with its link in the same call.

```rust
// i64 ticks at 1 ps last a little over 106 days.
assert_eq!(i64::MAX / 1_000_000_000_000 / 86_400, 106);
// At 1 ns they last 292 years of 365.25 days.
assert_eq!(i64::MAX / 1_000_000_000 / 31_557_600, 292);
```

A device's own time stays exact on its own clock, and maps place it on any other. The next part uses the clocks,
[[tracks]] and [[blocks]] of this part, starting with [Writing a tape](writing.md).
