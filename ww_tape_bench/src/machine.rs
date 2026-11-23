//! The machine a bench runs on and its load average: what a reader needs to weigh a time.

use std::time::Duration;

use ww_tape_bench_report::{Build, Load, Machine};

/// The load average now; None where the system doesn't say (not Linux).
pub fn load() -> Option<Load> {
    Load::parse(&std::fs::read_to_string("/proc/loadavg").ok()?)
}

/// This machine.
pub fn machine() -> Machine {
    let cpuinfo = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
    Machine {
        host: host().unwrap_or_else(|| "unknown host".to_owned()),
        virtual_machine: cpuinfo
            .lines()
            .any(|l| l.starts_with("flags") && l.split_whitespace().any(|f| f == "hypervisor")),
        cpu: cpu_model(&cpuinfo).unwrap_or_else(|| "unknown CPU".to_owned()),
        cpus: std::thread::available_parallelism().map_or(1, usize::from),
        arch: std::env::consts::ARCH.to_owned(),
        extensions: extensions(),
        memory_bytes: memory_bytes(),
    }
}

/// The CPU time this process has used so far, user and system of all its threads; None where the system doesn't say
/// (not Linux). Its resolution is a clock tick of 10 ms.
pub fn cpu_time() -> Option<Duration> {
    cpu_time_of(&std::fs::read_to_string("/proc/self/stat").ok()?)
}

/// The CPU time in the text of `/proc/PID/stat`: `utime` and `stime`, the 14th and 15th fields, in clock ticks of
/// 10 ms (`USER_HZ` is 100 on every Linux ABI).
fn cpu_time_of(stat: &str) -> Option<Duration> {
    // the command name, field 2, may hold spaces and parentheses: the fields restart after its last `)`
    let rest = stat.get(stat.rfind(')')? + 1..)?;
    let mut fields = rest.split_whitespace().skip(11);
    let user: u64 = fields.next()?.parse().ok()?;
    let system: u64 = fields.next()?.parse().ok()?;
    Some(Duration::from_millis(
        user.checked_add(system)?.checked_mul(10)?,
    ))
}

/// Bytes of memory the system has: `MemTotal` of `/proc/meminfo`; None where it doesn't say (not Linux).
fn memory_bytes() -> Option<u64> {
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
    let kib = meminfo
        .lines()
        .find_map(|l| l.strip_prefix("MemTotal:"))?
        .trim()
        .strip_suffix("kB")?
        .trim()
        .parse::<u64>()
        .ok()?;
    kib.checked_mul(1024)
}

/// How this bench was built: cargo's profile kind and opt-level (this crate's build script), and the jobs of
/// `CARGO_BUILD_JOBS`, which `just jobs=N` exports to the run as well.
pub fn build() -> Build {
    Build {
        profile: env!("WW_TAPE_BENCH_PROFILE").to_owned(),
        opt_level: env!("WW_TAPE_BENCH_OPT_LEVEL").to_owned(),
        jobs: std::env::var("CARGO_BUILD_JOBS")
            .ok()
            .and_then(|j| j.parse().ok()),
    }
}

/// The host name, from the kernel.
fn host() -> Option<String> {
    let h = std::fs::read_to_string("/proc/sys/kernel/hostname").ok()?;
    let h = h.trim();
    (!h.is_empty()).then(|| h.to_owned())
}

/// The model name of the first CPU in `/proc/cpuinfo`'s text.
fn cpu_model(cpuinfo: &str) -> Option<String> {
    cpuinfo
        .lines()
        .find_map(|l| l.strip_prefix("model name")?.split_once(':'))
        .map(|(_, v)| v.trim().to_owned())
}

#[cfg(target_arch = "x86_64")]
fn extensions() -> Vec<String> {
    let mut v = Vec::new();
    if std::arch::is_x86_feature_detected!("sse4.2") {
        v.push("sse4.2");
    }
    if std::arch::is_x86_feature_detected!("avx2") {
        v.push("avx2");
    }
    if std::arch::is_x86_feature_detected!("avx512bw")
        && std::arch::is_x86_feature_detected!("avx512vl")
    {
        v.push("avx512bw+vl");
    }
    v.into_iter().map(str::to_owned).collect()
}

#[cfg(target_arch = "aarch64")]
fn extensions() -> Vec<String> {
    let mut v = vec!["neon".to_owned()];
    if std::arch::is_aarch64_feature_detected!("crc") {
        v.push("crc".to_owned());
    }
    v
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
fn extensions() -> Vec<String> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_time_adds_user_and_system_ticks_after_a_command_name_with_parentheses() {
        let stat = "42 (a (b) c) S 1 2 3 4 5 6 7 8 9 10 250 50 0 0 20 0 1 0 100";
        assert_eq!(cpu_time_of(stat), Some(Duration::from_millis(3_000)));
        assert_eq!(cpu_time_of("42 (x) S 1"), None);
    }
}
