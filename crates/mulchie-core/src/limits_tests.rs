use super::*;
use crate::config::{Cap, WorkspaceBudget};
use crate::session::{Peak, RootSource, Usage};
use std::path::PathBuf;

fn session(id: &str, ws: &str, cpu: f64) -> Session {
    Session {
        id: id.into(),
        agent: "claude".into(),
        source: RootSource::Detected,
        status: SessionStatus::Running,
        root_pid: 10,
        root_start_time: 1,
        command: "claude".into(),
        workspace: Some(PathBuf::from(ws)),
        cwd: None,
        parent: None,
        client: None,
        client_session_id: None,
        started_unix: 0,
        ended_unix: None,
        usage: Usage {
            cpu_cores: cpu,
            memory_mb: 100.0,
            processes: 3,
            bytes_written: None,
            write_mb_per_hour: None,
            runtime_secs: 60,
            build_slots: 0,
        },
        peak: Peak::default(),
        listening: Vec::new(),
        enforced: false,
        not_enforced_reason: None,
    }
}

fn cap(max: f64, sustain: u64, action: LimitAction) -> Option<Cap> {
    Some(Cap {
        max,
        sustain_secs: sustain,
        action,
    })
}

fn cfg_cpu(max: f64, sustain: u64, action: LimitAction) -> LimitsConfig {
    LimitsConfig {
        default: Some(Budget {
            cpu_cores: cap(max, sustain, action),
            resume_after_secs: 30,
            ..Budget::default()
        }),
        ..LimitsConfig::default()
    }
}

const M: Machine = Machine {
    cores: 8,
    mem_mb: 16_000.0,
    strained: false,
};

fn breached(ev: &[LimitEvent]) -> Vec<String> {
    ev.iter()
        .filter_map(|e| match e {
            LimitEvent::Breached { breach_id } => Some(breach_id.clone()),
            _ => None,
        })
        .collect()
}

/// Over for less than sustain_secs: nothing. Then exactly one breach while
/// it stays over, with its peak; back under: cleared; over again: a new one.
#[test]
fn sustained_breach_is_raised_once_and_clears() {
    let cfg = cfg_cpu(2.0, 60, LimitAction::Notify);
    let mut l = Limits::default();
    let mut s = session("s-1-10", "/code/a", 3.0);
    assert!(l.evaluate(1000, [&s].into_iter(), &cfg, M).is_empty());
    assert!(l.evaluate(1059, [&s].into_iter(), &cfg, M).is_empty());
    let ev = l.evaluate(1060, [&s].into_iter(), &cfg, M);
    let ids = breached(&ev);
    assert_eq!(ids.len(), 1, "{ev:?}");
    let b = l.get(&ids[0]).unwrap();
    assert_eq!(
        (b.cap, b.value, b.over_since_unix, b.breached_unix),
        (CapKind::CpuCores, 3.0, 1000, 1060)
    );
    assert_eq!(b.action_state, ActionState::Pending);
    s.usage.cpu_cores = 5.0;
    assert!(
        l.evaluate(1070, [&s].into_iter(), &cfg, M).is_empty(),
        "once"
    );
    assert_eq!(l.get(&ids[0]).unwrap().peak, 5.0);
    s.usage.cpu_cores = 1.0;
    let ev = l.evaluate(1080, [&s].into_iter(), &cfg, M);
    assert_eq!(
        ev,
        vec![LimitEvent::Cleared {
            breach_id: ids[0].clone()
        }]
    );
    assert_eq!(l.get(&ids[0]).unwrap().cleared_unix, Some(1080));
    // A dip below resets the sustain clock.
    s.usage.cpu_cores = 3.0;
    assert!(l.evaluate(1090, [&s].into_iter(), &cfg, M).is_empty());
    assert!(l.evaluate(1149, [&s].into_iter(), &cfg, M).is_empty());
    assert_eq!(
        breached(&l.evaluate(1150, [&s].into_iter(), &cfg, M)).len(),
        1
    );
}

