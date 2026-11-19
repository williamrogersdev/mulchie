//! One test per rule of issue grouping, lifecycle and storage.

use super::*;
use crate::watch::Evidence;

fn proc_finding(kind: FindingKind, pid: u32, cmd: &str, ws: &str) -> Finding {
    Finding {
        id: format!("{}:{pid}:{}", kind.code(), 1000 + pid),
        kind,
        category: kind.category(),
        severity: Severity::Medium,
        title: format!("{cmd} misbehaves"),
        why: String::new(),
        evidence: Evidence {
            pid: Some(pid),
            start_time: Some(1000 + u64::from(pid)),
            cmd: Some(cmd.into()),
            workspace: Some(PathBuf::from(ws)),
            ..Evidence::default()
        },
        suggested: Vec::new(),
        first_seen_unix: 100,
        last_seen_unix: 100,
        dismissed: false,
        dismissed_until: None,
    }
}

fn runaway(pid: u32, cmd: &str) -> Finding {
    proc_finding(FindingKind::RunawayCpu, pid, cmd, "/code/site")
}

#[test]
fn normalize_strips_paths_numbers_and_ids() {
    assert_eq!(
        normalize_command("/usr/bin/node /code/site/node_modules/.bin/next dev --port=3001"),
        "node next dev --port=#"
    );
    assert_eq!(
        normalize_command("python3 -m http.server 8000"),
        "python3 -m http.server #"
    );
    assert_eq!(
        normalize_command("worker 3f2a9c1b7e4d5a60 v1.2.3"),
        "worker # v1.2.3"
    );
    assert_eq!(normalize_command("cargo build"), "cargo build");
}

#[test]
fn the_same_command_in_a_new_process_is_the_same_issue() {
    let a = fingerprint(&runaway(10, "/usr/bin/node next dev --port=3001")).unwrap();
    let b = fingerprint(&runaway(99, "/opt/node next dev --port=3002")).unwrap();
    assert_eq!(a.0, b.0);
    assert_eq!(a.1, "node next dev --port=#");
}

#[test]
fn another_workspace_kind_or_command_is_another_issue() {
    let base = fingerprint(&runaway(10, "node next dev")).unwrap().0;
    let other_ws = proc_finding(FindingKind::RunawayCpu, 10, "node next dev", "/code/api");
    let other_kind = proc_finding(
        FindingKind::GrowingMemory,
        10,
        "node next dev",
        "/code/site",
    );
    assert_ne!(base, fingerprint(&other_ws).unwrap().0);
    assert_ne!(base, fingerprint(&other_kind).unwrap().0);
    assert_ne!(base, fingerprint(&runaway(10, "node vite")).unwrap().0);
}

#[test]
fn security_posture_and_update_findings_are_not_issues() {
    for k in [
        FindingKind::ExposedListener,
        FindingKind::InsecurePermissions,
        FindingKind::MulchieUpdate,
        FindingKind::PausedListUnreadable,
    ] {
        assert!(!tracked(k), "{k:?}");
        assert!(fingerprint(&proc_finding(k, 1, "x", "/w")).is_none());
    }
    assert!(tracked(FindingKind::RunawayChain));
    assert!(tracked(FindingKind::MemoryLow));
}

#[test]
fn session_findings_group_by_agent_and_cap_not_session() {
    let mut a = proc_finding(FindingKind::LimitExceeded, 1, "claude", "/code/site");
    a.id = "limit_exceeded:s-100-1:mem".into();
    a.evidence.name = Some("claude".into());
    let mut b = a.clone();
    b.id = "limit_exceeded:s-200-7:mem".into();
    let fa = fingerprint(&a).unwrap();
    assert_eq!(fa.0, fingerprint(&b).unwrap().0);
    assert_eq!(fa.1, "claude mem");
}

#[test]
fn memory_low_uses_its_stable_id() {
    let mut f = proc_finding(FindingKind::MemoryLow, 1, "x", "/w");
    f.id = "memory_low".into();
    f.evidence.workspace = None;
    assert_eq!(fingerprint(&f).unwrap().1, "machine");
    f.id = "memory_low:app:Xcode".into();
    assert_eq!(fingerprint(&f).unwrap().1, "app:Xcode");
}

#[test]
fn a_finding_that_lasts_counts_once_and_a_new_one_counts_again() {
    let mut book = IssueBook::default();
    book.observe(&[runaway(10, "node next dev")], 100);
    book.observe(&[runaway(10, "node next dev")], 200);
    let i = book.list(None, false, None).remove(0);
    assert_eq!(i.occurrences, 1);
    assert!(i.active);
    assert_eq!(i.last_seen_unix, 200);

    book.observe(&[], 300);
    assert!(!book.get(&i.id).unwrap().active);
    book.observe(&[runaway(11, "node next dev")], 400);
    let i = book.get(&i.id).unwrap();
    assert_eq!(i.occurrences, 2);
    assert_eq!(i.first_seen_unix, 100);
    assert_eq!(i.finding_id, "runaway_cpu:11:1011");
}

