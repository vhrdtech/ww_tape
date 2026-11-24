//! The book's pages (`docs/`) as doctests.
//!
//! Every `rust` block of a page listed here runs with `cargo test --doc`. Code that a page includes from a test is
//! fenced `rs` and is not a doctest. A page with a `rust` block has a line here (docs/writers.md, "Code on a page").
//!
//! A page's code links (``[`TapeWriter::put`]``) are the book's own, which `vz prep` resolves against every crate
//! of the book; rustdoc, which sees only this crate's scope, leaves them as text.
#![expect(
    rustdoc::broken_intra_doc_links,
    reason = "the pages' code links name items of crates this library does not depend on; the book build checks them"
)]

macro_rules! pages {
    ($($name:ident = $path:literal;)*) => {$(
        #[doc = include_str!(concat!("../../docs/", $path))]
        pub struct $name;
    )*};
}

pages! {
    Writers = "writers.md";
    Tracks = "tracks.md";
    Clocks = "clocks.md";
    Blocks = "blocks.md";
    Definitions = "definitions.md";
    Anatomy = "anatomy.md";
    Damage = "damage.md";
    RawMedia = "raw_media.md";
    Grids = "grids.md";
}
