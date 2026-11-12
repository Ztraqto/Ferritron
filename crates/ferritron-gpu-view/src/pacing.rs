//! Frame pacing: when an editor is drawn, and the clock that decides it.
//!
//! The rules (see [`GpuEditor::next_frame`](crate::GpuEditor::next_frame)):
//!
//! * An editor that asks for nothing (`next_frame` is `None`) and has nothing that changed (no
//!   parameter, input, redraw request, stream values, instance change) is **not drawn at all**.
//! * Something changed: one frame at the next check, but never more often than the display
//!   refreshes.
//! * `next_frame` is `Some(interval)`: frames are scheduled `interval` apart on a fixed grid
//!   (`due += interval`), not "interval after the previous frame was finished", so the average
//!   rate is exact and a late timer tick does not stretch every following frame. `Duration::ZERO`
//!   means once per display refresh, and no interval is shorter than the refresh period.
//!
//! The decision is a plain function of the clock, so tests drive it with a [`ManualClock`].

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// A source of time that tests can replace. Editors read it through [`Ui::now`](crate::Ui::now)
/// instead of `Instant::now()`, so their time-dependent logic (animations, "quiet for 2 s",
/// thinning to 30 Hz) can be tested without waiting.
pub trait Clock: Send + Sync {
    /// Time since an arbitrary fixed starting point; never goes back.
    fn now(&self) -> Duration;
}

/// The real clock.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Duration {
        static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
        START.get_or_init(Instant::now).elapsed()
    }
}

/// A clock that only moves when told to (tests). Clones share the time.
#[derive(Clone, Debug, Default)]
pub struct ManualClock(Arc<AtomicU64>);

impl ManualClock {
    pub fn new() -> ManualClock {
        ManualClock::default()
    }

    pub fn advance(&self, by: Duration) {
        self.0.fetch_add(by.as_nanos() as u64, Ordering::AcqRel);
    }

    pub fn set(&self, to: Duration) {
        self.0.store(to.as_nanos() as u64, Ordering::Release);
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Duration {
        Duration::from_nanos(self.0.load(Ordering::Acquire))
    }
}

/// How often the view checks when nothing is scheduled (parameters changed by the host or by
/// automation are noticed by polling).
pub(crate) const IDLE_CHECK: Duration = Duration::from_millis(16);

/// What [`Pacer::tick`] decides.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Tick {
    /// Draw a frame now.
    pub render: bool,
    /// When to check again at the latest (the slow poll for changes made outside the editor).
    pub next_check: Duration,
    /// When the next frame is due, if one is scheduled or waiting for the display: the view wakes
    /// up exactly then (a timer thread on Windows), instead of at the next coarse timer tick.
    pub due_in: Option<Duration>,
}

/// Decides when frames are drawn.
pub(crate) struct Pacer {
    /// When the last frame was drawn.
    last: Option<Duration>,
    /// When the next scheduled frame is due.
    due: Option<Duration>,
    /// One display refresh.
    refresh: Duration,
}

impl Pacer {
    pub fn new(refresh: Duration) -> Pacer {
        Pacer { last: None, due: None, refresh: refresh.max(Duration::from_millis(1)) }
    }

    pub fn set_refresh(&mut self, refresh: Duration) {
        self.refresh = refresh.max(Duration::from_millis(1));
    }

    pub fn refresh(&self) -> Duration {
        self.refresh
    }

    /// The time of the last frame (zero-based by the clock), if any.
    pub fn last(&self) -> Option<Duration> {
        self.last
    }

    /// A check at `now`. `dirty`: something changed and wants a frame. `want`: the editor's
    /// `next_frame()`.
    pub fn tick(&mut self, now: Duration, dirty: bool, want: Option<Duration>) -> Tick {
        let period = self.refresh;
        let interval = want.map(|d| d.max(period));
        match interval {
            None => self.due = None,
            Some(d) => {
                if self.due.is_none() {
                    self.due = Some(self.last.map_or(now, |l| l + d));
                }
            }
        }
        // A change draws at the next check, but no faster than the display shows frames.
        let gap = period.mul_f64(0.9);
        let dirty_ready = dirty && self.last.is_none_or(|l| now >= l + gap);
        let due_hit = self.due.is_some_and(|t| now >= t);
        let render = due_hit || dirty_ready;
        if render {
            if due_hit {
                let d = interval.expect("a due time exists only while the editor asks for frames");
                let prev = self.due.unwrap_or(now);
                // On the grid while the timer keeps up; after a long stall start a new grid.
                self.due = Some(if now.saturating_sub(prev) < d { prev + d } else { now + d });
            }
            self.last = Some(now);
        }
        let mut next = IDLE_CHECK;
        let mut due_in = self.due.map(|t| t.saturating_sub(now));
        if let Some(d) = due_in {
            next = next.min(d);
        }
        if dirty && !render {
            // throttled: come back when the display could show another frame
            if let Some(l) = self.last {
                let wait = (l + gap).saturating_sub(now);
                next = next.min(wait);
                due_in = Some(due_in.map_or(wait, |d| d.min(wait)));
            }
        }
        let one_ms = Duration::from_millis(1);
        Tick { render, next_check: next.max(one_ms), due_in: due_in.map(|d| d.max(one_ms)) }
    }

