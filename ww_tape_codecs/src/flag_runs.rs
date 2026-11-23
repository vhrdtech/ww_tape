//! The flag runs column of a series chunk (`Codec::FLAG_RUNS`): the samples whose flags word is not 0, as runs of
//! consecutive samples sharing one word.
//!
//! ```text
//! n × run, n from 1 to FLAG_RUNS_MAX, ascending:
//!   u32  first: the run's first sample, counted from the chunk's first
//!   u32  count: samples in the run, 1 and up
//!   u32  flags: their flags word, not 0
//! ```
//!
//! A sample in no run has the flags word 0. Runs lie in the chunk's samples and apart. Two runs that touch have
//! other words: a writer merges them. So the column is in one form only, and a reader takes no other. A chunk with
//! no flagged sample has no such column.
//!
//! [FlagRuns] reads a column in place over its bytes and the chunk's sample count; [FlagRunsBuilder] writes one as
//! samples come.

#[cfg(feature = "alloc")]
use alloc::vec::Vec;
use core::ops::Range;

use crate::Codec;
use crate::bytes::le_u32;

/// Bytes of one run.
pub const FLAG_RUN_LEN: usize = 12;

/// Most runs of one chunk: 48 KiB of column. A writer completes a chunk before a run past it.
pub const FLAG_RUNS_MAX: usize = 4096;

/// A run of consecutive samples sharing one flags word.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct FlagRun {
    /// The run's first sample, counted from the chunk's first.
    pub first: u32,
    /// Samples in the run.
    pub count: u32,
    /// Their flags word.
    pub flags: u32,
}

impl FlagRun {
    /// The sample after the run's last; None past u32.
    pub fn end(&self) -> Option<u32> {
        self.first.checked_add(self.count)
    }
}

/// Why a flag runs column can't be read or a run added.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum FlagRunsError {
    /// The chunk's flags column is in another codec than [Codec::FLAG_RUNS]: an events chunk's, or one this reader
    /// doesn't know.
    Codec(Codec),
    /// A column of no run, or not a whole number of runs.
    Length { len: usize },
    /// More runs than [FLAG_RUNS_MAX].
    TooMany { runs: usize },
    /// Run `run` holds no sample, or its flags word is 0.
    Empty { run: usize },
    /// Run `run` starts before the one before ends, or touches it with the same flags word.
    Order { run: usize },
    /// Run `run` ends past the chunk's samples.
    PastEnd { run: usize },
}

impl core::fmt::Display for FlagRunsError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Codec(c) => write!(f, "a flags column in codec {}, not flag runs", c.0),
            Self::Length { len } => write!(f, "a flag runs column of {len} bytes"),
            Self::TooMany { runs } => write!(f, "{runs} flag runs, over {FLAG_RUNS_MAX}"),
            Self::Empty { run } => write!(f, "flag run {run} of no sample or no flag"),
            Self::Order { run } => {
                write!(f, "flag run {run} overlaps or repeats the one before")
            }
            Self::PastEnd { run } => write!(f, "flag run {run} past the chunk's samples"),
        }
    }
}

impl core::error::Error for FlagRunsError {}

/// The flag runs column of a chunk, its runs checked: read in place.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct FlagRuns<'b> {
    runs: &'b [[u8; FLAG_RUN_LEN]],
}

impl<'b> FlagRuns<'b> {
    /// No run: the flags of a chunk without the column.
    pub const NONE: Self = Self { runs: &[] };

    /// Read `column` as the runs of a chunk of `count` samples; refused unless it is in the writer's form.
    pub fn of(column: &'b [u8], count: u32) -> Result<Self, FlagRunsError> {
        let (runs, rest) = column.as_chunks::<FLAG_RUN_LEN>();
        if runs.is_empty() || !rest.is_empty() {
            return Err(FlagRunsError::Length { len: column.len() });
        }
        if runs.len() > FLAG_RUNS_MAX {
            return Err(FlagRunsError::TooMany { runs: runs.len() });
        }
        let out = Self { runs };
        let mut before: Option<FlagRun> = None;
        for (i, r) in out.iter().enumerate() {
            if r.count == 0 || r.flags == 0 {
                return Err(FlagRunsError::Empty { run: i });
            }
            let end = r.end().ok_or(FlagRunsError::PastEnd { run: i })?;
            if end > count {
                return Err(FlagRunsError::PastEnd { run: i });
            }
            if let Some(b) = before {
                // the run before ended in the chunk, so its end is a number
                let b_end = b.end().ok_or(FlagRunsError::PastEnd { run: i })?;
                if r.first < b_end || (r.first == b_end && r.flags == b.flags) {
                    return Err(FlagRunsError::Order { run: i });
                }
            }
            before = Some(r);
        }
        Ok(out)
    }

