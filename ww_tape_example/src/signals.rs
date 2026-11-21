//! What the drive does over the run: its mode, its phase current, its temperature, its CAN frames and its log lines,
//! each a pure function of the time or of the item's number, so that any part of the run is made alone and every run
//! is the same.

use core::f64::consts::PI;

/// Microseconds of the board's clock in one millisecond.
pub(crate) const US_PER_MS: i64 = 1_000;
/// Microseconds in one hour: item rates per hour are spread over it.
pub(crate) const US_PER_HOUR: u64 = 3_600_000_000;

/// The minute the bench switches the drive from idle to run.
pub const RUN_MINUTE: u32 = 1;
/// The minute the host loses power in the story.
pub const CRASH_MINUTE: u32 = 37;
/// The minute the drive faults on overcurrent.
pub const FAULT_MINUTE: u32 = 52;

/// Milliseconds since boot at which the drive faults.
pub(crate) const FAULT_MS: i64 = FAULT_MINUTE as i64 * 60_000;
/// Milliseconds since boot at which the drive runs.
const RUN_MS: i64 = RUN_MINUTE as i64 * 60_000;
/// How long the current rises past the limit before the drive trips, ms.
const OVERCURRENT_MS: i64 = 20;
/// The current limit the drive trips at, mA: within the 16-bit range of `phase_a`, so the samples show the overcurrent
/// unclipped.
pub const LIMIT_MA: i32 = 31_000;
/// How far past the limit the current rises before the drive trips, mA.
const OVERSHOOT_MA: f64 = 1_500.0;

// --8<-- [start:mode_type_item]
/// The drive's mode, the state track `mode`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Mode {
    Idle,
    Run,
    Fault,
}

impl Mode {
    /// The mode in force at `us` since boot.
    pub fn at(us: i64) -> Self {
        let ms = us.div_euclid(US_PER_MS);
        if ms >= FAULT_MS {
            Self::Fault
        } else if ms >= RUN_MS {
            Self::Run
        } else {
            Self::Idle
        }
    }

    /// The item a put of the mode carries.
    pub fn item(self) -> &'static [u8] {
        match self {
            Self::Idle => b"idle",
            Self::Run => b"run, 31 A limit",
            Self::Fault => b"fault: overcurrent",
        }
    }

    /// Microseconds since boot at which the mode after `self` starts, if any.
    pub(crate) fn next_change_us(self) -> Option<i64> {
        match self {
            Self::Idle => Some(RUN_MS * US_PER_MS),
            Self::Run => Some(FAULT_MS * US_PER_MS),
            Self::Fault => None,
        }
    }
}
// --8<-- [end:mode_type_item]

/// A number from `seed` and `n`, the same for the same pair (SplitMix64).
pub(crate) fn hash(seed: u64, n: u64) -> u64 {
    let mut z = seed ^ n.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// The peak phase current at `ms` since boot, mA: none while idle; in run a cycle of five minutes from 5 A up to
/// 30 A and down again; rising past the limit in the last 20 ms before the fault, none after it.
fn peak_ma(ms: i64) -> f64 {
    if !(RUN_MS..FAULT_MS).contains(&ms) {
        return 0.0;
    }
    if ms >= FAULT_MS - OVERCURRENT_MS {
        let rise = f64::from(i32::try_from(ms - (FAULT_MS - OVERCURRENT_MS)).unwrap_or(0));
        return 30_000.0 + (f64::from(LIMIT_MA) - 30_000.0 + OVERSHOOT_MA) * rise / 20.0;
    }
    let cycle = f64::from(i32::try_from((ms - RUN_MS) % 300_000).unwrap_or(0)) / 300_000.0;
    5_000.0 + 25_000.0 * 0.5 * (1.0 - libm::cos(2.0 * PI * cycle))
}

/// Samples of the phase current per second at full scale, and the electrical frequency there.
const FULL_HZ: f64 = 25_000.0;
const ELECTRICAL_HZ: f64 = 50.0;

/// Phase current sample `n` at `us` since boot, mA, of a series of `hz` samples a second.
///
/// The current is a sine of the load cycle's peak with a little noise, 50 Hz at 25 kHz and slower in proportion at a lower rate,
/// so that a scaled run keeps 500 samples a cycle.
pub fn phase_ma(us: i64, n: u64, hz: u32) -> i16 {
    let ms = us.div_euclid(US_PER_MS);
    let peak = peak_ma(ms);
    #[expect(
        clippy::cast_precision_loss,
        reason = "an hour of microseconds is far below 2^52: exact in an f64"
    )]
    let t = us as f64 / 1e6;
    let noise = f64::from(u16::try_from(hash(0x5a, n) >> 56).unwrap_or(0)) - 127.5;
    let f = ELECTRICAL_HZ * f64::from(hz) / FULL_HZ;
    let ma = peak * libm::sin(2.0 * PI * f * t) + noise / 4.0;
    #[expect(
        clippy::cast_possible_truncation,
        reason = "clamped to the i16 range first"
    )]
    let v = libm::round(ma.clamp(f64::from(i16::MIN), f64::from(i16::MAX))) as i16;
    v
}

