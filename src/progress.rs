/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

//! Optional `--progress` reporting for long import and export runs.
//!
//! A single process-wide reporter, like the logger: it owns one line of the
//! terminal and is written to from whichever thread is accounting results.
//! Disabled by default, in which case every entry point is a no-op.
//!
//! Phases nest: a long stage inside a type (such as scanning the target
//! server before transferring) starts its own phase, and finishing it
//! resumes the enclosing one. Time spent in a nested phase does not count
//! against the parent's rate or ETA.
//!
//! On a terminal a background ticker repaints the current line even when no
//! progress is being made, so the elapsed clock keeps moving and a stalled
//! stage is visibly alive rather than frozen.

use std::io::{IsTerminal, Write};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Minimum gap between redraws, so a fast run does not spend its time in
/// `write!` and a piped run does not produce thousands of lines.
const TTY_INTERVAL: Duration = Duration::from_millis(100);
const PIPE_INTERVAL: Duration = Duration::from_secs(5);

static PROGRESS: OnceLock<Progress> = OnceLock::new();

pub fn init(enabled: bool) {
    let _ = PROGRESS.set(Progress::new(enabled));
    if let Some(p) = get()
        && p.tty
    {
        std::thread::spawn(|| {
            loop {
                std::thread::sleep(TTY_INTERVAL);
                if let Some(p) = get() {
                    p.tick();
                }
            }
        });
    }
}

fn get() -> Option<&'static Progress> {
    PROGRESS.get().filter(|p| p.enabled)
}

/// Begin a phase. `total` is the number of items expected, when known.
/// While an earlier phase is still open the new one nests inside it and
/// `finish` returns to the enclosing phase.
pub fn start(label: &str, total: Option<u64>) {
    if let Some(p) = get() {
        p.start(label, total);
    }
}

/// Record `n` completed items in the current (innermost) phase.
pub fn advance(n: u64) {
    if let Some(p) = get() {
        p.advance(n);
    }
}

/// Set the expected total of the current phase once it becomes known.
pub fn set_total(total: u64) {
    if let Some(p) = get() {
        p.set_total(total);
    }
}

/// End the current phase, leaving a final line in the scrollback. If a phase
/// was nested, the enclosing phase becomes current again.
pub fn finish() {
    if let Some(p) = get() {
        p.finish();
    }
}

struct Progress {
    enabled: bool,
    tty: bool,
    state: Mutex<Vec<Phase>>,
}

struct Phase {
    label: String,
    total: Option<u64>,
    done: u64,
    started: Instant,
    last_render: Instant,
}

impl Progress {
    fn new(enabled: bool) -> Progress {
        Progress {
            enabled,
            tty: std::io::stderr().is_terminal(),
            state: Mutex::new(Vec::new()),
        }
    }

    fn interval(&self) -> Duration {
        if self.tty {
            TTY_INTERVAL
        } else {
            PIPE_INTERVAL
        }
    }

    fn start(&self, label: &str, total: Option<u64>) {
        let now = Instant::now();
        let mut guard = lock(&self.state);
        guard.push(Phase {
            label: label.to_owned(),
            total,
            done: 0,
            started: now,
            last_render: now,
        });
        if let Some(phase) = guard.last_mut() {
            self.render(phase, false);
        }
    }

    fn advance(&self, n: u64) {
        let mut guard = lock(&self.state);
        let Some(phase) = guard.last_mut() else {
            return;
        };
        phase.done += n;
        if phase.last_render.elapsed() >= self.interval() {
            self.render(phase, false);
        }
    }

    fn set_total(&self, total: u64) {
        let mut guard = lock(&self.state);
        if let Some(phase) = guard.last_mut() {
            phase.total = Some(total);
        }
    }

    fn finish(&self) {
        let mut guard = lock(&self.state);
        let Some(mut phase) = guard.pop() else {
            return;
        };
        self.render(&mut phase, true);
        // The parent was paused for the whole life of the nested phase; shift
        // its clock forward so its rate and ETA reflect only its own work.
        if let Some(parent) = guard.last_mut() {
            parent.started += phase.started.elapsed();
            if self.tty {
                self.render(parent, false);
            }
        }
    }

    /// Periodic repaint from the ticker thread, so the elapsed clock and the
    /// rate stay live even while no items complete.
    fn tick(&self) {
        if !self.tty {
            return;
        }
        let mut guard = lock(&self.state);
        if let Some(phase) = guard.last_mut()
            && phase.last_render.elapsed() >= self.interval()
        {
            self.render(phase, false);
        }
    }