    /// Runs in the column.
    pub fn len(&self) -> usize {
        self.runs.len()
    }

    /// Whether there is no run: no sample of the chunk is flagged.
    pub fn is_empty(&self) -> bool {
        self.runs.is_empty()
    }

    /// Run `i`; None past the last.
    pub fn get(&self, i: usize) -> Option<FlagRun> {
        self.runs.get(i).map(run_of)
    }

    /// The runs, in order.
    pub fn iter(&self) -> impl Iterator<Item = FlagRun> + 'b {
        self.runs.iter().map(run_of)
    }

    /// The samples `0..count` cut where their flags word changes: each stretch with its word, those of no run with
    /// 0. Stretches are not empty and cover `0..count` in order; runs past `count` are cut at it.
    pub fn spans(&self, count: u32) -> impl Iterator<Item = (Range<u32>, u32)> + 'b {
        let mut runs = self.iter().peekable();
        let mut at = 0u32;
        core::iter::from_fn(move || {
            while at < count {
                let next = runs.peek().copied();
                match next {
                    Some(r) if r.first <= at => {
                        runs.next();
                        let end = r.end().unwrap_or(u32::MAX).min(count);
                        if end > at {
                            let span = at..end;
                            at = end;
                            return Some((span, r.flags));
                        }
                    }
                    Some(r) => {
                        let end = r.first.min(count);
                        let span = at..end;
                        at = end;
                        return Some((span, 0));
                    }
                    None => {
                        let span = at..count;
                        at = count;
                        return Some((span, 0));
                    }
                }
            }
            None
        })
    }

    /// The flags word of sample `i` of the chunk: 0 in no run.
    pub fn flags_at(&self, i: u32) -> u32 {
        let k = self.runs.partition_point(|r| run_of(r).first <= i);
        k.checked_sub(1)
            .and_then(|k| self.get(k))
            .filter(|r| r.end().is_none_or(|end| i < end))
            .map_or(0, |r| r.flags)
    }
}

fn run_of(r: &[u8; FLAG_RUN_LEN]) -> FlagRun {
    let word = |at| le_u32(r, at).unwrap_or(0);
    FlagRun {
        first: word(0),
        count: word(4),
        flags: word(8),
    }
}

/// The runs of a chunk being written, as samples come: [Self::push] each stretch with its flags word.
#[cfg(feature = "alloc")]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FlagRunsBuilder {
    runs: Vec<FlagRun>,
    /// Samples pushed so far
    next: u32,
}

#[cfg(feature = "alloc")]
impl FlagRunsBuilder {
    /// The runs of a chunk with no sample yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether a push of samples flagged `flags` fits: they start no run, or a run under [FLAG_RUNS_MAX].
    pub fn fits(&self, flags: u32) -> bool {
        flags == 0 || self.extends(flags) || self.runs.len() < FLAG_RUNS_MAX
    }

    /// Whether samples flagged `flags` pushed now go on with the last run.
    fn extends(&self, flags: u32) -> bool {
        self.runs
            .last()
            .is_some_and(|r| r.flags == flags && r.end() == Some(self.next))
    }

    /// Add the next `count` samples, each flagged `flags` (0: none). Refused when they would start a run past
    /// [FLAG_RUNS_MAX] ([Self::fits]) or count past u32 samples; nothing is added then.
    pub fn push(&mut self, count: u32, flags: u32) -> Result<(), FlagRunsError> {
        let runs = self.runs.len();
        let next = self
            .next
            .checked_add(count)
            .ok_or(FlagRunsError::PastEnd { run: runs })?;
        if count == 0 || flags == 0 {
            self.next = next;
            return Ok(());
        }
        if self.extends(flags)
            && let Some(last) = self.runs.last_mut()
        {
            last.count = last.count.saturating_add(count);
        } else if runs < FLAG_RUNS_MAX {
            self.runs.push(FlagRun {
                first: self.next,
                count,
                flags,
            });
        } else {
            return Err(FlagRunsError::TooMany {
                runs: runs.saturating_add(1),
            });
        }
        self.next = next;
        Ok(())
    }

    /// The runs so far.
    pub fn runs(&self) -> &[FlagRun] {
        &self.runs
    }

    /// Whether no sample so far is flagged: the chunk gets no flag runs column.
    pub fn is_empty(&self) -> bool {
        self.runs.is_empty()
    }

    /// Bytes of the column ([Self::write]).
    pub fn len(&self) -> usize {
        self.runs.len().saturating_mul(FLAG_RUN_LEN)
    }

