//! Merging streams of timed items by time: several chains, one per capture process, read as one.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// An item a [TimeMerge] gives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Merged<T> {
    /// The source it came from ([TimeMerge::add_source]).
    pub source: usize,
    pub time: i128,
    pub item: T,
    /// It came from a source that was left out of the bound while quiet ([TimeMerge::new]), and is older than an
    /// item given before it: the only way an item leaves time order.
    pub late: bool,
}

struct Source<T> {
    items: VecDeque<(i128, T)>,
    /// No item of this source will be older than this: its newest time, or a time it vouched for.
    mark: Option<i128>,
    /// When it last gave an item or a mark.
    heard: Instant,
}

/// Merges sources of timed items into one stream in time order, as far as the slowest source allows.
///
/// Each source gives its items in time order ([Self::push]), in one time base for all: the caller converts each
/// chain's ticks (clocks related by their maps). Within a chain, a track's items come in time order but tracks
/// don't, so a source is one track's stream (or a stream the caller put in order).
///
/// **The bound.** An item of time `t` is given only once every source has shown it holds nothing older: its newest
/// item or its mark ([Self::mark]) is at least `t`. So the stream is in time order, and an item waits for the slowest
/// source's newest time. A quiet source holds every other back; past `quiet_max` without an item or a mark it is
/// left out of the bound until it speaks again, and its items older than what was given come out marked
/// [Merged::late]. With `quiet_max` None a quiet source holds the merge as long as it is quiet: exact order, no
/// bound on the wait.
pub struct TimeMerge<T> {
    sources: Vec<Source<T>>,
    quiet_max: Option<Duration>,
    /// The time of the last item given.
    given: Option<i128>,
}

impl<T> TimeMerge<T> {
    /// A merge with no sources, which leaves a source out of the bound after `quiet_max` without a word.
    pub fn new(quiet_max: Option<Duration>) -> Self {
        Self {
            sources: Vec::new(),
            quiet_max,
            given: None,
        }
    }

    /// Add a source, heard first at `now`; its index.
    pub fn add_source(&mut self, now: Instant) -> usize {
        self.sources.push(Source {
            items: VecDeque::new(),
            mark: None,
            heard: now,
        });
        self.sources.len().saturating_sub(1)
    }

    /// Item `item` of `source` at `time`, heard at `now`. A source's items are given in the order pushed, so a
    /// source pushes them in time order.
    ///
    /// # Panics
    ///
    /// When `source` is no source of this merge.
    pub fn push(&mut self, source: usize, time: i128, item: T, now: Instant) {
        let s = self.source(source);
        s.mark = Some(s.mark.map_or(time, |m| m.max(time)));
        s.heard = now;
        s.items.push_back((time, item));
    }

    /// `source` vouches, at `now`, that it holds nothing older than `time` (a chain's progress with no item on this
    /// track, a heartbeat); its mark moves only forward.
    ///
    /// # Panics
    ///
    /// When `source` is no source of this merge.
    pub fn mark(&mut self, source: usize, time: i128, now: Instant) {
        let s = self.source(source);
        s.mark = Some(s.mark.map_or(time, |m| m.max(time)));
        s.heard = now;
    }

    fn source(&mut self, source: usize) -> &mut Source<T> {
        let n = self.sources.len();
        #[expect(
            clippy::panic,
            reason = "a source index the merge gave is the caller's precondition, stated under # Panics"
        )]
        self.sources
            .get_mut(source)
            .unwrap_or_else(|| panic!("source {source} of a merge of {n}"))
    }

    /// The time items are given up to now, at `now`: the oldest mark of the sources in the bound; None while one
    /// of them has no mark yet.
    pub fn bound(&self, now: Instant) -> Option<i128> {
        let mut bound: Option<i128> = None;
        for s in &self.sources {
            if self.left_out(s, now) {
                continue;
            }
            let m = s.mark?;
            bound = Some(bound.map_or(m, |b| b.min(m)));
        }
        bound
    }

    /// Whether `s` is left out of the bound at `now`: quiet past `quiet_max` with nothing waiting.
    fn left_out(&self, s: &Source<T>, now: Instant) -> bool {
        s.items.is_empty()
            && self
                .quiet_max
                .is_some_and(|q| now.saturating_duration_since(s.heard) > q)
    }

    /// The oldest item at or before the bound at `now`, when there is one.
    pub fn pop(&mut self, now: Instant) -> Option<Merged<T>> {
        let bound = self.bound(now);
        let mut best: Option<(usize, i128)> = None;
        for (i, s) in self.sources.iter().enumerate() {
            if let Some((t, _)) = s.items.front()
                && best.is_none_or(|(_, b)| *t < b)
            {
                best = Some((i, *t));
            }
        }
        let (i, t) = best?;
        if bound.is_none_or(|b| t > b) {
            return None;
        }
        let (time, item) = self.sources.get_mut(i)?.items.pop_front()?;
        let late = self.given.is_some_and(|g| time < g);
        self.given = Some(self.given.map_or(time, |g| g.max(time)));
        Some(Merged {
            source: i,
            time,
            item,
            late,
        })
    }

    /// Items waiting, of every source.
    pub fn waiting(&self) -> usize {
        self.sources.iter().map(|s| s.items.len()).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A source quiet past the wait bound is left out: the others' items flow, and its older item, when it comes,
    /// is marked late. Without a bound the quiet source holds the merge.
    #[test]
    fn a_quiet_source_is_left_out_after_the_bound_and_its_old_items_come_late() {
        let t0 = Instant::now();
        let later = t0 + Duration::from_secs(2);
        let mut m = TimeMerge::new(Some(Duration::from_secs(1)));
        let (a, b) = (m.add_source(t0), m.add_source(t0));
        m.push(a, 10, "a10", t0);
        m.push(a, 20, "a20", t0);
        assert_eq!(m.pop(t0), None, "b may still hold older items");
        assert_eq!(m.bound(later), Some(20));
        let got: Vec<_> = std::iter::from_fn(|| m.pop(later))
            .map(|x| x.item)
            .collect();
        assert_eq!(got, ["a10", "a20"]);
        m.push(b, 15, "b15", later);
        let late = m.pop(later).unwrap();
        assert_eq!((late.source, late.item, late.late), (b, "b15", true));

        let mut exact = TimeMerge::new(None);
        let (a, _b) = (exact.add_source(t0), exact.add_source(t0));
        exact.push(a, 10, "a10", t0);
        assert_eq!(exact.pop(t0 + Duration::from_secs(3600)), None);
    }

    /// A mark lets a source vouch for a time without an item: items up to it flow.
    #[test]
    fn a_mark_moves_the_bound_without_an_item() {
        let t0 = Instant::now();
        let mut m = TimeMerge::new(None);
        let (a, b) = (m.add_source(t0), m.add_source(t0));
        m.push(a, 10, 1, t0);
        m.push(a, 30, 2, t0);
        m.mark(b, 20, t0);
        assert_eq!(m.pop(t0).map(|x| x.item), Some(1));
        assert_eq!(m.pop(t0), None, "30 is past b's mark");
        m.mark(b, 5, t0);
        assert_eq!(m.bound(t0), Some(20), "a mark moves only forward");
        m.push(b, 25, 3, t0);
        assert_eq!(m.pop(t0).map(|x| x.item), Some(3));
        assert_eq!(m.waiting(), 1);
    }
}
