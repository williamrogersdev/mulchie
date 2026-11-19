//! Is something slower than it used to be? The statistics behind the
//! `build_slower` finding, kept free of builds so other measurements (a dev
//! server's startup, an endpoint's latency) can use them too.
//!
//! A series is a list of measurements of one thing, oldest first, only the
//! ones that count (for builds: the runs that succeeded). It is slower when
//! the last few measurements, at least [`Thresholds::slow_runs`] of them,
//! were each both more than [`Thresholds::ratio`] times the baseline and more
//! than [`Thresholds::min_increase`] above it. The baseline is the median of
//! up to [`Thresholds::baseline_runs`] measurements just before that slow
//! streak, so slow measurements never raise their own baseline. A median
//! shrugs off a single odd run on either side.
//!
//! Pure: no clock, no disk. Nothing acts on the answer; it is reported.

use serde::{Deserialize, Serialize};

/// A baseline needs at least this many measurements.
pub const MIN_BASELINE: usize = 3;

/// When a series counts as slower. The unit of `min_increase` is the
/// series' own (milliseconds for build timings).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Thresholds {
    /// The baseline is the median of up to this many measurements.
    pub baseline_runs: usize,
    /// At least this many of the latest measurements must each be slow.
    pub slow_runs: usize,
    /// A slow measurement is more than this many times the baseline...
    pub ratio: f64,
    /// ...and more than this much above it.
    pub min_increase: f64,
}

/// A series that got slower.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Slower {
    /// The median of the measurements before the slow streak.
    pub baseline: f64,
    /// How many measurements the baseline is the median of.
    pub baseline_runs: usize,
    /// The slow streak, oldest first (at least `slow_runs` long).
    pub recent: Vec<f64>,
    /// The median of `recent` divided by `baseline`.
    pub ratio: f64,
    /// Index in the series of the last measurement before the streak.
    pub last_normal: usize,
    /// Index in the series of the first slow measurement.
    pub first_slow: usize,
}

/// The median of `v` (the mean of the middle two for an even count).
/// `None` when `v` is empty.
pub fn median(v: &[f64]) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    let mut s: Vec<f64> = v.to_vec();
    s.sort_by(f64::total_cmp);
    let n = s.len();
    Some(if n % 2 == 1 {
        s[n / 2]
    } else {
        (s[n / 2 - 1] + s[n / 2]) / 2.0
    })
}

/// Whether `x` is slow against `baseline`.
pub fn is_slow(x: f64, baseline: f64, t: &Thresholds) -> bool {
    x > baseline * t.ratio && x - baseline > t.min_increase
}

/// The baseline for a streak starting at `start`: the median of up to
/// `baseline_runs` measurements before it, and how many there were.
fn baseline_before(series: &[f64], start: usize, t: &Thresholds) -> Option<(f64, usize)> {
    let from = start.saturating_sub(t.baseline_runs.max(1));
    let window = &series[from..start];
    if window.len() < MIN_BASELINE {
        return None;
    }
    median(window).map(|m| (m, window.len()))
}

/// Whether `series` (oldest first) ends slower than it was. `None` when it
/// does not, or when there are too few measurements to say.
pub fn slower(series: &[f64], t: &Thresholds) -> Option<Slower> {
    let k = t.slow_runs.max(1);
    let n = series.len();
    if n < k + MIN_BASELINE || series.iter().any(|x| !x.is_finite()) {
        return None;
    }
    let mut start = n - k;
    let (mut base, mut count) = baseline_before(series, start, t)?;
    if !series[start..].iter().all(|x| is_slow(*x, base, t)) {
        return None;
    }
    // Grow the streak backwards while the run before it is slow too, so a
    // slowdown that began earlier is dated from its first slow run.
    while start > MIN_BASELINE {
        let Some((b, c)) = baseline_before(series, start - 1, t) else {
            break;
        };
        if !series[start - 1..].iter().all(|x| is_slow(*x, b, t)) {
            break;
        }
        start -= 1;
        base = b;
        count = c;
    }
    let recent = series[start..].to_vec();
    let ratio = median(&recent).unwrap_or(base) / base.max(f64::MIN_POSITIVE);
    Some(Slower {
        baseline: base,
        baseline_runs: count,
        recent,
        ratio,
        last_normal: start - 1,
        first_slow: start,
    })
}

