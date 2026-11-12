//! One test per rule of the memory guard's decision.

use super::*;
use crate::config::MemoryScope;

fn unit(key: &str, mb: f64) -> Unit {
    Unit {
        key: key.into(),
        name: key.into(),
        kind: UnitKind::Session,
        session_id: None,
        workspace: None,
        mem_mb: mb,
        procs: vec![(10, 1)],
    }
}

fn mem(free: f32) -> Memory {
    Memory {
        free_pct: free,
        pressure: Some("normal".into()),
    }
}

fn stop() -> MemoryGuardConfig {
    MemoryGuardConfig {
        action: MemoryAction::Stop,
        scope: MemoryScope::Sessions,
        free_pct: 5.0,
        sustain_secs: 10,
        min_mb: 300,
        cooldown_secs: 30,
        ..MemoryGuardConfig::default()
    }
}

#[test]
fn the_default_only_reports() {
    let c = MemoryGuardConfig::default();
    assert_eq!(c.action, MemoryAction::Notify);
    assert_eq!(c.scope, MemoryScope::Sessions);
    assert!(!c.include_agent);
    let mut g = MemGuard::default();
    let u = [unit("a", 4000.0)];
    assert_eq!(g.evaluate(0, &mem(2.0), &u, &c, true), None);
    assert_eq!(g.evaluate(100, &mem(2.0), &u, &c, true), None);
    assert!(g.low());
    assert_eq!(g.candidate().unwrap().key, "a");
    assert_eq!(g.closings().count(), 0);
    // Announced once per episode.
    assert!(g.take_announcement().is_some());
    assert!(g.take_announcement().is_none());
}

#[test]
fn low_memory_must_last_sustain_secs_and_closes_the_biggest() {
    let c = stop();
    let mut g = MemGuard::default();
    let u = [
        unit("small", 400.0),
        unit("big", 3000.0),
        unit("mid", 900.0),
    ];
    assert_eq!(g.evaluate(0, &mem(3.0), &u, &c, true), None);
    assert_eq!(g.evaluate(9, &mem(3.0), &u, &c, true), None);
    let id = g.evaluate(10, &mem(3.0), &u, &c, true).unwrap();
    let cl = g.get(&id).unwrap();
    assert_eq!(cl.key, "big");
    assert_eq!(cl.trigger, Trigger::MemoryLow);
    assert_eq!(cl.state, ActionState::Pending);
    assert_eq!(cl.targets, vec![(10, 1)]);
}

#[test]
fn a_recovery_resets_the_clock() {
    let c = stop();
    let mut g = MemGuard::default();
    let u = [unit("big", 3000.0)];
    g.evaluate(0, &mem(3.0), &u, &c, true);
    assert_eq!(g.evaluate(8, &mem(20.0), &u, &c, true), None);
    assert_eq!(g.evaluate(12, &mem(3.0), &u, &c, true), None);
    assert!(g.evaluate(22, &mem(3.0), &u, &c, true).is_some());
}

#[test]
fn critical_pressure_counts_as_low_unless_turned_off() {
    let crit = Memory {
        free_pct: 30.0,
        pressure: Some("critical".into()),
    };
    let u = [unit("big", 3000.0)];
    let mut c = stop();
    c.sustain_secs = 0;
    assert!(MemGuard::default()
        .evaluate(0, &crit, &u, &c, true)
        .is_some());
    c.on_pressure = false;
    assert!(MemGuard::default()
        .evaluate(0, &crit, &u, &c, true)
        .is_none());
}

#[test]
fn one_closing_per_cooldown() {
    let mut c = stop();
    c.sustain_secs = 0;
    let mut g = MemGuard::default();
    let u = [unit("big", 3000.0), unit("mid", 900.0)];
    let first = g.evaluate(0, &mem(3.0), &u, &c, true).unwrap();
    assert_eq!(g.get(&first).unwrap().key, "big");
    assert_eq!(g.evaluate(29, &mem(3.0), &u, &c, true), None);
    // Still low after the cooldown: the biggest unit then goes.
    let second = g
        .evaluate(30, &mem(3.0), &[unit("mid", 900.0)], &c, true)
        .unwrap();
    assert_eq!(g.get(&second).unwrap().key, "mid");
}