#[test]
fn a_resolved_issue_that_comes_back_regresses() {
    let mut book = IssueBook::default();
    book.observe(&[runaway(10, "node next dev")], 100);
    let id = book.list(None, false, None)[0].id.clone();
    book.set_status(&id, IssueStatus::Resolved, 150).unwrap();
    // The same finding still current is not a regression.
    assert!(book
        .observe(&[runaway(10, "node next dev")], 160)
        .is_empty());
    assert_eq!(book.get(&id).unwrap().status, IssueStatus::Resolved);
    book.observe(&[], 170);
    let back = book.observe(&[runaway(12, "node next dev")], 180);
    assert_eq!(back.len(), 1);
    let i = book.get(&id).unwrap();
    assert_eq!(i.status, IssueStatus::Regressed);
    assert_eq!(i.regressions, 1);
    assert_eq!(i.status_unix, Some(180));
}

#[test]
fn an_ignored_issue_is_counted_but_never_regresses() {
    let mut book = IssueBook::default();
    book.observe(&[runaway(10, "node next dev")], 100);
    let id = book.list(None, false, None)[0].id.clone();
    book.set_status(&id, IssueStatus::Ignored, 110).unwrap();
    book.observe(&[], 120);
    assert!(book
        .observe(&[runaway(11, "node next dev")], 130)
        .is_empty());
    let i = book.get(&id).unwrap();
    assert_eq!(i.status, IssueStatus::Ignored);
    assert_eq!(i.occurrences, 2);
    assert!(book.list(None, false, None).is_empty());
    assert_eq!(book.list(None, true, None).len(), 1);
    assert_eq!(book.counts(), IssueCounts::default());
}

#[test]
fn unknown_issue_ids_are_none() {
    let mut book = IssueBook::default();
    assert!(book.set_status("nope", IssueStatus::Resolved, 1).is_none());
}

#[test]
fn old_issues_are_dropped_and_the_book_is_capped() {
    let mut book = IssueBook::default();
    book.observe(&[runaway(1, "old")], 100);
    book.observe(&[], 100);
    book.observe(&[runaway(2, "kept")], 100 + RETAIN_SECS);
    let left: Vec<String> = book
        .list(None, true, None)
        .into_iter()
        .map(|i| i.culprit)
        .collect();
    assert_eq!(left, vec!["kept".to_string()]);

    let mut book = IssueBook::default();
    for n in 0..(MAX_ISSUES as u32 + 5) {
        book.observe(
            &[runaway(n, &format!("cmd{}", word_for(n)))],
            1000 + u64::from(n),
        );
    }
    assert_eq!(book.list(None, true, None).len(), MAX_ISSUES);
    // The ones seen longest ago went first.
    assert!(book
        .list(None, true, None)
        .iter()
        .all(|i| i.culprit != "cmda"));
}

/// Letters only, so `normalize_command` keeps each command distinct.
fn word_for(n: u32) -> String {
    let mut n = n;
    let mut s = String::new();
    loop {
        s.push((b'a' + (n % 26) as u8) as char);
        n /= 26;
        if n == 0 {
            break;
        }
    }
    s
}

#[test]
fn the_book_survives_a_restart_without_counting_current_findings_again() {
    let t = tempfile::tempdir().unwrap();
    let paths = Paths::under(t.path());
    let mut book = IssueBook::default();
    book.observe(&[runaway(10, "node next dev")], 100);
    let id = book.list(None, false, None)[0].id.clone();
    book.set_status(&id, IssueStatus::Resolved, 110).unwrap();
    assert!(book.save_if_needed(&paths, 110).unwrap());
    assert!(!book.save_if_needed(&paths, 111).unwrap());

    let file = IssueBook::file(&paths);
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
        0o600
    );

    let mut again = IssueBook::load(&paths);
    let i = again.get(&id).unwrap().clone();
    assert_eq!(i.status, IssueStatus::Resolved);
    assert!(again
        .observe(&[runaway(10, "node next dev")], 200)
        .is_empty());
    assert_eq!(again.get(&id).unwrap().occurrences, 1);
}

#[test]
fn a_symlinked_or_foreign_file_is_ignored() {
    let t = tempfile::tempdir().unwrap();
    let paths = Paths::under(t.path());
    std::fs::create_dir_all(&paths.config_dir).unwrap();
    let elsewhere = t.path().join("elsewhere.json");
    std::fs::write(&elsewhere, br#"{"format":1,"issues":[],"live":[]}"#).unwrap();
    std::os::unix::fs::symlink(&elsewhere, IssueBook::file(&paths)).unwrap();
    assert!(IssueBook::load(&paths).issues.is_empty());

    std::fs::remove_file(IssueBook::file(&paths)).unwrap();
    std::fs::write(IssueBook::file(&paths), br#"{"format":99,"issues":[]}"#).unwrap();
    assert!(IssueBook::load(&paths).issues.is_empty());
}

#[test]
fn an_active_issue_saves_its_last_seen_every_few_minutes() {
    let t = tempfile::tempdir().unwrap();
    let paths = Paths::under(t.path());
    let mut book = IssueBook::default();
    book.observe(&[runaway(10, "x")], 100);
    assert!(book.save_if_needed(&paths, 100).unwrap());
    book.observe(&[runaway(10, "x")], 200);
    assert!(!book.save_if_needed(&paths, 200).unwrap());
    book.observe(&[runaway(10, "x")], 100 + SAVE_SEEN_SECS);
    assert!(book.save_if_needed(&paths, 100 + SAVE_SEEN_SECS).unwrap());
}