    fn render(&self, phase: &mut Phase, final_line: bool) {
        phase.last_render = Instant::now();
        // A piped run only reports on the interval or at the end; the
        // intermediate redraws exist for a terminal that can overwrite them.
        if !self.tty && !final_line && phase.done == 0 {
            return;
        }
        let line = format_line(phase);
        let mut err = std::io::stderr().lock();
        if self.tty {
            let _ = write!(err, "\r\x1b[2K{line}");
            if final_line {
                let _ = writeln!(err);
            }
        } else {
            let _ = writeln!(err, "{line}");
        }
        let _ = err.flush();
    }
}

fn format_line(phase: &Phase) -> String {
    let elapsed = phase.started.elapsed();
    let secs = elapsed.as_secs_f64();
    let rate = if secs > 0.0 {
        phase.done as f64 / secs
    } else {
        0.0
    };
    match phase.total {
        Some(total) if total > 0 => {
            let pct = (phase.done as f64 / total as f64 * 100.0).min(100.0);
            format!(
                "{}: {}/{} ({pct:.0}%) {rate:.1}/s eta {} [{}]",
                phase.label,
                phase.done,
                total,
                eta(phase.done, total, rate),
                hms(elapsed.as_secs()),
            )
        }
        _ => format!(
            "{}: {} ({rate:.1}/s) [{}]",
            phase.label,
            phase.done,
            hms(elapsed.as_secs())
        ),
    }
}

fn eta(done: u64, total: u64, rate: f64) -> String {
    if rate <= 0.0 || done >= total {
        return "--:--".to_owned();
    }
    let secs = ((total - done) as f64 / rate).round() as u64;
    hms(secs)
}

fn hms(secs: u64) -> String {
    if secs >= 3600 {
        format!("{}:{:02}:{:02}", secs / 3600, (secs % 3600) / 60, secs % 60)
    } else {
        format!("{:02}:{:02}", secs / 60, secs % 60)
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match m.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn phase(done: u64, total: Option<u64>) -> Phase {
        Phase {
            label: "Email".to_owned(),
            total,
            done,
            started: Instant::now() - Duration::from_secs(10),
            last_render: Instant::now(),
        }
    }

    #[test]
    fn line_reports_percentage_rate_eta_and_elapsed() {
        let line = format_line(&phase(100, Some(400)));
        assert!(line.starts_with("Email: 100/400 (25%)"), "{line}");
        assert!(line.contains("10.0/s"), "{line}");
        assert!(line.contains("eta 00:30"), "{line}");
        assert!(line.contains("[00:10]"), "{line}");
    }

    #[test]
    fn line_without_total_omits_percentage() {
        let line = format_line(&phase(42, None));
        assert_eq!(line, "Email: 42 (4.2/s) [00:10]");
    }

    #[test]
    fn eta_is_unknown_at_zero_rate_or_when_complete() {
        assert_eq!(eta(0, 100, 0.0), "--:--");
        assert_eq!(eta(100, 100, 5.0), "--:--");
    }

    #[test]
    fn long_durations_gain_an_hours_field() {
        assert_eq!(hms(59), "00:59");
        assert_eq!(hms(3599), "59:59");
        assert_eq!(hms(3600), "1:00:00");
        assert_eq!(hms(3700), "1:01:40");
    }

    #[test]
    fn percentage_never_exceeds_one_hundred() {
        let line = format_line(&phase(500, Some(400)));
        assert!(line.contains("(100%)"), "{line}");
    }

    #[test]
    fn disabled_reporter_is_inert() {
        let p = Progress::new(false);
        assert!(!p.enabled);
        p.start("Email", Some(10));
        p.advance(1);
        p.finish();
    }

    #[test]
    fn nested_phase_resumes_the_enclosing_one() {
        let p = Progress::new(true);
        p.start("Email", Some(100));
        p.advance(7);
        p.start("Email: scan target", Some(50));
        p.advance(50);
        {
            let guard = lock(&p.state);
            assert_eq!(guard.len(), 2);
            assert_eq!(guard.last().unwrap().done, 50);
        }
        p.finish();
        {
            let guard = lock(&p.state);
            assert_eq!(guard.len(), 1);
            let top = guard.last().unwrap();
            assert_eq!(top.label, "Email");
            assert_eq!(top.done, 7);
            assert_eq!(top.total, Some(100));
        }
        p.finish();
        assert!(lock(&p.state).is_empty());
        // Finishing with nothing open must not panic.
        p.finish();
    }

    #[test]
    fn set_total_upgrades_an_indeterminate_phase() {
        let p = Progress::new(true);
        p.start("Email: scan target", None);
        p.set_total(400);
        assert_eq!(lock(&p.state).last().unwrap().total, Some(400));
        p.finish();
    }
}
