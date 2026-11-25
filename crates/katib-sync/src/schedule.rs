//! When to run a sync cycle (design.md section 5): after a quiet period
//! following a change, on a fetch timer, and with backoff while offline.
//! Times are passed in, so the rules are tested without waiting.

use std::time::{Duration, Instant};

use crate::Outcome;

/// Commit once the vault has been quiet this long.
pub const QUIET: Duration = Duration::from_secs(30);
/// Fetch at least this often.
pub const FETCH_EVERY: Duration = Duration::from_secs(60);
/// The longest wait between attempts while offline.
pub const MAX_BACKOFF: Duration = Duration::from_secs(600);
/// Rejected pushes retried at once; later ones back off as offline does.
pub const RETRIES: u32 = 3;

#[derive(Clone, Debug)]
pub struct Schedule {
    /// The vault changed since the last cycle began.
    changed: bool,
    last_change: Instant,
    next_fetch: Instant,
    backoff: Duration,
    /// Cycles in a row that ended in `Outcome::Retry`.
    retries: u32,
}

impl Schedule {
    /// The first cycle is due at `now`.
    pub fn new(now: Instant) -> Self {
        Self {
            changed: false,
            last_change: now,
            next_fetch: now,
            backoff: Duration::ZERO,
            retries: 0,
        }
    }

    pub fn changed(&self) -> bool {
        self.changed
    }

    /// Whether rejected pushes have stopped being retried at once.
    pub fn retries_exhausted(&self) -> bool {
        self.retries > RETRIES
    }

    pub fn note_change(&mut self, now: Instant) {
        self.changed = true;
        self.last_change = now;
    }

    pub fn fetch_soon(&mut self, now: Instant) {
        self.next_fetch = now;
    }

    /// Whether a cycle should start at `now`.
    pub fn due(&self, now: Instant) -> bool {
        let commit_due = self.changed && now.duration_since(self.last_change) >= QUIET;
        commit_due || now >= self.next_fetch
    }

    /// A cycle starts; changes after this belong to the next one.
    pub fn started(&mut self) {
        self.changed = false;
    }

    /// A cycle ended at `now` with `outcome`, or `None` if it failed.
    pub fn finished(&mut self, outcome: Option<&Outcome>, now: Instant) {
        let retry = matches!(outcome, Some(Outcome::Retry));
        self.retries = if retry { self.retries + 1 } else { 0 };
        self.next_fetch = match outcome {
            Some(Outcome::Retry) if self.retries <= RETRIES => now,
            // A push that keeps being rejected, as when fetch and push reach
            // different remotes, would otherwise loop without pause.
            Some(Outcome::Offline(_) | Outcome::Retry) => {
                self.backoff = (self.backoff * 2).clamp(FETCH_EVERY, MAX_BACKOFF);
                now + self.backoff
            }
            Some(_) => {
                self.backoff = Duration::ZERO;
                now + FETCH_EVERY
            }
            None => now + FETCH_EVERY,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: Duration = Duration::from_secs(1);

    #[test]
    fn a_change_waits_for_quiet_and_the_fetch_timer_runs_anyway() {
        let t0 = Instant::now();
        let mut s = Schedule::new(t0);
        assert!(s.due(t0));
        s.started();
        s.finished(Some(&Outcome::Synced), t0);
        assert!(!s.due(t0 + 10 * S));
        s.note_change(t0 + 10 * S);
        assert!(!s.due(t0 + 39 * S));
        assert!(s.due(t0 + 40 * S));
        s.started();
        assert!(!s.changed());
        s.finished(Some(&Outcome::Synced), t0 + 40 * S);
        assert!(!s.due(t0 + 99 * S));
        assert!(s.due(t0 + 100 * S));
        s.fetch_soon(t0 + 50 * S);
        assert!(s.due(t0 + 50 * S));
    }

    #[test]
    fn offline_backs_off_to_a_cap_and_a_sync_resets_it() {
        let t0 = Instant::now();
        let mut s = Schedule::new(t0);
        let offline = Outcome::Offline("down".into());
        let waits: Vec<u64> = (0..6)
            .map(|_| {
                s.finished(Some(&offline), t0);
                s.next_fetch.duration_since(t0).as_secs()
            })
            .collect();
        assert_eq!(waits, [60, 120, 240, 480, 600, 600]);
        s.finished(Some(&Outcome::Synced), t0);
        s.finished(Some(&offline), t0);
        assert_eq!(s.next_fetch, t0 + FETCH_EVERY);
    }

    #[test]
    fn a_push_rejected_every_time_backs_off_and_a_sync_resets_the_count() {
        let t0 = Instant::now();
        let mut s = Schedule::new(t0);
        let waits: Vec<u64> = (0..6)
            .map(|_| {
                s.finished(Some(&Outcome::Retry), t0);
                s.next_fetch.duration_since(t0).as_secs()
            })
            .collect();
        assert_eq!(waits, [0, 0, 0, 60, 120, 240]);
        assert!(s.retries_exhausted());
        s.finished(Some(&Outcome::Synced), t0);
        assert!(!s.retries_exhausted());
        s.finished(Some(&Outcome::Retry), t0);
        assert_eq!(s.next_fetch, t0);
        s.finished(Some(&Outcome::Offline("down".into())), t0);
        assert_eq!(s.next_fetch, t0 + FETCH_EVERY);
    }

    #[test]
    fn a_moved_remote_retries_at_once_and_an_error_waits_a_fetch_interval() {
        let t0 = Instant::now();
        let mut s = Schedule::new(t0);
        s.finished(Some(&Outcome::Retry), t0 + S);
        assert!(s.due(t0 + S));
        s.finished(None, t0);
        assert_eq!(s.next_fetch, t0 + FETCH_EVERY);
    }
}