/// Each cap reads its own figure; an unknown write rate never breaches.
#[test]
fn every_cap_kind() {
    let cfg = LimitsConfig {
        default: Some(Budget {
            memory_mb: cap(50.0, 0, LimitAction::Notify),
            processes: cap(2.0, 0, LimitAction::Pause),
            write_mb_per_hour: cap(1.0, 0, LimitAction::Notify),
            runtime_hours: cap(0.01, 0, LimitAction::Stop),
            ..Budget::default()
        }),
        ..LimitsConfig::default()
    };
    let mut l = Limits::default();
    let s = session("s-1-10", "/code/a", 0.0);
    let ev = l.evaluate(1000, [&s].into_iter(), &cfg, M);
    let mut kinds: Vec<CapKind> = breached(&ev)
        .iter()
        .map(|id| l.get(id).unwrap().cap)
        .collect();
    kinds.sort();
    assert_eq!(
        kinds,
        vec![CapKind::MemoryMb, CapKind::Processes, CapKind::RuntimeHours]
    );
    let p = l.breaches().find(|b| b.cap == CapKind::Processes).unwrap();
    assert_eq!(p.action, LimitAction::Pause);
    let mut s2 = s.clone();
    s2.id = "s-2-11".into();
    s2.usage.write_mb_per_hour = Some(5.0);
    let ev = l.evaluate(1001, [&s, &s2].into_iter(), &cfg, M);
    assert!(breached(&ev)
        .iter()
        .any(|id| l.get(id).unwrap().cap == CapKind::WriteMbPerHour
            && l.get(id).unwrap().session_id == "s-2-11"));
}

/// The longest matching workspace override replaces the default whole; a
/// session with no budget is only watched for hogs.
#[test]
fn workspace_overrides_and_no_budget() {
    let mut cfg = cfg_cpu(2.0, 0, LimitAction::Notify);
    cfg.workspaces = vec![WorkspaceBudget {
        path: PathBuf::from("/code/big"),
        budget: Budget {
            cpu_cores: cap(8.0, 0, LimitAction::Notify),
            ..Budget::default()
        },
    }];
    let mut l = Limits::default();
    let a = session("s-1-10", "/code/a", 3.0);
    let big = session("s-2-11", "/code/big/wt", 3.0);
    let ev = l.evaluate(1000, [&a, &big].into_iter(), &cfg, M);
    let ids = breached(&ev);
    assert_eq!(ids.len(), 1);
    assert_eq!(l.get(&ids[0]).unwrap().session_id, "s-1-10");

    let none = LimitsConfig::default();
    assert!(none.report_only());
    let mut l = Limits::default();
    assert!(l.evaluate(1000, [&a].into_iter(), &none, M).is_empty());
    assert_eq!(l.breaches().count(), 0);
}

/// A hog: at least hog_share_pct of the machine for hog_sustain_secs, only
/// while health is strained; with or without budgets.
#[test]
fn hogs_are_reported_only_under_strain() {
    let cfg = LimitsConfig {
        hog_share_pct: 50.0,
        hog_sustain_secs: 120,
        ..LimitsConfig::default()
    };
    let strained = Machine {
        strained: true,
        ..M
    };
    let mut l = Limits::default();
    let s = session("s-1-10", "/code/a", 5.0); // 5 of 8 cores
    l.evaluate(1000, [&s].into_iter(), &cfg, M);
    l.evaluate(1200, [&s].into_iter(), &cfg, M);
    assert!(l.hogs().is_empty(), "not strained");
    l.evaluate(1300, [&s].into_iter(), &cfg, strained);
    assert!(l.hogs().is_empty(), "not for long enough");
    l.evaluate(1420, [&s].into_iter(), &cfg, strained);
    assert_eq!(l.hogs().len(), 1);
    assert_eq!(l.hogs()[0].resource, "cpu");
    assert!((l.hogs()[0].share_pct - 62.5).abs() < 1e-9);
    let mut small = s.clone();
    small.usage.cpu_cores = 1.0;
    l.evaluate(1423, [&small].into_iter(), &cfg, strained);
    assert!(l.hogs().is_empty());
}

/// A session that ends clears its breaches.
#[test]
fn ended_sessions_clear_their_breaches() {
    let cfg = cfg_cpu(2.0, 0, LimitAction::Notify);
    let mut l = Limits::default();
    let mut s = session("s-1-10", "/code/a", 3.0);
    let id = breached(&l.evaluate(1000, [&s].into_iter(), &cfg, M))[0].clone();
    s.status = SessionStatus::Ended;
    let ev = l.evaluate(1003, [&s].into_iter(), &cfg, M);
    assert_eq!(
        ev,
        vec![LimitEvent::Cleared {
            breach_id: id.clone()
        }]
    );
    assert!(l.evaluate(1006, std::iter::empty(), &cfg, M).is_empty());
}

