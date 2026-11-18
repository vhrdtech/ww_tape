//! The API snapshot check (`ww api check ww_tape`) as a test: the source must match the snapshot of the crate's
//! version in `api_snapshots/`, and a change must come with the version bump it needs. After a change to `Describe`
//! or a type it names, `WW_API_SAVE=1 cargo nextest run -p ww_tape --test evolution` (or `ww api save ww_tape`)
//! saves the snapshot of the current version first.

#[test]
fn api_evolution() {
    wire_weaver_evolution::Check::new(env!("CARGO_MANIFEST_DIR"))
        .require_current(true)
        .assert();
}