    /// The time since the previous frame, for a frame drawn at `now` (zero for the first).
    pub fn delta(&self, previous: Option<Duration>, now: Duration) -> Duration {
        previous.map_or(Duration::ZERO, |p| now.saturating_sub(p))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: fn(u64) -> Duration = Duration::from_millis;

    /// Runs `pacer` for `total` on a simulated timer that fires every `step` (with the next-check
    /// request honoured as an upper bound), returning the render times.
    fn run(pacer: &mut Pacer, clock: &ManualClock, total: Duration, step: Duration, mut dirty: impl FnMut(Duration) -> bool, want: Option<Duration>) -> Vec<Duration> {
        let mut frames = Vec::new();
        let end = clock.now() + total;
        let mut pending_dirty = false;
        while clock.now() < end {
            let now = clock.now();
            pending_dirty |= dirty(now);
            let t = pacer.tick(now, pending_dirty, want);
            if t.render {
                frames.push(now);
                pending_dirty = false;
            }
            clock.advance(step.min(t.next_check));
        }
        frames
    }

    #[test]
    fn a_still_editor_is_never_drawn() {
        let clock = ManualClock::new();
        let mut p = Pacer::new(Duration::from_secs_f64(1.0 / 60.0));
        let frames = run(&mut p, &clock, Duration::from_secs(60), MS(16), |_| false, None);
        assert!(frames.is_empty(), "{} frames while nothing changed", frames.len());
    }

    #[test]
    fn the_first_change_draws_once_and_then_nothing_until_the_next_change() {
        let clock = ManualClock::new();
        let mut p = Pacer::new(Duration::from_secs_f64(1.0 / 60.0));
        let mut fired = false;
        let frames = run(
            &mut p,
            &clock,
            Duration::from_secs(5),
            MS(16),
            |now| {
                let go = now >= MS(1000) && !fired;
                fired |= go;
                go
            },
            None,
        );
        assert_eq!(frames.len(), 1);
        assert!(frames[0] >= MS(1000) && frames[0] < MS(1020));
    }

    #[test]
    fn a_burst_of_changes_is_limited_to_the_display_rate() {
        let clock = ManualClock::new();
        let mut p = Pacer::new(Duration::from_secs_f64(1.0 / 60.0));
        // something changes at every 1 ms check for one second
        let frames = run(&mut p, &clock, Duration::from_secs(1), MS(1), |_| true, None);
        assert!((55..=67).contains(&frames.len()), "{} frames in a second at 60 Hz (a change may draw a refresh early: 90% of the period)", frames.len());
        let min_gap = frames.windows(2).map(|w| w[1] - w[0]).min().unwrap();
        assert!(min_gap >= MS(14), "frames {min_gap:?} apart");
    }

    #[test]
    fn thirty_hertz_frames_are_evenly_spaced_even_with_a_coarse_timer() {
        // E9: the clock is replaced, the timer is the Windows-like 16 ms tick.
        let clock = ManualClock::new();
        let mut p = Pacer::new(Duration::from_secs_f64(1.0 / 60.0));
        let want = Some(Duration::from_secs_f64(1.0 / 30.0));
        let frames = run(&mut p, &clock, Duration::from_secs(10), MS(16), |_| false, want);
        // 30 Hz for 10 s: 300 frames, none missing and none extra
        assert!((298..=301).contains(&frames.len()), "{} frames", frames.len());
        let gaps: Vec<Duration> = frames.windows(2).map(|w| w[1] - w[0]).collect();
        // the timer ticks every 16 ms, so a frame lands on a 32 or 48 ms boundary: never
        // two frames closer than one timer tick, never a gap above three ticks
        assert!(gaps.iter().all(|g| *g >= MS(16) && *g <= MS(48)), "{gaps:?}");
        let mean = (frames.last().unwrap().as_secs_f64() - frames[0].as_secs_f64()) / (frames.len() - 1) as f64;
        assert!((mean - 1.0 / 30.0).abs() < 0.0005, "mean interval {mean}");
    }

    #[test]
    fn thirty_hertz_is_exact_when_the_timer_honours_the_requested_wait() {
        let clock = ManualClock::new();
        let mut p = Pacer::new(Duration::from_secs_f64(1.0 / 60.0));
        let want = Some(Duration::from_secs_f64(1.0 / 30.0));
        // an ideal timer: wakes exactly when asked
        let frames = run(&mut p, &clock, Duration::from_secs(10), Duration::from_secs(1), |_| false, want);
        let gaps: Vec<f64> = frames.windows(2).map(|w| (w[1] - w[0]).as_secs_f64()).collect();
        assert!(gaps.iter().all(|g| (g - 1.0 / 30.0).abs() < 1e-6), "{:?}", &gaps[..5]);
    }

    #[test]
    fn a_late_timer_tick_does_not_shift_the_grid() {
        let clock = ManualClock::new();
        let mut p = Pacer::new(Duration::from_secs_f64(1.0 / 60.0));
        let want = Some(MS(100));
        assert!(p.tick(clock.now(), false, want).render, "the first frame at once");
        clock.set(MS(100));
        assert!(p.tick(clock.now(), false, want).render);
        // the third frame is due at 200 ms, but the timer only fires at 230 ms
        clock.set(MS(230));
        assert!(p.tick(clock.now(), false, want).render);
        // the grid is kept: the next one is due at 300 ms
        clock.set(MS(299));
        assert!(!p.tick(clock.now(), false, want).render);
        clock.set(MS(300));
        assert!(p.tick(clock.now(), false, want).render);
        // after a long stall (a minute) a new grid starts instead of a burst of catch-up frames
        clock.set(MS(60_300));
        assert!(p.tick(clock.now(), false, want).render);
        clock.set(MS(60_301));
        assert!(!p.tick(clock.now(), false, want).render);
    }

    #[test]
    fn zero_means_once_per_display_refresh_and_nothing_is_faster_than_the_display() {
        for (hz, expect) in [(60.0, 600), (144.0, 1440), (30.0, 300)] {
            let clock = ManualClock::new();
            let mut p = Pacer::new(Duration::from_secs_f64(1.0 / hz));
            let frames = run(&mut p, &clock, Duration::from_secs(10), Duration::from_secs(1), |_| false, Some(Duration::ZERO));
            assert!((frames.len() as i64 - expect as i64).abs() <= 2, "{hz} Hz: {} frames", frames.len());
        }
        // asking for 240 Hz on a 60 Hz display gives 60 Hz
        let clock = ManualClock::new();
        let mut p = Pacer::new(Duration::from_secs_f64(1.0 / 60.0));
        let frames = run(&mut p, &clock, Duration::from_secs(10), Duration::from_secs(1), |_| false, Some(Duration::from_micros(4166)));
        assert!((frames.len() as i64 - 600).abs() <= 2, "{} frames", frames.len());
    }

    #[test]
    fn stopping_the_animation_stops_the_frames() {
        let clock = ManualClock::new();
        let mut p = Pacer::new(Duration::from_secs_f64(1.0 / 60.0));
        let a = run(&mut p, &clock, Duration::from_secs(1), MS(16), |_| false, Some(MS(33)));
        assert!(a.len() > 20);
        let b = run(&mut p, &clock, Duration::from_secs(5), MS(16), |_| false, None);
        assert!(b.is_empty(), "{} frames after the editor stopped asking", b.len());
    }

    #[test]
    fn the_next_check_is_as_late_as_allowed() {
        let clock = ManualClock::new();
        let mut p = Pacer::new(Duration::from_secs_f64(1.0 / 60.0));
        let t = p.tick(clock.now(), false, None);
        assert_eq!((t.render, t.next_check, t.due_in), (false, IDLE_CHECK, None), "idle: the slow poll for parameter changes, nothing due");
        let t = p.tick(clock.now(), false, Some(MS(100)));
        assert!(t.render);
        assert_eq!(t.next_check, IDLE_CHECK.min(MS(100)));
        assert_eq!(t.due_in, Some(MS(100)), "the exact time to the next frame, for the precise timer");
        clock.advance(MS(10));
        let t = p.tick(clock.now(), false, Some(MS(100)));
        assert!(!t.render);
        assert_eq!(t.next_check, IDLE_CHECK, "never more than the idle poll");
        assert_eq!(t.due_in, Some(MS(90)));
    }

    #[test]
    fn the_manual_clock_is_shared_and_system_clock_moves() {
        let a = ManualClock::new();
        let b = a.clone();
        a.advance(MS(5));
        assert_eq!(b.now(), MS(5));
        b.set(Duration::from_secs(2));
        assert_eq!(a.now(), Duration::from_secs(2));
        let s = SystemClock;
        let t0 = s.now();
        std::thread::sleep(MS(2));
        assert!(s.now() > t0);
    }
}
