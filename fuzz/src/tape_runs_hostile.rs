//! A ww_tape piece whose runs of blocks marked FLAG_WITH_NEXT gain or lose marks, take damage or are cut
//! (ww_tape_fuzz::hostile::runs): no reader shows part of a run it dropped.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| ww_tape_fuzz::targets::runs_hostile(data));
