//! A turn: from a prompt sent to the agent until it goes quiet. Agents redraw
//! a spinner or a timer the whole time they work, so QUIET without output
//! means the answer is in or the agent is waiting on you.

use std::time::{Duration, Instant};

const QUIET: Duration = Duration::from_secs(3);

#[derive(Default)]
pub struct Turn {
    started: Option<Instant>,
    last_output: Option<Instant>,
}

impl Turn {
    /// A prompt sent while one is running joins it.
    pub fn start(&mut self, now: Instant) {
        self.started.get_or_insert(now);
    }

    pub fn output(&mut self, now: Instant) {
        self.last_output = Some(now);
    }

    /// How long the running turn has taken; None once it has gone quiet.
    pub fn working(&mut self, now: Instant) -> Option<Duration> {
        let started = self.started?;
        let heard = self.last_output.map_or(started, |t| t.max(started));
        if now.duration_since(heard) >= QUIET {
            self.started = None;
            return None;
        }
        Some(now.duration_since(started))
    }
}

/// `12s`, `1m12s`, `2h05m`.
pub fn elapsed(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m{:02}s", s / 60, s % 60),
        _ => format!("{}h{:02}m", s / 3600, s % 3600 / 60),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    #[test]
    fn idle_until_a_prompt() {
        let t0 = Instant::now();
        let mut t = Turn::default();
        t.output(t0);
        assert_eq!(t.working(at(t0, 100)), None);
    }

    #[test]
    fn output_keeps_it_working_and_quiet_ends_it() {
        let t0 = Instant::now();
        let mut t = Turn::default();
        t.start(t0);
        assert_eq!(t.working(at(t0, 2_900)), Some(Duration::from_millis(2_900)));
        t.output(at(t0, 2_500));
        assert_eq!(t.working(at(t0, 5_400)), Some(Duration::from_millis(5_400)));
        assert_eq!(t.working(at(t0, 5_500)), None);
        assert_eq!(t.working(at(t0, 5_000)), None, "ended stays ended");
    }

    #[test]
    fn output_from_before_the_prompt_does_not_end_it() {
        let t0 = Instant::now();
        let mut t = Turn::default();
        t.output(t0);
        t.start(at(t0, 10_000));
        assert!(t.working(at(t0, 11_000)).is_some());
    }

    #[test]
    fn a_second_prompt_joins_the_running_turn() {
        let t0 = Instant::now();
        let mut t = Turn::default();
        t.start(t0);
        t.output(at(t0, 1_000));
        t.start(at(t0, 2_000));
        assert_eq!(t.working(at(t0, 2_500)), Some(Duration::from_millis(2_500)));
    }

    #[test]
    fn elapsed_forms() {
        let f = |s| elapsed(Duration::from_secs(s));
        assert_eq!([f(0), f(59), f(60), f(72), f(3599), f(3600), f(7500)], ["0s", "59s", "1m00s", "1m12s", "59m59s", "1h00m", "2h05m"]);
    }
}
