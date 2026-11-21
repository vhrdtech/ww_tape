//! `WW_TAPE_BENCH_PROFILE` and `WW_TAPE_BENCH_OPT_LEVEL`: how the bench was built, for its report.

fn main() {
    for (var, from) in [
        ("WW_TAPE_BENCH_PROFILE", "PROFILE"),
        ("WW_TAPE_BENCH_OPT_LEVEL", "OPT_LEVEL"),
    ] {
        let v = std::env::var(from).unwrap_or_else(|_| "unknown".to_owned());
        println!("cargo:rustc-env={var}={v}");
    }
}
