use super::*;
use std::os::unix::fs::{MetadataExt, PermissionsExt};

fn limits() -> RecordLimits {
    RecordLimits {
        max_bytes: 1_048_576,
        max_events: 1000,
        max_total_bytes: 10 * 1_048_576,
        retain_secs: 14 * 86_400,
        per_minute: 3,
    }
}

const S: &str = "s-100-10";

#[test]
fn events_have_seq_and_files_are_private() {
    let t = tempfile::tempdir().unwrap();
    let dir = t.path().join("sessions");
    let r = Record::new(dir.clone(), limits());
    r.event(S, 1000, "session_start", json!({"agent": "claude"}));
    r.event(S, 1001, "hook", json!({"event": "prompt"}));
    r.flush();
    let ev = r.read(S, 0, 100).unwrap();
    assert_eq!(ev.len(), 2);
    assert_eq!(
        (ev[0]["seq"].as_u64(), ev[0]["type"].as_str()),
        (Some(1), Some("session_start"))
    );
    assert_eq!(ev[1]["seq"], 2);
    assert_eq!(r.read(S, 1, 100).unwrap().len(), 1, "since");
    assert_eq!(r.read(S, 0, 1).unwrap().len(), 1, "limit");
    assert!(r.read("s-1-1", 0, 10).unwrap().is_empty(), "no file");
    assert_eq!(std::fs::metadata(&dir).unwrap().mode() & 0o777, 0o700);
    assert_eq!(
        std::fs::metadata(file_of(&dir, S)).unwrap().mode() & 0o777,
        0o600
    );
    // A new writer continues the numbering.
    let r2 = Record::new(dir, limits());
    r2.event(S, 1002, "hook", json!({}));
    r2.flush();
    assert_eq!(r2.read(S, 2, 10).unwrap()[0]["seq"], 3);
    assert_eq!(r.error(), None);
}

#[test]
fn caps_end_with_one_truncated_marker() {
    let t = tempfile::tempdir().unwrap();
    let r = Record::new(
        t.path().to_path_buf(),
        RecordLimits {
            max_events: 5,
            ..limits()
        },
    );
    for i in 0..20 {
        r.event(S, 1000 + i, "hook", json!({"i": i}));
    }
    r.flush();
    let ev = r.read(S, 0, 100).unwrap();
    assert_eq!(ev.len(), 5, "{ev:?}");
    assert_eq!(ev[4]["type"], "truncated");
    assert_eq!(ev.iter().filter(|e| e["type"] == "truncated").count(), 1);

    let t = tempfile::tempdir().unwrap();
    let r = Record::new(
        t.path().to_path_buf(),
        RecordLimits {
            max_bytes: 2000,
            ..limits()
        },
    );
    for i in 0..100 {
        r.event(S, 1000 + i, "hook", json!({"pad": "x".repeat(50)}));
    }
    r.flush();
    let size = std::fs::metadata(file_of(t.path(), S)).unwrap().len();
    assert!(size <= 2000, "{size}");
    let ev = r.read(S, 0, 1000).unwrap();
    assert_eq!(ev.last().unwrap()["type"], "truncated");
}

/// Process starts: at most per_minute written per minute, the rest summed
/// up when the minute ends or the session closes.
#[test]
fn process_starts_are_coalesced_per_minute() {
    let t = tempfile::tempdir().unwrap();
    let r = Record::new(t.path().to_path_buf(), limits());
    for i in 0..10u32 {
        r.process_start(
            S,
            6000 + i as u64,
            100 + i,
            if i % 2 == 0 { "rustc" } else { "cc" },
            "x",
        );
    }
    // Next minute: the summary comes first.
    r.process_start(S, 6061, 200, "ld", "ld -o x");
    r.close(S, 6062);
    r.flush();
    let ev = r.read(S, 0, 100).unwrap();
    let types: Vec<&str> = ev.iter().map(|e| e["type"].as_str().unwrap()).collect();
    assert_eq!(
        types,
        vec![
            "process_start",
            "process_start",
            "process_start",
            "process_starts_summarised",
            "process_start"
        ]
    );
    let sum = &ev[3];
    assert_eq!(sum["count"], 7);
    assert_eq!(sum["minute_unix"], 6000);
    assert_eq!(sum["names"]["cc"], 4);
    assert_eq!(sum["names"]["rustc"], 3);
}

