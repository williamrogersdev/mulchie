//! Install and uninstall round trips on temp files only.

use super::*;
use HookClient::*;

struct T {
    _d: tempfile::TempDir,
    env: HookEnv,
}

fn t() -> T {
    let d = tempfile::tempdir().unwrap();
    let home = d.path().canonicalize().unwrap().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let env = HookEnv {
        home: home.clone(),
        claude_config: None,
        codex_home: None,
        copilot_home: None,
        exe: PathBuf::from("/opt/my tools/bin/mulchie"),
    };
    T { _d: d, env }
}

fn read(p: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}

const EXE: &str = "'/opt/my tools/bin/mulchie'";

#[test]
fn the_files_and_entries_each_client_gets() {
    let t = t();
    let h = &t.env.home;
    for (c, file) in [
        (ClaudeCode, h.join(".claude/settings.json")),
        (Codex, h.join(".codex/hooks.json")),
        (Cursor, h.join(".cursor/hooks.json")),
        (Gemini, h.join(".gemini/settings.json")),
        (Copilot, h.join(".copilot/hooks/mulchie.json")),
    ] {
        assert_eq!(t.env.file(c), file);
        let msg = install_in(&t.env, c, false, None).unwrap();
        assert!(msg.contains("add "), "{msg}");
        let v = read(&file);
        let names: Vec<&String> = v["hooks"].as_object().unwrap().keys().collect();
        let want: Vec<&str> = events_for(c).iter().map(|s| s.native).collect();
        assert_eq!(names, want, "{c:?}");
        // Installing again changes nothing.
        let again = install_in(&t.env, c, false, None).unwrap();
        assert!(again.contains("already set up"), "{again}");
    }
    let claude = read(&h.join(".claude/settings.json"));
    assert_eq!(
        claude["hooks"]["PreToolUse"],
        json!([{ "matcher": "Bash", "hooks": [{ "type": "command",
            "command": format!("{EXE} hook claude-code pre_tool"), "timeout": 5 }] }])
    );
    assert_eq!(
        claude["hooks"]["SessionStart"],
        json!([{ "hooks": [{ "type": "command", "command": format!("{EXE} hook claude-code session_start"), "timeout": 5 }] }])
    );
    let codex = read(&h.join(".codex/hooks.json"));
    assert_eq!(codex["hooks"]["SessionEnd"][0]["hooks"][0]["timeout"], 2);
    let cursor = read(&h.join(".cursor/hooks.json"));
    assert_eq!(cursor["version"], 1);
    assert_eq!(
        cursor["hooks"]["beforeShellExecution"],
        json!([{ "command": format!("{EXE} hook cursor pre_tool"), "timeout": 5 }])
    );
    let gemini = read(&h.join(".gemini/settings.json"));
    assert_eq!(
        gemini["hooks"]["BeforeTool"],
        json!([{ "matcher": "run_shell_command", "hooks": [{ "name": "mulchie", "type": "command",
            "command": format!("{EXE} hook gemini pre_tool"), "timeout": 5000 }] }])
    );
    let copilot = read(&h.join(".copilot/hooks/mulchie.json"));
    assert_eq!(copilot["version"], 1);
    assert_eq!(
        copilot["hooks"]["preToolUse"],
        json!([{ "type": "command", "bash": format!("{EXE} hook copilot pre_tool"), "timeoutSec": 5 }])
    );
    // The commands run `mulchie hook` with the words they were written with.
    let w = sh_words(&format!("{EXE} hook copilot pre_tool --guard warn"));
    assert_eq!(
        w,
        [
            "/opt/my tools/bin/mulchie",
            "hook",
            "copilot",
            "pre_tool",
            "--guard",
            "warn"
        ]
    );
    // Each client's notes for the user.
    let t2 = self::t();
    let msg = install_in(&t2.env, Codex, false, None).unwrap();
    assert!(msg.contains("/hooks") && msg.contains("trust"), "{msg}");
}