/// A duration in milliseconds, short: `850ms`, `45s`, `2m`, `4m10s`, `1h5m`.
pub fn human_ms(ms: u64) -> String {
    if ms < 1000 {
        return format!("{ms}ms");
    }
    let s = (ms + 500) / 1000;
    let (h, m, s) = (s / 3600, (s % 3600) / 60, s % 60);
    match (h, m, s) {
        (0, 0, s) => format!("{s}s"),
        (0, m, 0) => format!("{m}m"),
        (0, m, s) => format!("{m}m{s}s"),
        (h, 0, _) => format!("{h}h"),
        (h, m, _) => format!("{h}h{m}m"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t() -> Thresholds {
        Thresholds {
            baseline_runs: 10,
            slow_runs: 3,
            ratio: 1.5,
            min_increase: 30_000.0,
        }
    }

    fn secs(v: &[u64]) -> Vec<f64> {
        v.iter().map(|s| (*s * 1000) as f64).collect()
    }

    #[test]
    fn median_of_odd_even_and_empty() {
        assert_eq!(median(&[]), None);
        assert_eq!(median(&[3.0, 1.0, 2.0]), Some(2.0));
        assert_eq!(median(&[4.0, 1.0, 3.0, 2.0]), Some(2.5));
    }

    #[test]
    fn three_slow_runs_after_a_steady_baseline() {
        let s = secs(&[120, 118, 125, 121, 119, 250, 260, 255]);
        let r = slower(&s, &t()).expect("slower");
        assert_eq!(r.baseline, 120_000.0);
        assert_eq!(r.baseline_runs, 5);
        assert_eq!(r.recent, secs(&[250, 260, 255]));
        assert_eq!((r.last_normal, r.first_slow), (4, 5));
        assert!((r.ratio - 255.0 / 120.0).abs() < 1e-9);
    }

    #[test]
    fn too_few_runs_say_nothing() {
        assert!(slower(&secs(&[120, 120, 250, 260, 255]), &t()).is_none());
        assert!(slower(&secs(&[120, 120, 120, 250, 260]), &t()).is_none());
        assert!(slower(&[], &t()).is_none());
    }

    #[test]
    fn one_normal_run_in_the_last_k_clears_it() {
        let s = secs(&[120, 120, 120, 120, 250, 121, 255]);
        assert!(slower(&s, &t()).is_none());
    }

    #[test]
    fn ratio_without_the_absolute_minimum_is_not_slower() {
        // 2× slower, but only 10 s more: noise for a 10 s build.
        let s = secs(&[10, 10, 10, 10, 20, 20, 20]);
        assert!(slower(&s, &t()).is_none());
        // 31 s more but only 1.2×: not slower either.
        let s = secs(&[150, 150, 150, 150, 181, 181, 181]);
        assert!(slower(&s, &t()).is_none());
    }

    #[test]
    fn exactly_at_the_thresholds_is_not_slower() {
        // 1.5× exactly.
        let s = secs(&[100, 100, 100, 150, 150, 150]);
        assert!(slower(&s, &t()).is_none());
        // 30 s more exactly, well over the ratio.
        let s = secs(&[20, 20, 20, 50, 50, 50]);
        assert!(slower(&s, &t()).is_none());
        let s = secs(&[20, 20, 20, 51, 51, 51]);
        assert!(slower(&s, &t()).is_some());
    }

    #[test]
    fn a_longer_streak_is_dated_from_its_first_slow_run() {
        let s = secs(&[60, 61, 59, 60, 62, 200, 210, 205, 215, 220]);
        let r = slower(&s, &t()).unwrap();
        assert_eq!(r.first_slow, 5);
        assert_eq!(r.last_normal, 4);
        assert_eq!(r.baseline, 60_000.0);
        assert_eq!(r.recent.len(), 5);
    }

    #[test]
    fn the_baseline_uses_only_the_last_n_runs_before_the_streak() {
        let mut v = vec![1000; 20];
        v.extend([60; 10]);
        v.extend([200, 200, 200]);
        let r = slower(&secs(&v), &t()).unwrap();
        assert_eq!(r.baseline, 60_000.0);
        assert_eq!(r.baseline_runs, 10);
    }

    #[test]
    fn one_odd_run_in_the_baseline_does_not_move_it() {
        let s = secs(&[60, 60, 900, 60, 60, 200, 200, 200]);
        let r = slower(&s, &t()).unwrap();
        assert_eq!(r.baseline, 60_000.0);
    }

    #[test]
    fn non_finite_values_say_nothing() {
        let mut s = secs(&[60, 60, 60, 200, 200, 200]);
        s[1] = f64::NAN;
        assert!(slower(&s, &t()).is_none());
    }

    #[test]
    fn human_durations() {
        assert_eq!(human_ms(850), "850ms");
        assert_eq!(human_ms(45_000), "45s");
        assert_eq!(human_ms(120_000), "2m");
        assert_eq!(human_ms(250_000), "4m10s");
        assert_eq!(human_ms(3_900_000), "1h5m");
        assert_eq!(human_ms(7_200_000), "2h");
    }
}
