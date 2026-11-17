//! ww_tape series tracks on any bytes: a track-0 definition (a track with its kind) decodes without panic; series
//! chunk heads built from the input on grids, then damaged by input bytes and pushed in any order, give a timeline
//! whose kept stretches hold its rules, and every tick, scale and grid query on it answers without panic.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| ww_tape_fuzz::targets::series(data));