/// Retention by age and the total cap remove ended sessions' records,
/// oldest first; live ones and other files stay.
#[test]
fn retention_and_total_cap() {
    let t = tempfile::tempdir().unwrap();
    let dir = t.path().to_path_buf();
    let r = Record::new(
        dir.clone(),
        RecordLimits {
            max_total_bytes: 3000,
            retain_secs: 100,
            ..limits()
        },
    );
    let set_mtime = |id: &str, secs: u64| {
        let f = std::fs::File::options()
            .write(true)
            .open(file_of(&dir, id))
            .unwrap();
        f.set_modified(std::time::UNIX_EPOCH + Duration::from_secs(secs))
            .unwrap();
    };
    for (id, n) in [("s-1-1", 10), ("s-2-2", 10), ("s-3-3", 10), ("s-4-4", 10)] {
        for i in 0..n {
            r.event(id, 1000 + i, "hook", json!({"pad": "y".repeat(40)}));
        }
    }
    std::fs::write(dir.join("paused.json"), "[]").unwrap();
    r.flush();
    set_mtime("s-1-1", 100); // past the retention
    set_mtime("s-2-2", 9_000);
    set_mtime("s-3-3", 9_500);
    set_mtime("s-4-4", 9_000); // live
    r.sweep(10_000, vec!["s-4-4".into()]);
    r.flush();
    assert!(!file_of(&dir, "s-1-1").exists(), "expired");
    assert!(file_of(&dir, "s-4-4").exists(), "live");
    assert!(dir.join("paused.json").exists());
    let total: u64 = ["s-2-2", "s-3-3", "s-4-4"]
        .iter()
        .filter_map(|id| std::fs::metadata(file_of(&dir, id)).ok())
        .map(|m| m.len())
        .sum();
    assert!(total <= 3000, "{total}");
    assert!(!file_of(&dir, "s-2-2").exists(), "oldest goes first");
}

/// A write failure is reported, not a panic, and clears once writes work.
#[test]
fn write_failures_surface_as_an_error() {
    let t = tempfile::tempdir().unwrap();
    let dir = t.path().join("sessions");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();
    let r = Record::new(dir.clone(), limits());
    r.event(S, 1, "hook", json!({}));
    r.flush();
    let failed = r.error();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    if std::fs::File::create(dir.join("probe")).is_ok() && failed.is_none() {
        // Running as root: permissions do not apply.
        return;
    }
    assert!(failed
        .unwrap()
        .contains("could not write the session record"));
    r.event(S, 2, "hook", json!({}));
    r.flush();
    assert_eq!(r.error(), None);
    assert_eq!(r.read(S, 0, 10).unwrap().len(), 1);
}

/// N12 and A5: pages seek through an index kept per file (one mark per
/// STRIDE events, extended as the file grows, reset when it is replaced);
/// `tail` and `before` page backwards; a read waits on the writer only while
/// events are queued.
#[test]
fn pages_seek_through_the_index() {
    let t = tempfile::tempdir().unwrap();
    let dir = t.path().join("sessions");
    let r = Record::new(
        dir.clone(),
        RecordLimits {
            max_events: 100_000,
            ..limits()
        },
    );
    for i in 0..1000 {
        r.event(S, i, "hook", json!({"i": i}));
    }
    r.flush();
    assert_eq!(r.pending.load(Ordering::SeqCst), 0);
    let seqs = |p: &Page| -> Vec<u64> {
        p.events
            .iter()
            .map(|e| e["seq"].as_u64().unwrap())
            .collect()
    };
    let p = r
        .page(
            S,
            Query::Since {
                since: 900,
                limit: 10,
            },
        )
        .unwrap();
    assert_eq!(seqs(&p), (901..=910).collect::<Vec<_>>());
    assert_eq!(p.prev_before, Some(901));
    let p = r.page(S, Query::Tail { n: 5, before: None }).unwrap();
    assert_eq!(seqs(&p), (996..=1000).collect::<Vec<_>>());
    assert_eq!(p.prev_before, Some(996));
    let p = r
        .page(
            S,
            Query::Tail {
                n: 5,
                before: Some(996),
            },
        )
        .unwrap();
    assert_eq!(seqs(&p), (991..=995).collect::<Vec<_>>());
    let p = r
        .page(
            S,
            Query::Tail {
                n: 10,
                before: Some(3),
            },
        )
        .unwrap();
    assert_eq!(seqs(&p), vec![1, 2]);
    assert_eq!(p.prev_before, None, "the start of the record");
    {
        let map = r.index.0.lock().unwrap();
        let idx = &map[S];
        assert_eq!(idx.marks.len(), 1000usize.div_ceil(STRIDE as usize));
        assert!(idx.marks.windows(2).all(|w| w[1].0 - w[0].0 == STRIDE));
        assert_eq!(idx.last_seq, 1000);
    }
    // The file grows: only the new lines are indexed.
    let scanned = r.index.0.lock().unwrap()[S].scanned;
    for i in 0..10 {
        r.event(S, 2000 + i, "hook", json!({}));
    }
    r.flush();
    let p = r.page(S, Query::Tail { n: 2, before: None }).unwrap();
    assert_eq!(seqs(&p), vec![1009, 1010]);
    assert!(r.index.0.lock().unwrap()[S].scanned > scanned);
    // A replaced file (another inode) starts the index over.
    let path = file_of(&dir, S);
    std::fs::remove_file(&path).unwrap();
    let mut body = String::new();
    for seq in 1..=3 {
        body.push_str(&format!("{{\"seq\":{seq},\"t\":1,\"type\":\"hook\"}}\n"));
    }
    body.push_str("{\"seq\":4,\"t\":1"); // still being written
    std::fs::write(&path, body).unwrap();
    let p = r
        .page(
            S,
            Query::Tail {
                n: 10,
                before: None,
            },
        )
        .unwrap();
    assert_eq!(seqs(&p), vec![1, 2, 3]);
    // Nothing queued: a reader's flush returns at once.
    let at = std::time::Instant::now();
    r.flush_pending(Duration::from_secs(5));
    assert!(at.elapsed() < Duration::from_secs(1));
}