#[test]
fn small_units_are_spared_and_nothing_closable_closes_nothing() {
    let mut c = stop();
    c.sustain_secs = 0;
    let mut g = MemGuard::default();
    assert_eq!(
        g.evaluate(0, &mem(1.0), &[unit("tiny", 299.0)], &c, true),
        None
    );
    assert_eq!(g.evaluate(3, &mem(1.0), &[], &c, true), None);
    assert!(g.low());
    assert!(g.candidate().is_none());
}

#[test]
fn an_engine_that_may_not_act_only_reports() {
    let mut c = stop();
    c.sustain_secs = 0;
    let mut g = MemGuard::default();
    assert_eq!(
        g.evaluate(0, &mem(1.0), &[unit("big", 3000.0)], &c, false),
        None
    );
    assert!(g.candidate().is_some());
    assert_eq!(g.closings().count(), 0);
}

#[test]
fn off_watches_nothing() {
    let mut c = stop();
    c.action = MemoryAction::Off;
    let mut g = MemGuard::default();
    assert_eq!(
        g.evaluate(100, &mem(1.0), &[unit("big", 3000.0)], &c, true),
        None
    );
    assert!(!g.low());
    assert!(g.candidate().is_none());
}

#[test]
fn a_unit_over_the_cap_is_closed_without_low_memory() {
    let mut c = stop();
    c.unit_max_mb = 2000;
    let mut g = MemGuard::default();
    let u = [unit("ok", 1500.0), unit("fat", 2500.0)];
    assert_eq!(g.evaluate(0, &mem(40.0), &u, &c, true), None);
    let id = g.evaluate(10, &mem(40.0), &u, &c, true).unwrap();
    let cl = g.get(&id).unwrap();
    assert_eq!((cl.key.as_str(), cl.trigger), ("fat", Trigger::OverCap));
    assert!(!g.low());
    // Back under the cap: the clock starts again.
    let mut g = MemGuard::default();
    g.evaluate(0, &mem(40.0), &u, &c, true);
    g.evaluate(5, &mem(40.0), &[unit("fat", 1000.0)], &c, true);
    assert_eq!(g.evaluate(10, &mem(40.0), &u, &c, true), None);
}

#[test]
fn a_refused_unit_is_left_alone_for_a_while() {
    let mut c = stop();
    c.sustain_secs = 0;
    let mut g = MemGuard::default();
    let u = [unit("big", 3000.0), unit("mid", 900.0)];
    let id = g.evaluate(0, &mem(3.0), &u, &c, true).unwrap();
    g.set_result(&id, ActionState::Skipped, "0 of 1 process(es)".into(), 0);
    let next = g.evaluate(60, &mem(3.0), &u, &c, true).unwrap();
    assert_eq!(g.get(&next).unwrap().key, "mid");
    let later = g
        .evaluate(REFUSED_BACKOFF_SECS + 1, &mem(3.0), &u, &c, true)
        .unwrap();
    assert_eq!(g.get(&later).unwrap().key, "big");
}

#[test]
fn closings_are_capped_and_newest_first() {
    let mut c = stop();
    c.sustain_secs = 0;
    c.cooldown_secs = 10;
    let mut g = MemGuard::default();
    for i in 0..(CLOSINGS_KEPT as u64 + 5) {
        let u = [unit(&format!("u{i}"), 1000.0)];
        assert!(g.evaluate(i * 10, &mem(3.0), &u, &c, true).is_some());
    }
    assert_eq!(g.closings().count(), CLOSINGS_KEPT);
    assert_eq!(g.closings().next().unwrap().key, "u54");
}

#[test]
fn config_ranges_are_checked() {
    let mut e = Vec::new();
    MemoryGuardConfig::default().validate(&mut e);
    assert!(e.is_empty(), "{e:?}");
    let bad = MemoryGuardConfig {
        free_pct: 80.0,
        cooldown_secs: 1,
        unit_max_mb: 5,
        never: vec!["a/b".into()],
        ..MemoryGuardConfig::default()
    };
    bad.validate(&mut e);
    assert_eq!(e.len(), 4, "{e:?}");
}