/// The root mean square of the phase current at `ms` since boot, mA.
fn rms_ma(ms: i64) -> u32 {
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a peak of at most 32.5 A: in u32, and never negative"
    )]
    let v = libm::round(peak_ma(ms) / core::f64::consts::SQRT_2) as u32;
    v
}

/// The board temperature at `ms` since boot, in hundredths of a degree Celsius: from 25 °C towards 65 °C while it
/// runs, cooling after the fault.
pub fn temp_centi(ms: i64) -> i16 {
    #[expect(
        clippy::cast_precision_loss,
        reason = "an hour of milliseconds is far below 2^52: exact in an f64"
    )]
    let minutes = ms as f64 / 60_000.0;
    let heated = 40.0 * (1.0 - libm::exp(-minutes / 20.0));
    let c = if ms < FAULT_MS {
        25.0 + heated
    } else {
        let at_fault = 25.0 + 40.0 * (1.0 - libm::exp(-f64::from(FAULT_MINUTE) / 20.0));
        #[expect(
            clippy::cast_precision_loss,
            reason = "an hour of milliseconds is far below 2^52: exact in an f64"
        )]
        let since = (ms - FAULT_MS) as f64 / 60_000.0;
        25.0 + (at_fault - 25.0) * libm::exp(-since / 10.0)
    };
    #[expect(
        clippy::cast_possible_truncation,
        reason = "25 to 65 °C in hundredths: in i16"
    )]
    let v = libm::round(c * 100.0) as i16;
    v
}

/// The CAN identifiers the drive sends, in turn.
const FRAME_IDS: [u32; 4] = [0x101, 0x181, 0x281, 0x381];
/// The identifier of the drive's emergency frame, sent once at the fault.
pub const EMERGENCY_ID: u32 = 0x081;

/// CAN frame `k` on `can0`: its time in µs since boot and its payload.
///
/// The payload is the 11-bit identifier (u32 LE), then the data. Frames are due `period_us` apart and each comes up to 40 % of that late. The first frame due at or after the fault
/// is the emergency frame.
pub fn frame(k: u64, period_us: u64) -> (i64, Vec<u8>) {
    let nominal = k.saturating_mul(period_us);
    let late = hash(0xca, k) % (period_us * 2 / 5).max(1);
    let us = i64::try_from(nominal.saturating_add(late)).unwrap_or(i64::MAX);
    let ms = us.div_euclid(US_PER_MS);
    let id = if k == emergency_frame(period_us) {
        EMERGENCY_ID
    } else {
        let turn = usize::try_from(k % 4).unwrap_or(0);
        FRAME_IDS.get(turn).copied().unwrap_or(0x101)
    };
    let mut payload = id.to_le_bytes().to_vec();
    match id {
        // command: the speed setpoint in rpm and the torque limit in 0.1 A
        0x101 => {
            let rpm: u16 = if Mode::at(us) == Mode::Run { 1500 } else { 0 };
            payload.extend(rpm.to_le_bytes());
            // the trip limit: 310 in 0.1 A
            let limit = u16::try_from(LIMIT_MA / 100).unwrap_or(u16::MAX);
            payload.extend(limit.to_le_bytes());
            payload.extend([0; 4]);
        }
        // status: the mode and the phase current's RMS in mA
        0x181 => {
            payload.push(match Mode::at(us) {
                Mode::Idle => 0,
                Mode::Run => 1,
                Mode::Fault => 2,
            });
            payload.extend(rms_ma(ms).to_le_bytes());
            payload.extend([0; 3]);
        }
        // temperatures: the board's in 0.01 °C
        0x281 => {
            payload.extend(temp_centi(ms).to_le_bytes());
            payload.extend([0; 6]);
        }
        // the emergency frame: error code 0x2310, overcurrent
        EMERGENCY_ID => {
            payload.extend(0x2310u16.to_le_bytes());
            payload.extend([0; 6]);
        }
        // bus voltage in mV
        _ => {
            payload.extend(48_000u32.to_le_bytes());
            payload.extend([0; 4]);
        }
    }
    (us, payload)
}

/// The number of the emergency frame: the first frame due at or after the fault.
pub fn emergency_frame(period_us: u64) -> u64 {
    let fault_us = FAULT_MS.unsigned_abs() * US_PER_MS.unsigned_abs();
    fault_us.div_ceil(period_us.max(1))
}

/// Log line `n` of the firmware at `ms` since boot.
pub fn log_line(n: u64, ms: i64) -> String {
    let at = format!(
        "{:02}:{:02}.{:03}",
        ms / 60_000,
        ms / 1_000 % 60,
        ms % 1_000
    );
    match Mode::at(ms * US_PER_MS) {
        Mode::Idle => format!("{at} idle, waiting for the bench"),
        Mode::Run => format!(
            "{at} run: phase A {} mA rms, {} rpm, line {n}",
            rms_ma(ms),
            1500
        ),
        Mode::Fault => format!("{at} fault: overcurrent past {LIMIT_MA} mA, outputs off"),
    }
}