/// N11: a symlink, a FIFO or a hard link where a session file belongs is
/// refused (never followed, never blocks) and the failure surfaces.
#[test]
fn writes_refuse_links_and_special_files() {
    let t = tempfile::tempdir().unwrap();
    let dir = t.path().join("sessions");
    std::fs::create_dir_all(&dir).unwrap();
    let outside = t.path().join("victim");
    std::fs::write(&outside, b"keep\n").unwrap();
    let cases = ["s-1-1", "s-2-2", "s-3-3"];
    std::os::unix::fs::symlink(&outside, file_of(&dir, cases[0])).unwrap();
    let fifo = std::ffi::CString::new(file_of(&dir, cases[1]).to_str().unwrap()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    std::fs::hard_link(&outside, file_of(&dir, cases[2])).unwrap();
    for s in cases {
        let r = Record::new(dir.clone(), limits());
        r.event(s, 1, "hook", json!({"x": 1}));
        r.flush();
        let err = r.error().unwrap_or_default();
        assert!(
            err.contains("could not write the session record"),
            "{s}: {err}"
        );
    }
    assert_eq!(std::fs::read(&outside).unwrap(), b"keep\n");
    // A plain file still works.
    let r = Record::new(dir.clone(), limits());
    r.event(S, 1, "hook", json!({}));
    r.flush();
    assert_eq!(r.error(), None);
}

/// N5: the directory cap holds at write time, live sessions included: a
/// session whose next event would take the total past `max_total_bytes`
/// gets one `truncated` event and nothing more. Long strings are cut.
#[test]
fn the_total_cap_holds_at_write_time_and_strings_are_cut() {
    let t = tempfile::tempdir().unwrap();
    let dir = t.path().join("sessions");
    std::fs::create_dir_all(&dir).unwrap();
    // Another live session already holds most of the room.
    std::fs::write(file_of(&dir, "s-1-1"), vec![b'x'; 9_000]).unwrap();
    let r = Record::new(
        dir.clone(),
        RecordLimits {
            max_total_bytes: 10_000,
            ..limits()
        },
    );
    for i in 0..50 {
        r.event(
            S,
            1000 + i,
            "hook",
            json!({ "i": i, "pad": "y".repeat(100) }),
        );
    }
    r.flush();
    let ev = r.read(S, 0, 1000).unwrap();
    let last = ev.last().unwrap();
    assert_eq!(last["type"], "truncated", "{ev:?}");
    assert!(last["detail"].as_str().unwrap().contains("max_total_mb"));
    assert_eq!(ev.iter().filter(|v| v["type"] == "truncated").count(), 1);
    let total: u64 = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|e| e.metadata().unwrap().len())
        .sum();
    assert!(total <= 10_000 + 200, "total {total}");
    // Strings are cut at MAX_FIELD_BYTES.
    let r2 = Record::new(t.path().join("other"), limits());
    r2.event(
        "s-2-2",
        1,
        "hook_prompt",
        json!({ "cwd": "/a".repeat(5_000), "nested": { "detail": ["z".repeat(9_000)] } }),
    );
    r2.flush();
    let ev = r2.read("s-2-2", 0, 10).unwrap();
    assert!(ev[0]["cwd"].as_str().unwrap().len() <= MAX_FIELD_BYTES + 3);
    assert!(ev[0]["nested"]["detail"][0].as_str().unwrap().len() <= MAX_FIELD_BYTES + 3);
}
