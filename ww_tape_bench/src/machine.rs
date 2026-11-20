//! The machine a bench runs on and its load average: what a reader needs to weigh a time.

use serde::{Deserialize, Serialize};

/// The load average: runnable tasks averaged over 1, 5 and 15 minutes (`/proc/loadavg`).
#[derive(Copy, Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Load {
    pub one: f64,
    pub five: f64,
    pub fifteen: f64,
}

impl Load {
    /// The load average now; None where the system doesn't say (not Linux).
    pub fn now() -> Option<Self> {
        let text = std::fs::read_to_string("/proc/loadavg").ok()?;
        let mut v = text.split_whitespace().map(str::parse::<f64>);
        Some(Self {
            one: v.next()?.ok()?,
            five: v.next()?.ok()?,
            fifteen: v.next()?.ok()?,
        })
    }

    /// `5.62 before, 5.80 after` of the 1-minute averages, `unknown` where one is missing.
    pub fn span(before: Option<Self>, after: Option<Self>) -> String {
        let one =
            |l: Option<Self>| l.map_or_else(|| "unknown".to_owned(), |l| format!("{:.2}", l.one));
        format!("{} before, {} after", one(before), one(after))
    }
}

/// What a bench ran on.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Machine {
    /// The CPU's model name, as the system reports it.
    pub cpu: String,
    /// Logical CPUs the bench could use.
    pub cpus: usize,
    /// `x86_64`, `aarch64`, ...
    pub arch: String,
    /// The vector and CRC extensions the kernels of ww_tape may pick.
    pub extensions: Vec<String>,
}

impl Machine {
    /// This machine.
    pub fn this() -> Self {
        Self {
            cpu: cpu_model().unwrap_or_else(|| "unknown CPU".to_owned()),
            cpus: std::thread::available_parallelism().map_or(1, usize::from),
            arch: std::env::consts::ARCH.to_owned(),
            extensions: extensions(),
        }
    }
}

impl core::fmt::Display for Machine {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "{}, {} logical CPUs, {} ({})",
            self.cpu,
            self.cpus,
            self.arch,
            self.extensions.join(", ")
        )
    }
}

/// The model name of the first CPU in `/proc/cpuinfo`.
fn cpu_model() -> Option<String> {
    let text = std::fs::read_to_string("/proc/cpuinfo").ok()?;
    text.lines()
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
