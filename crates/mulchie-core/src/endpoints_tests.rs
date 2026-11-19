use super::*;
use crate::otel::Resource;

const W: u64 = 5 * 60 * 1000;

fn t() -> EndpointThresholds {
    EndpointThresholds::of(&PerfConfig::default(), &OtelConfig::default())
}

fn stats(count: u64, errors: u64, p95: f64) -> Stats {
    Stats {
        count,
        errors,
        p95_ms: p95,
        ..Stats::default()
    }
}

fn win(i: u64, count: u64, errors: u64, p95: f64, rev: Option<&str>) -> WindowSummary {
    WindowSummary {
        start_ms: i * W,
        end_ms: (i + 1) * W,
        stats: stats(count, errors, p95),
        resource: Resource {
            vcs_revision: rev.map(String::from),
            ..Resource::default()
        },
    }
}

#[test]
fn thresholds_come_from_perf_and_otel() {
    let t = t();
    assert_eq!(t.ratio, 1.5);
    assert_eq!(t.min_slowdown_ms, 100.0);
    assert_eq!(t.min_requests, 20);
}

#[test]
fn the_baseline_is_the_median_of_busy_windows_before_the_recent_range() {
    let ws = vec![
        win(0, 50, 0, 100.0, Some("aaa")),
        win(1, 5, 0, 900.0, None), // too few requests
        win(2, 50, 5, 110.0, None),
        win(3, 50, 0, 90.0, Some("bbb")),
        win(4, 50, 0, 500.0, Some("ccc")), // recent: not in the baseline
    ];
    let b = baseline(&ws, 4 * W, &t()).unwrap();
    assert_eq!(b.p95_ms, 100.0);
    assert_eq!(b.windows, 3);
    assert_eq!(b.error_rate, 0.0);
    assert_eq!((b.from_ms, b.to_ms), (0, 4 * W));
    assert_eq!(b.revision.as_deref(), Some("bbb"));
    // Two busy windows are not enough.
    assert!(baseline(&ws[..3], 4 * W, &t()).is_none());
}

#[test]
fn slower_needs_the_ratio_the_minimum_and_enough_requests() {
    let b = Baseline {
        p95_ms: 100.0,
        error_rate: 0.0,
        windows: 10,
        from_ms: 0,
        to_ms: 0,
        revision: None,
    };
    assert!(is_slower(&stats(100, 0, 420.0), &b, &t()));
    // 1.5× exactly is not more than 1.5×.
    assert!(!is_slower(&stats(100, 0, 150.0), &b, &t()));
    // 3× but only 40 ms more: noise for a fast endpoint.
    let fast = Baseline {
        p95_ms: 20.0,
        ..b.clone()
    };
    assert!(!is_slower(&stats(100, 0, 60.0), &fast, &t()));
    // Too few recent requests.
    assert!(!is_slower(&stats(19, 0, 420.0), &b, &t()));
}

#[test]
fn more_errors_needs_the_ratio_and_five_points() {
    let b = Baseline {
        p95_ms: 100.0,
        error_rate: 0.01,
        windows: 10,
        from_ms: 0,
        to_ms: 0,
        revision: None,
    };
    assert!(has_more_errors(&stats(100, 12, 100.0), &b, &t()));
    // 4 points up: under the floor.
    assert!(!has_more_errors(&stats(100, 5, 100.0), &b, &t()));
    let zero = Baseline {
        error_rate: 0.0,
        ..b.clone()
    };
    assert!(has_more_errors(&stats(100, 6, 100.0), &zero, &t()));
    assert!(!has_more_errors(&stats(10, 6, 100.0), &zero, &t()));
}

fn ev() -> EndpointEvidence {
    EndpointEvidence {
        service: "shop-web".into(),
        operation: "GET /api/orders".into(),
        recent_minutes: 15,
        recent_count: 240,
        recent_errors: 0,
        recent_p95_ms: 420.0,
        recent_error_rate: 0.0,
        baseline_p95_ms: 100.0,
        baseline_error_rate: 0.0,
        baseline_windows: 12,
        ratio: 4.2,
        attribution: Some(Attribution {
            workspace: Some("/w/shop".into()),
            via: "pid".into(),
            ..Attribution::default()
        }),
        ..EndpointEvidence::default()
    }
}

#[test]
fn a_slower_finding_names_the_numbers_and_is_stable() {
    let f = finding(FindingKind::EndpointSlower, ev(), &t(), 90, 100);
    assert_eq!(
        f.title,
        "`GET /api/orders` in `shop-web` is 4.2× slower at p95 (420 ms vs 100 ms)"
    );
    assert_eq!(f.category, FindingCategory::Performance);
    assert_eq!(f.severity, Severity::Medium);
    assert!(f.id.starts_with("endpoint_slower:"));
    assert_eq!(
        f.id,
        finding(FindingKind::EndpointSlower, ev(), &t(), 1, 2).id
    );
    assert_eq!(f.evidence.workspace.as_deref(), Some(Path::new("/w/shop")));
    assert!(f.why.contains("not known"), "{}", f.why);
    assert_eq!(
        f.suggested[0].api_request.as_ref().unwrap().route,
        "/v1/otel?service=shop-web"
    );
    // Tracked as an issue keyed on the service and the whole route.
    let (_, culprit, _) = crate::issues::fingerprint(&f).unwrap();
    assert_eq!(culprit, "shop-web GET /api/orders");
}

#[test]
fn an_errors_finding_and_the_commit_story() {
    let a: String = std::iter::repeat_n('a', 40).collect();
    let b: String = std::iter::repeat_n('b', 40).collect();
    let log = vec![
        CommitEntry {
            at_unix: 1,
            workspace: "/w/shop".into(),
            branch: None,
            head: a.clone(),
            previous_head: None,
            session_id: None,
            agent: None,
            via: "poll".into(),
        },
        CommitEntry {
            at_unix: 2,
            workspace: "/w/shop".into(),
            branch: None,
            head: b.clone(),
            previous_head: Some(a.clone()),
            session_id: Some("s-1-1".into()),
            agent: Some("claude".into()),
            via: "hook".into(),
        },
    ];
    let (moves, cut) = commit_range(&log, Some(Path::new("/w/shop")), Some(&a), Some(&b));
    assert_eq!((moves.len(), cut), (1, false));
    let mut e = ev();
    e.recent_errors = 30;
    e.recent_error_rate = 0.125;
    e.baseline_error_rate = 0.005;
    e.from_revision = Some(a.clone());
    e.to_revision = Some(b);
    e.commits = moves;
    let f = finding(FindingKind::EndpointErrors, e.clone(), &t(), 1, 2);
    assert_eq!(
        f.title,
        "`GET /api/orders` in `shop-web` fails more often: 12% of requests vs 0.5% usually"
    );
    assert!(f.why.contains("while claude worked there"), "{}", f.why);
    e.to_revision = Some(a);
    let f = finding(FindingKind::EndpointSlower, e, &t(), 1, 2);
    assert!(f.why.contains("did not change"), "{}", f.why);
}

#[test]
fn durations_read_well() {
    assert_eq!(fmt_ms(4.21), "4.2 ms");
    assert_eq!(fmt_ms(420.4), "420 ms");
    assert_eq!(fmt_ms(1340.0), "1.3 s");
}