/// A pause breach that was carried out is due for resume resume_after_secs
/// after the pause, once.
#[test]
fn resume_is_due_after_the_pause_once() {
    let cfg = cfg_cpu(2.0, 0, LimitAction::Pause);
    let mut l = Limits::default();
    let mut s = session("s-1-10", "/code/a", 3.0);
    let id = breached(&l.evaluate(1000, [&s].into_iter(), &cfg, M))[0].clone();
    l.get_mut(&id).unwrap().action_state = ActionState::Done;
    s.usage.cpu_cores = 0.0;
    l.evaluate(1010, [&s].into_iter(), &cfg, M);
    assert!(l.evaluate(1029, [&s].into_iter(), &cfg, M).is_empty());
    let ev = l.evaluate(1030, [&s].into_iter(), &cfg, M);
    assert_eq!(
        ev,
        vec![LimitEvent::ResumeDue {
            breach_id: id.clone()
        }]
    );
    assert!(
        l.evaluate(1050, [&s].into_iter(), &cfg, M).is_empty(),
        "once"
    );
    // A skipped pause has nothing to resume.
    let mut l = Limits::default();
    s.usage.cpu_cores = 3.0;
    let id = breached(&l.evaluate(2000, [&s].into_iter(), &cfg, M))[0].clone();
    l.get_mut(&id).unwrap().action_state = ActionState::Skipped;
    s.usage.cpu_cores = 0.0;
    l.evaluate(2001, [&s].into_iter(), &cfg, M);
    assert!(l.evaluate(3000, [&s].into_iter(), &cfg, M).is_empty());
}

/// Pausing does not lower memory: the pause still ends after
/// resume_after_secs, the breach ends with it, and a session still over the
/// cap is breached (and paused) again after sustain_secs.
#[test]
fn a_pause_on_a_cap_that_stays_over_resumes_and_rearms() {
    let cfg = LimitsConfig {
        default: Some(Budget {
            memory_mb: cap(50.0, 10, LimitAction::Pause),
            resume_after_secs: 30,
            ..Budget::default()
        }),
        ..LimitsConfig::default()
    };
    let mut l = Limits::default();
    let s = session("s-1-10", "/code/a", 0.0); // 100 MB, over 50
    l.evaluate(1000, [&s].into_iter(), &cfg, M);
    let id = breached(&l.evaluate(1010, [&s].into_iter(), &cfg, M))[0].clone();
    l.get_mut(&id).unwrap().action_state = ActionState::Done;
    assert!(l.evaluate(1039, [&s].into_iter(), &cfg, M).is_empty());
    let ev = l.evaluate(1040, [&s].into_iter(), &cfg, M);
    assert_eq!(
        ev,
        vec![LimitEvent::ResumeDue {
            breach_id: id.clone()
        }]
    );
    assert_eq!(l.get(&id).unwrap().cleared_unix, Some(1040));
    assert!(breached(&l.evaluate(1041, [&s].into_iter(), &cfg, M)).is_empty());
    assert!(breached(&l.evaluate(1050, [&s].into_iter(), &cfg, M)).is_empty());
    let again = breached(&l.evaluate(1051, [&s].into_iter(), &cfg, M));
    assert_eq!(again.len(), 1);
    assert_ne!(again[0], id);
}

/// Exactly-once delivery per session.
#[test]
fn undelivered_breaches_are_taken_once() {
    let cfg = cfg_cpu(2.0, 0, LimitAction::Notify);
    let mut l = Limits::default();
    let a = session("s-1-10", "/code/a", 3.0);
    let b = session("s-2-11", "/code/b", 3.0);
    l.evaluate(1000, [&a, &b].into_iter(), &cfg, M);
    let got = l.take_undelivered("s-1-10");
    assert_eq!(got.len(), 1);
    assert!(got[0].delivered);
    assert!(l.take_undelivered("s-1-10").is_empty());
    assert_eq!(l.take_undelivered("s-2-11").len(), 1);
    assert!(l.breaches().all(|b| b.delivered));
}

#[test]
fn sentences_name_the_cap_and_action() {
    let cfg = cfg_cpu(2.0, 60, LimitAction::Pause);
    let mut l = Limits::default();
    let s = session("s-1-10", "/code/a", 3.0);
    l.evaluate(1000, [&s].into_iter(), &cfg, M);
    let id = breached(&l.evaluate(1060, [&s].into_iter(), &cfg, M))[0].clone();
    let t = l.get(&id).unwrap().sentence();
    assert!(
        t.contains("3.0 CPU cores") && t.contains("limit of 2") && t.contains("pauses"),
        "{t}"
    );
}

/// Removing the cap from the config clears its breach.
#[test]
fn removing_a_cap_clears_its_breach() {
    let cfg = cfg_cpu(2.0, 0, LimitAction::Notify);
    let mut l = Limits::default();
    let s = session("s-1-10", "/code/a", 3.0);
    let id = breached(&l.evaluate(1000, [&s].into_iter(), &cfg, M))[0].clone();
    let ev = l.evaluate(1003, [&s].into_iter(), &LimitsConfig::default(), M);
    assert_eq!(ev, vec![LimitEvent::Cleared { breach_id: id }]);
}
