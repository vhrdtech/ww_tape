//! A chain on memory tapes driven by the input's operations: writes, cuts, crashes and resumes, closes, removed
//! pieces and dropped fronts. It never panics, and after each operation that changes pieces a reader finds the
//! pieces and chunks the model holds ([crate::stores]).
use ww_tape::MemPieces;

pub fn stores(data: &[u8]) {
    crate::stores::run(MemPieces::new(), data);
}
