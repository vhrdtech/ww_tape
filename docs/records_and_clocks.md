# Records, tracks and clocks

This page explains how a tape stores time, and how it relates the time lines of different devices. It assumes the
words of [Tape concepts](concepts.md).

## Records and tracks

A **track** is a named sequence of items of one type. Examples are the frames a link received, the temperature
readings of a sensor, and the log lines of a firmware.

A **record** is one item on one track at one time.

A track's definition (`TrackDef`) gives the track a small number for use inside the file. It also gives the track a
stable key that names the same track across files.

The definition also names the WireWeaver resource path that the items come from.

## Clocks and ticks

A **clock** is anything that counts time: the host's nanosecond clock, a device's microsecond timer since boot, an
audio codec's sample clock. A **tick** is one step of a clock, and every record time is a signed 64-bit count of ticks.

A track's definition names the clock that its times count.

A clock's definition (`ClockDef`) gives an exact rate in ticks per second and an exact resolution. It also gives an
**origin**: what tick 0 is.

The origin is an identity, never a number. It is one of these:

- a device's boot;
- the Unix epoch;
- the start of a media file;
- the start of the producer's session;
- nothing known, as for a free-running counter.

Rates are exact. A clock that does not run at a whole number of hertz therefore stays exact.

For example, a video at 60000/1001 frames per second lives on its stream's 60 000 Hz time base. Each frame lasts 1001
ticks, and every frame time is exact.

### Clocks that don't share a time base

A **clock map** relates two clocks, and nothing else does. A map is a list of **map segments**, each over a range of
clock `a`.

Each segment says `b = b0 + (a - a0) · slope`, with an error bound in ticks of clock `b`. The slope is a reduced
fraction.

A fitted drift such as 1000000007/999999931 therefore stays exact. A map must increase strictly across all its
segments, or the order of items would flip between the clocks.

A forward jump of the wall clock, such as a step from a time server, starts a new map segment. A backward step is a
gap or a new clock.

Wall time is one more clock, with the Unix epoch as its origin. No block carries a wall time of its own.

A producer that knows the wall time at its start writes a map from its clock to the wall clock. A producer that
estimates drift over time writes the map as a track of its own, whose items are map segments.

A device restart is a new clock: the same counter, with the boot count as part of its key. Only an estimated map
relates the two boots, because nothing says how long the device was off.

### Epochs: long captures at fine ticks

Signed 64-bit ticks cover about 292 years at 1 ns, but only about 106 days at 1 ps. A producer cuts a capture that
needs more into **clock epochs**.

Each epoch is a new clock of the same counter. An exact map with slope 1 and error 0 says that tick 0 of the new epoch
is tick `b0` of the previous one.

A reader joins the epochs into one long time line, while the stored ticks stay 64-bit.

### Example: 48 kHz audio on nanosecond ticks

A recorder stamps everything on the host's nanosecond clock and records a 48 kHz audio stream. One sample lasts
1/48000 s, which is 20833⅓ ns and not a whole number of ticks.

The track stores its period as the exact fraction 62500/3 ticks per sample. Sample `i` sits at tick
`start + ⌊i · 62500/3⌋`:

| Sample | Exact tick | Stored tick |
|---:|---:|---:|
| 0 | 0 | 0 |
| 1 | 20833.33… | 20833 |
| 2 | 41666.66… | 41666 |
| 3 | 62500 | 62500 |
| 172 800 000 (one hour) | 3 600 000 000 000 | 3 600 000 000 000 |

The formula rounds each sample's tick down on its own, and the rounding never adds up. After an hour, the samples sit
exactly one hour after the first.

A period rounded to 20833 ns would drift by 57.6 ms per hour.

Instead, the recorder could put the audio on a 48 kHz clock of its own. A map then relates that clock to the host
clock.

That is the right choice when the codec's crystal drifts against the host.