#[test]
fn user_hooks_and_other_keys_are_kept_and_restored_exactly() {
    let t = t();
    let f = t.env.file(ClaudeCode);
    std::fs::create_dir_all(f.parent().unwrap()).unwrap();
    let original = r#"{
  "model": "opus",
  "permissions": { "allow": ["Bash(git *)"] },
  "hooks": {
    "PreToolUse": [
      { "matcher": "Bash", "hooks": [{ "type": "command", "command": "~/bin/my-guard.sh" }] }
    ],
    "Notification": [
      { "hooks": [{ "type": "command", "command": "say done" }] }
    ]
  },
  "zz_last": true
}
"#;
    std::fs::write(&f, original).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o640)).unwrap();
    install_in(&t.env, ClaudeCode, false, Some(GuardMode::Warn)).unwrap();
    let v = read(&f);
    let keys: Vec<&String> = v.as_object().unwrap().keys().collect();
    assert_eq!(
        keys,
        ["model", "permissions", "hooks", "zz_last"],
        "key order kept"
    );
    let pre = v["hooks"]["PreToolUse"].as_array().unwrap();
    assert_eq!(pre.len(), 2);
    assert_eq!(
        pre[0]["hooks"][0]["command"], "~/bin/my-guard.sh",
        "user hook first and untouched"
    );
    assert_eq!(
        pre[1]["hooks"][0]["command"],
        format!("{EXE} hook claude-code pre_tool --guard warn")
    );
    assert_eq!(
        v["hooks"]["Notification"][0]["hooks"][0]["command"],
        "say done"
    );
    assert_eq!(
        std::fs::metadata(&f).unwrap().permissions().mode() & 0o777,
        0o640
    );
    // A backup of the original sits next to it.
    let backups: Vec<_> = std::fs::read_dir(f.parent().unwrap())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("settings.json.mulchie-backup-")
        })
        .collect();
    assert_eq!(backups.len(), 1);
    assert_eq!(
        std::fs::read_to_string(backups[0].path()).unwrap(),
        original
    );

    // A changed guard updates the entry in place.
    let msg = install_in(&t.env, ClaudeCode, false, Some(GuardMode::Block)).unwrap();
    assert!(msg.contains("update PreToolUse"), "{msg}");
    assert_eq!(read(&f)["hooks"]["PreToolUse"].as_array().unwrap().len(), 2);

    // Uninstall leaves exactly what was there.
    uninstall_in(&t.env, ClaudeCode, false).unwrap();
    let back = read(&f);
    let orig: Value = serde_json::from_str(original).unwrap();
    assert_eq!(back, orig);
    let msg = uninstall_in(&t.env, ClaudeCode, false).unwrap();
    assert!(msg.contains("not set up"), "{msg}");
}

#[test]
fn a_user_hook_sharing_a_group_with_ours_is_kept() {
    let t = t();
    let f = t.env.file(Gemini);
    std::fs::create_dir_all(f.parent().unwrap()).unwrap();
    let ours = format!("{EXE} hook gemini pre_tool");
    let old = json!({ "hooks": { "BeforeTool": [{ "matcher": "run_shell_command", "hooks": [
        { "name": "mine", "type": "command", "command": "./check.sh" },
        { "name": "mulchie", "type": "command", "command": "/old/path/mulchie hook gemini pre_tool", "timeout": 5000 },
        { "name": "mulchie", "type": "command", "command": "mulchie hook gemini pre_tool", "timeout": 5000 }
    ] }] } });
    std::fs::write(&f, serde_json::to_string_pretty(&old).unwrap()).unwrap();
    let msg = install_in(&t.env, Gemini, false, None).unwrap();
    assert!(
        msg.contains("update BeforeTool") && msg.contains("duplicate"),
        "{msg}"
    );
    let hs = read(&f)["hooks"]["BeforeTool"][0]["hooks"].clone();
    assert_eq!(hs.as_array().unwrap().len(), 2, "{hs}");
    assert_eq!(hs[0]["command"], "./check.sh");
    assert_eq!(hs[1]["command"], ours);
    uninstall_in(&t.env, Gemini, false).unwrap();
    let v = read(&f);
    assert_eq!(
        v["hooks"]["BeforeTool"],
        json!([{ "matcher": "run_shell_command", "hooks": [{ "name": "mine", "type": "command", "command": "./check.sh" }] }])
    );
    assert!(v["hooks"].get("SessionStart").is_none());
    // Another client's mulchie hooks are not ours to remove.
    assert!(!is_ours(
        &json!({ "command": "mulchie hook cursor pre_tool" }),
        Gemini
    ));
    assert!(!is_ours(
        &json!({ "command": "/bin/notmulchie hook gemini x" }),
        Gemini
    ));
    assert!(!is_ours(
        &json!({ "command": "echo mulchie hook gemini" }),
        Gemini
    ));
}

