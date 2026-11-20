//! The workloads: seeded generators of start ticks (and ends, for spans), and a system journal read from a file.

use ww_tape_bench::Rng;

/// Start ticks, non-decreasing, and for spans an end per start.
pub struct Workload {
    /// What it is, as the table names it.
    pub name: String,
    pub starts: Vec<i64>,
    pub ends: Option<Vec<i64>>,
}

const US: i64 = 1_000;
const MS: i64 = 1_000_000;

/// Log-uniform in `[lo, hi)`, truncated: as many values per decade.
fn log_uniform(r: &mut Rng, lo: f64, hi: f64) -> i64 {
    libm::exp(libm::log(lo) + r.unit() * (libm::log(hi) - libm::log(lo))) as i64
}

/// Exponential with mean `mean`, truncated: the gaps of a Poisson process.
fn exponential(r: &mut Rng, mean: f64) -> i64 {
    (-libm::log(1.0 - r.unit()) * mean) as i64
}

/// Exactly every 1 ms on a 1 ns clock, 1 to 9 events missing after about every 100 000.
pub fn exact_gaps(n: usize) -> Workload {
    let mut r = Rng(3);
    let mut starts = Vec::with_capacity(n);
    let mut k = 0i64;
    while starts.len() < n {
        starts.push(k * MS);
        k += 1;
        if r.below(100_000) == 0 {
            k += r.range(1, 10);
        }
    }
    Workload {
        name: "exact 1 ms period with a few gaps".to_owned(),
        starts,
        ends: None,
    }
}

/// Every 1 ms with uniform jitter of ±300 ns, on a 1 ns clock.
pub fn jitter(n: usize) -> Workload {
    let mut r = Rng(2);
    let starts = (0..n as i64).map(|i| i * MS + r.range(-300, 301)).collect();
    Workload {
        name: "1 ms period, ±300 ns jitter".to_owned(),
        starts,
        ends: None,
    }
}

/// Bursty protocol traffic on a 1 ns clock: bursts of 32 packets on average, 1 to 20 µs apart inside a burst, bursts
/// 1 ms to 2 s apart (log-uniform).
pub fn bursty(n: usize) -> Workload {
    let mut r = Rng(1);
    let mut starts = Vec::with_capacity(n);
    let mut t = 0i64;
    while starts.len() < n {
        let burst = 1 + exponential(&mut r, 31.0) as usize;
        for _ in 0..burst.min(n - starts.len()) {
            starts.push(t);
            t += r.range(US, 20 * US + 1);
        }
        t += log_uniform(&mut r, MS as f64, 2e9);
    }
    Workload {
        name: "bursty traffic".to_owned(),
        starts,
        ends: None,
    }
}

/// `n` video frames at 60000/1001 frames per second, on a clock of `hz`: exact on the stream's own 60 000 Hz, floored
/// on a faster one.
pub fn frames(n: usize, hz: u64) -> Workload {
    let starts = (0..n as u64)
        .map(|i| (u128::from(i) * 1001 * u128::from(hz) / 60_000) as i64)
        .collect();
    let name = if hz == 60_000 {
        "video frames on their 60 000 Hz clock"
    } else {
        "video frames on a 1 ns clock"
    };
    Workload {
        name: name.to_owned(),
        starts,
        ends: None,
    }
}

/// Spans back to back on a 1 ns clock: 1 µs to 1 ms long (log-uniform), each ending where the next starts.
pub fn spans_back_to_back(n: usize) -> Workload {
    let mut r = Rng(5);
    let mut edges = Vec::with_capacity(n + 1);
    let mut t = 0;
    for _ in 0..=n {
        edges.push(t);
        t += log_uniform(&mut r, 1e3, 1e6);
    }
    let ends = edges.get(1..).unwrap_or_default().to_vec();
    edges.truncate(n);
    Workload {
        name: "spans back to back".to_owned(),
        starts: edges,
        ends: Some(ends),
    }
}

/// Spans with gaps on a 1 ns clock: 1 µs to 1 ms long, 1 µs to 10 ms apart, both log-uniform.
pub fn spans_gaps(n: usize) -> Workload {
    let mut r = Rng(6);
    let mut starts = Vec::with_capacity(n);
    let mut ends = Vec::with_capacity(n);
    let mut t = 0;
    for _ in 0..n {
        let d = log_uniform(&mut r, 1e3, 1e6);
        starts.push(t);
        ends.push(t + d);
        t += d + log_uniform(&mut r, 1e3, 1e7);
    }
    Workload {
        name: "spans with gaps".to_owned(),
        starts,
        ends: Some(ends),
    }
}

/// The entries of a system journal on a 1 µs clock: one `SECONDS.MICROS` time at the start of each line of `text`
/// (`journalctl -o short-unix`), the rest of the line ignored; sorted, since a journal is merged from several sources.
pub fn journal(text: &str) -> Workload {
    let mut starts: Vec<i64> = text
        .lines()
        .filter_map(|l| {
            let (s, u) = l.split_whitespace().next()?.split_once('.')?;
            Some(s.parse::<i64>().ok()? * 1_000_000 + u.parse::<i64>().ok()?)
        })
        .collect();
    starts.sort_unstable();
    Workload {
        name: "a system journal, 1 µs clock".to_owned(),
        starts,
        ends: None,
    }
}
