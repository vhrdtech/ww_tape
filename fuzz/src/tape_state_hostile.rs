//! A ww_tape piece whose state records and index state entries lie behind good CRCs (ww_tape_fuzz::hostile::state):
//! lookups hand out only the records their entries name, and a resume finishes whole.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| ww_tape_fuzz::targets::state_hostile(data));