    /// Append the column's bytes to `out`.
    pub fn write(&self, out: &mut Vec<u8>) {
        out.reserve(self.len());
        for r in &self.runs {
            out.extend_from_slice(&r.first.to_le_bytes());
            out.extend_from_slice(&r.count.to_le_bytes());
            out.extend_from_slice(&r.flags.to_le_bytes());
        }
    }

    /// Start the next chunk: no run, no sample.
    pub fn clear(&mut self) {
        self.runs.clear();
        self.next = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn column(runs: &[(u32, u32, u32)]) -> Vec<u8> {
        runs.iter()
            .flat_map(|&(a, b, c)| [a, b, c])
            .flat_map(u32::to_le_bytes)
            .collect()
    }

    /// Pushes merge into the writer's form, and the column reads back to the same runs and spans.
    #[test]
    fn pushes_merge_and_read_back() {
        let mut b = FlagRunsBuilder::new();
        for (n, f) in [
            (3, 0),
            (2, 5),
            (1, 5),
            (0, 9),
            (4, 6),
            (2, 0),
            (1, 6),
            (3, 0),
        ] {
            b.push(n, f).unwrap();
        }
        let want = [(3, 3, 5), (6, 4, 6), (12, 1, 6)];
        let mut bytes = vec![];
        b.write(&mut bytes);
        assert_eq!(bytes, column(&want));
        let r = FlagRuns::of(&bytes, 16).unwrap();
        let got: Vec<_> = r.iter().map(|r| (r.first, r.count, r.flags)).collect();
        assert_eq!(got, want);
        let spans: Vec<_> = r.spans(16).collect();
        assert_eq!(
            spans,
            [
                (0..3, 0),
                (3..6, 5),
                (6..10, 6),
                (10..12, 0),
                (12..13, 6),
                (13..16, 0)
            ]
        );
        let flags: Vec<u32> = (0..16).map(|i| r.flags_at(i)).collect();
        assert_eq!(flags, [0, 0, 0, 5, 5, 5, 6, 6, 6, 6, 0, 0, 6, 0, 0, 0]);
        assert_eq!(FlagRuns::NONE.spans(4).collect::<Vec<_>>(), [(0..4, 0)]);
        assert_eq!(FlagRuns::NONE.spans(0).count(), 0);
    }

    /// Every other form is refused: no run, part of a run, an empty run, a run of no flag, overlap, a touching run
    /// of the same word, a run past the samples or past u32.
    #[test]
    fn other_forms_are_refused() {
        use FlagRunsError as E;
        let cases: [(&[u8], E); 8] = [
            (&[], E::Length { len: 0 }),
            (&column(&[(0, 1, 1)])[..11], E::Length { len: 11 }),
            (&column(&[(0, 0, 1)]), E::Empty { run: 0 }),
            (&column(&[(0, 1, 0)]), E::Empty { run: 0 }),
            (&column(&[(0, 4, 1), (3, 1, 2)]), E::Order { run: 1 }),
            (&column(&[(0, 4, 1), (4, 1, 1)]), E::Order { run: 1 }),
            (&column(&[(8, 3, 1)]), E::PastEnd { run: 0 }),
            (&column(&[(u32::MAX, 2, 1)]), E::PastEnd { run: 0 }),
        ];
        for (bytes, want) in cases {
            assert_eq!(FlagRuns::of(bytes, 10), Err(want), "{bytes:?}");
        }
        assert!(FlagRuns::of(&column(&[(0, 4, 1), (4, 1, 2)]), 5).is_ok());
        let many: Vec<_> = (0..=FLAG_RUNS_MAX as u32).map(|i| (i * 2, 1, 1)).collect();
        assert_eq!(
            FlagRuns::of(&column(&many), u32::MAX),
            Err(E::TooMany {
                runs: FLAG_RUNS_MAX + 1
            })
        );
    }

    /// A builder takes runs up to the cap, then only pushes that start none.
    #[test]
    fn builder_stops_at_the_cap() {
        let mut b = FlagRunsBuilder::new();
        for _ in 0..FLAG_RUNS_MAX {
            b.push(1, 1).unwrap();
            b.push(1, 0).unwrap();
        }
        assert!(!b.fits(1) && b.fits(0));
        assert_eq!(
            b.push(1, 1),
            Err(FlagRunsError::TooMany {
                runs: FLAG_RUNS_MAX + 1
            })
        );
        b.push(1, 2).unwrap_err();
        b.push(1, 0).unwrap();
        assert_eq!(b.runs().len(), FLAG_RUNS_MAX);
        let mut bytes = vec![];
        b.write(&mut bytes);
        assert_eq!(bytes.len(), b.len());
        FlagRuns::of(&bytes, 2 * FLAG_RUNS_MAX as u32 + 1).unwrap();
        b.clear();
        assert!(b.is_empty() && b.fits(3));
    }
}