#[test]
fn a_symlinked_file_is_edited_through_the_link() {
    let t = t();
    let real = t.env.home.join("dotfiles/cursor-hooks.json");
    std::fs::create_dir_all(real.parent().unwrap()).unwrap();
    std::fs::write(
        &real,
        r#"{"version":1,"hooks":{"stop":[{"command":"./audit.sh"}]}}"#,
    )
    .unwrap();
    let link = t.env.file(Cursor);
    std::fs::create_dir_all(link.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let msg = install_in(&t.env, Cursor, false, None).unwrap();
    assert!(msg.contains("a link to"), "{msg}");
    assert!(std::fs::symlink_metadata(&link)
        .unwrap()
        .file_type()
        .is_symlink());
    let v = read(&real);
    assert_eq!(v["hooks"]["stop"].as_array().unwrap().len(), 2);
    assert_eq!(v["hooks"]["stop"][0]["command"], "./audit.sh");
    uninstall_in(&t.env, Cursor, false).unwrap();
    assert_eq!(
        read(&real),
        json!({"version":1,"hooks":{"stop":[{"command":"./audit.sh"}]}})
    );
    // A dangling link is refused.
    std::fs::remove_file(&real).unwrap();
    let e = install_in(&t.env, Cursor, false, None).unwrap_err();
    assert!(e.contains("does not exist"), "{e}");
}

#[test]
fn malformed_files_are_refused_and_left_alone() {
    let t = t();
    for (c, text) in [
        (ClaudeCode, "{ \"hooks\": { oops"),
        (Codex, "[1, 2]"),
        (Cursor, r#"{"version":1,"hooks":[]}"#),
        (
            Copilot,
            r#"{"version":1,"hooks":{"preToolUse":{"bash":"x"}}}"#,
        ),
    ] {
        let f = t.env.file(c);
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(&f, text).unwrap();
        let e = install_in(&t.env, c, false, None).unwrap_err();
        assert!(e.contains("left as it is"), "{c:?}: {e}");
        if c != Copilot {
            assert!(uninstall_in(&t.env, c, false).is_err() || c == Copilot);
        }
        assert_eq!(std::fs::read_to_string(&f).unwrap(), text, "{c:?}");
        let n = std::fs::read_dir(f.parent().unwrap()).unwrap().count();
        assert_eq!(n, 1, "no backup or temp file left for {c:?}");
    }
}

#[test]
fn dry_run_prints_the_exact_diff_and_writes_nothing() {
    let t = t();
    let f = t.env.file(Copilot);
    let msg = install_in(&t.env, Copilot, true, None).unwrap();
    assert!(!f.exists());
    assert!(
        msg.contains("Would change") && msg.contains("Nothing was changed"),
        "{msg}"
    );
    assert!(
        msg.contains(&format!(
            "+        \"bash\": \"{EXE} hook copilot pre_tool\","
        )),
        "{msg}"
    );
    install_in(&t.env, Copilot, false, None).unwrap();
    let before = std::fs::read_to_string(&f).unwrap();
    let msg = uninstall_in(&t.env, Copilot, true).unwrap();
    assert_eq!(std::fs::read_to_string(&f).unwrap(), before);
    assert!(msg.contains("-        \"bash\""), "{msg}");
    assert!(
        !msg.lines()
            .any(|l| l.starts_with('+') && !l.starts_with("+++") && l.contains("bash")),
        "{msg}"
    );
}

#[test]
fn diff_shows_changes_with_context() {
    let d = diff(
        "a\nb\nc\nd\ne\nf\ng\n",
        "a\nb\nc\nX\ne\nf\ng\n",
        Path::new("/f"),
    );
    assert_eq!(d, "--- /f\n+++ /f\n b\n c\n-d\n+X\n e\n f\n");
    let d = diff("", "x\n", Path::new("/f"));
    assert!(d.ends_with("+x\n"));
}

#[test]
fn status_lists_each_client() {
    let t = t();
    install_in(&t.env, Cursor, false, None).unwrap();
    let s = status_in(&t.env);
    let cursor = s
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["client"] == "cursor")
        .unwrap();
    assert_eq!(cursor["events"].as_array().unwrap().len(), 6);
    assert_eq!(cursor["runs_this_binary"], true);
    assert_eq!(cursor["runs"], json!(["/opt/my tools/bin/mulchie"]));
    let claude = s
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["client"] == "claude-code")
        .unwrap();
    assert_eq!(claude["events"], json!([]));
    assert_eq!(s.as_array().unwrap().len(), 5);
    let text = render_status(&s);
    assert!(
        text.contains("cursor") && text.contains("not installed"),
        "{text}"
    );
}
