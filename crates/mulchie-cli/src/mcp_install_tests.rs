use super::*;

fn env(home: &Path, mac: bool, on_path: &'static [&'static str]) -> Env {
    Env {
        home: home.to_path_buf(),
        xdg_config: None,
        codex_home: None,
        claude_config: None,
        exe: PathBuf::from("/opt/mulchie/bin/mulchie"),
        mac,
        which: Box::new(move |n: &str| {
            on_path
                .contains(&n)
                .then(|| PathBuf::from(format!("/usr/local/bin/{n}")))
        }),
    }
}

fn install(client: McpClient) -> McpInstallArgs {
    McpInstallArgs {
        client: Some(client),
        ..Default::default()
    }
}

#[test]
fn methods_and_paths() {
    let h = tempfile::tempdir().unwrap();
    let mac = env(h.path(), true, &[]);
    assert_eq!(
        method(McpClient::ClaudeDesktop, &mac),
        Method::Json {
            path: h
                .path()
                .join("Library/Application Support/Claude/claude_desktop_config.json"),
            typed: false
        }
    );
    let linux = env(h.path(), false, &[]);
    assert_eq!(
        method(McpClient::ClaudeDesktop, &linux),
        Method::Json {
            path: h.path().join(".config/Claude/claude_desktop_config.json"),
            typed: false
        }
    );
    assert_eq!(
        method(McpClient::Cursor, &mac),
        Method::Json {
            path: h.path().join(".cursor/mcp.json"),
            typed: true
        }
    );
    assert_eq!(
        method(McpClient::Codex, &mac),
        Method::Toml {
            path: h.path().join(".codex/config.toml")
        }
    );
    assert!(matches!(
        method(McpClient::ClaudeCode, &mac),
        Method::Manual(_)
    ));
    let with_clis = env(h.path(), true, &["claude", "codex"]);
    assert!(matches!(
        method(McpClient::ClaudeCode, &with_clis),
        Method::Cli { .. }
    ));
    assert!(matches!(
        method(McpClient::Codex, &with_clis),
        Method::Cli { .. }
    ));
}

/// Stand-in `claude` and `codex` scripts in `<home>/bin` that log their
/// arguments to `<home>/log`. `claude mcp add` fails while `<home>/fail-add`
/// exists; `codex mcp get` prints `<home>/codex-entry` when it exists.
fn stub_env(home: &Path) -> Env {
    use std::os::unix::fs::PermissionsExt;
    let bin = home.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let h = home.display();
    let claude = format!(
        "#!/bin/sh\necho \"claude $*\" >> '{h}/log'\nif [ \"$2\" = add ] && [ -e '{h}/fail-add' ]; then echo 'add broke' >&2; exit 1; fi\nexit 0\n"
    );
    let codex = format!(
        "#!/bin/sh\necho \"codex $*\" >> '{h}/log'\nif [ \"$2\" = get ]; then\n  if [ -e '{h}/codex-entry' ]; then cat '{h}/codex-entry'; exit 0; fi\n  echo \"Error: No MCP server named 'mulchie' found.\" >&2; exit 1\nfi\nexit 0\n"
    );
    for (n, body) in [("claude", claude), ("codex", codex)] {
        let p = bin.join(n);
        std::fs::write(&p, body).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let mut e = env(home, true, &[]);
    e.which = Box::new(move |n: &str| Some(bin.join(n)).filter(|p| p.exists()));
    e
}

fn log(home: &Path) -> String {
    std::fs::read_to_string(home.join("log")).unwrap_or_default()
}

#[test]
fn cli_commands_are_exact() {
    let h = tempfile::tempdir().unwrap();
    let e = stub_env(h.path());
    let mut a = install(McpClient::ClaudeCode);
    a.dry_run = true;
    a.read_only = true;
    let out = install_in(&e, &a).unwrap();
    assert!(
        out.contains(
            "claude mcp add --scope user mulchie -- /opt/mulchie/bin/mulchie mcp --read-only"
        ),
        "{out}"
    );
    assert!(!out.contains("remove"), "nothing to replace: {out}");
    a.client = Some(McpClient::Codex);
    a.read_only = false;
    let out = install_in(&e, &a).unwrap();
    assert!(
        out.contains("codex mcp add mulchie -- /opt/mulchie/bin/mulchie mcp"),
        "{out}"
    );
    // Dry runs only looked.
    assert!(!log(h.path()).contains(" add"), "{}", log(h.path()));
    let out = uninstall_in(
        &e,
        &McpUninstallArgs {
            client: Some(McpClient::ClaudeCode),
            dry_run: true,
        },
    )
    .unwrap();
    assert!(out.contains("not set up"), "{out}");
}

#[test]
fn claude_force_restores_the_old_entry_when_add_fails() {
    let h = tempfile::tempdir().unwrap();
    let e = stub_env(h.path());
    let old = json!({ "type": "stdio", "command": "/old/mulchie", "args": ["mcp"], "env": { "TOKEN": "sekrit" } });
    std::fs::write(
        h.path().join(".claude.json"),
        json!({ "mcpServers": { "mulchie": old } }).to_string(),
    )
    .unwrap();
    // Without --force: refused, and the secret is not shown.
    let err = install_in(&e, &install(McpClient::ClaudeCode)).unwrap_err();
    assert!(
        err.contains("--force") && err.contains("/old/mulchie"),
        "{err}"
    );
    assert!(
        err.contains("TOKEN=***") && !err.contains("sekrit"),
        "{err}"
    );
    assert!(log(h.path()).is_empty());

    // --force with a failing add: removed, add failed, put back.
    std::fs::write(h.path().join("fail-add"), "").unwrap();
    let mut a = install(McpClient::ClaudeCode);
    a.force = true;
    let err = install_in(&e, &a).unwrap_err();
    assert!(
        err.contains("removed the previous") && err.contains("add broke"),
        "{err}"
    );
    assert!(err.contains("put back"), "{err}");
    assert!(
        !err.contains("pass --force") && !err.contains("sekrit"),
        "{err}"
    );
    let l = log(h.path());
    let lines: Vec<&str> = l.lines().collect();
    assert!(
        lines[0].starts_with("claude mcp remove --scope user mulchie"),
        "{l}"
    );
    assert!(
        lines[1].starts_with("claude mcp add --scope user mulchie --"),
        "{l}"
    );
    assert!(
        lines[2].starts_with("claude mcp add-json --scope user mulchie {"),
        "{l}"
    );
    assert!(
        lines[2].contains("sekrit"),
        "the restore carries the full entry: {l}"
    );
    // The removed entry was kept, readable only by the user.
    let saved: Vec<_> = std::fs::read_dir(h.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .contains("mulchie-replaced-entry")
        })
        .collect();
    assert_eq!(saved.len(), 1);
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        saved[0].metadata().unwrap().permissions().mode() & 0o777,
        0o600
    );

    // --force with a working add: remove then add, both reported.
    std::fs::remove_file(h.path().join("fail-add")).unwrap();
    let out = install_in(&e, &a).unwrap();
    assert!(
        out.contains("removed the previous") && out.contains("ran `claude mcp add"),
        "{out}"
    );
}

#[test]
fn codex_never_replaces_silently() {
    let h = tempfile::tempdir().unwrap();
    let e = stub_env(h.path());
    std::fs::write(
        h.path().join("codex-entry"),
        json!({ "name": "mulchie", "transport": { "type": "stdio", "command": "/other", "args": ["x"], "env": { "K": "secret" } } }).to_string(),
    )
    .unwrap();
    let err = install_in(&e, &install(McpClient::Codex)).unwrap_err();
    assert!(
        err.contains("--force") && err.contains("/other") && !err.contains("secret"),
        "{err}"
    );
    assert!(!log(h.path()).contains("mcp add"), "{}", log(h.path()));
    let mut a = install(McpClient::Codex);
    a.force = true;
    install_in(&e, &a).unwrap();
    // `codex mcp add` replaces in place: no remove first.
    let l = log(h.path());
    assert!(
        !l.contains("mcp remove") && l.contains("codex mcp add mulchie --"),
        "{l}"
    );
    // Already set up: nothing run.
    std::fs::write(
        h.path().join("codex-entry"),
        json!({ "transport": { "command": "/opt/mulchie/bin/mulchie", "args": ["mcp"] } })
            .to_string(),
    )
    .unwrap();
    std::fs::remove_file(h.path().join("log")).unwrap();
    assert!(install_in(&e, &install(McpClient::Codex))
        .unwrap()
        .contains("already set up"));
    assert!(!log(h.path()).contains("mcp add"));
}

#[test]
fn symlinked_config_is_edited_at_its_target() {
    use std::os::unix::fs::PermissionsExt;
    let h = tempfile::tempdir().unwrap();
    let e = env(h.path(), true, &[]);
    let dot = h.path().join("dotfiles");
    std::fs::create_dir_all(&dot).unwrap();
    std::fs::create_dir_all(h.path().join(".cursor")).unwrap();
    let real = dot.join("mcp.json");
    std::fs::write(
        &real,
        r#"{"zeta":1,"mcpServers":{"other":{"command":"x","env":{"API_KEY":"sekrit"}}},"alpha":2}"#,
    )
    .unwrap();
    std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o640)).unwrap();
    let link = h.path().join(".cursor/mcp.json");
    std::os::unix::fs::symlink(&real, &link).unwrap();

    let mut a = install(McpClient::Cursor);
    a.dry_run = true;
    let out = install_in(&e, &a).unwrap();
    assert!(
        !out.contains("sekrit") && !out.contains("other"),
        "dry run shows only ours: {out}"
    );
    assert!(out.contains("mcpServers.mulchie"), "{out}");

    a.dry_run = false;
    install_in(&e, &a).unwrap();
    assert!(
        std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink(),
        "the link is kept"
    );
    assert_eq!(
        std::fs::metadata(&real).unwrap().permissions().mode() & 0o777,
        0o640
    );
    let text = std::fs::read_to_string(&real).unwrap();
    // The app's own key order is kept.
    let (z, m, al) = (
        text.find("zeta").unwrap(),
        text.find("mcpServers").unwrap(),
        text.find("alpha").unwrap(),
    );
    assert!(z < m && m < al, "{text}");
    assert!(text.find("\"other\"").unwrap() < text.find("\"mulchie\"").unwrap());
    assert!(text.contains("sekrit"));

    // Uninstall: through the link too, and the output masks nothing of others.
    let out = uninstall_in(
        &e,
        &McpUninstallArgs {
            client: Some(McpClient::Cursor),
            dry_run: false,
        },
    )
    .unwrap();
    assert!(!out.contains("sekrit"), "{out}");
    assert!(std::fs::symlink_metadata(&link)
        .unwrap()
        .file_type()
        .is_symlink());
    assert!(!std::fs::read_to_string(&real).unwrap().contains("mulchie"));

    // A dangling link is refused and left alone.
    std::fs::remove_file(&real).unwrap();
    let err = install_in(&e, &install(McpClient::Cursor)).unwrap_err();
    assert!(err.contains("does not exist"), "{err}");
    assert!(std::fs::symlink_metadata(&link)
        .unwrap()
        .file_type()
        .is_symlink());
    assert!(!real.exists());
}

#[test]
fn a_file_changed_meanwhile_is_not_overwritten() {
    let h = tempfile::tempdir().unwrap();
    let p = h.path().join("c.json");
    std::fs::write(&p, "{\"a\":2}").unwrap();
    let err = write_checked(&p, "{}", 0o600, Some("{\"a\":1}")).unwrap_err();
    assert!(err.contains("changed while"), "{err}");
    assert_eq!(std::fs::read_to_string(&p).unwrap(), "{\"a\":2}");
    let leftovers = std::fs::read_dir(h.path()).unwrap().count();
    assert_eq!(leftovers, 1, "the temp file is removed");
    write_checked(&p, "{}", 0o600, Some("{\"a\":2}")).unwrap();
    assert_eq!(std::fs::read_to_string(&p).unwrap(), "{}");
}

#[test]
fn refusals_mask_secrets_of_an_existing_entry() {
    let h = tempfile::tempdir().unwrap();
    let e = env(h.path(), true, &[]);
    let path = h.path().join(".cursor/mcp.json");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        &path,
        r#"{"mcpServers":{"mulchie":{"command":"/old","args":["mcp"],"env":{"T":"sekrit"}}}}"#,
    )
    .unwrap();
    let err = install_in(&e, &install(McpClient::Cursor)).unwrap_err();
    assert!(
        err.contains("/old") && err.contains("T=***") && !err.contains("sekrit"),
        "{err}"
    );
    let out = uninstall_in(
        &e,
        &McpUninstallArgs {
            client: Some(McpClient::Cursor),
            dry_run: true,
        },
    )
    .unwrap();
    assert!(
        out.contains("would remove") && !out.contains("sekrit"),
        "{out}"
    );
}

#[test]
fn json_install_keeps_other_entries_and_backs_up() {
    let h = tempfile::tempdir().unwrap();
    let e = env(h.path(), true, &[]);
    let path = h.path().join(".cursor/mcp.json");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let before = r#"{"mcpServers":{"other":{"command":"x","args":["y"]}},"theme":"dark"}"#;
    std::fs::write(&path, before).unwrap();

    // Dry run changes nothing.
    let mut a = install(McpClient::Cursor);
    a.dry_run = true;
    let out = install_in(&e, &a).unwrap();
    assert!(out.contains("would add mcpServers.mulchie"), "{out}");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), before);

    a.dry_run = false;
    let out = install_in(&e, &a).unwrap();
    assert!(out.contains("old file saved as"), "{out}");
    let v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(v["mcpServers"]["other"]["command"], "x");
    assert_eq!(v["theme"], "dark");
    assert_eq!(
        v["mcpServers"]["mulchie"],
        json!({ "type": "stdio", "command": "/opt/mulchie/bin/mulchie", "args": ["mcp"] })
    );
    let backups: Vec<_> = std::fs::read_dir(path.parent().unwrap())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().contains("mulchie-backup"))
        .collect();
    assert_eq!(backups.len(), 1);
    assert_eq!(std::fs::read_to_string(backups[0].path()).unwrap(), before);

    // Again: nothing to do.
    assert!(install_in(&e, &a).unwrap().contains("already set up"));

    // A different entry needs --force.
    a.read_only = true;
    let err = install_in(&e, &a).unwrap_err();
    assert!(err.contains("--force"), "{err}");
    a.force = true;
    install_in(&e, &a).unwrap();
    let v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(
        v["mcpServers"]["mulchie"]["args"],
        json!(["mcp", "--read-only"])
    );

    // Uninstall removes only ours.
    let u = McpUninstallArgs {
        client: Some(McpClient::Cursor),
        dry_run: false,
    };
    uninstall_in(&e, &u).unwrap();
    let v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert!(v["mcpServers"].get("mulchie").is_none());
    assert_eq!(v["mcpServers"]["other"]["command"], "x");
    assert!(uninstall_in(&e, &u).unwrap().contains("not set up"));
}

#[test]
fn json_new_file_and_refusals() {
    let h = tempfile::tempdir().unwrap();
    let e = env(h.path(), false, &[]);
    let path = h.path().join(".config/Claude/claude_desktop_config.json");
    install_in(&e, &install(McpClient::ClaudeDesktop)).unwrap();
    let v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(
        v,
        json!({ "mcpServers": { "mulchie": { "command": "/opt/mulchie/bin/mulchie", "args": ["mcp"] } } })
    );
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );

    for bad in ["{not json", "[1,2]", r#"{"mcpServers": 3}"#] {
        std::fs::write(&path, bad).unwrap();
        let err = install_in(&e, &install(McpClient::ClaudeDesktop)).unwrap_err();
        assert!(err.contains("left as it is"), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), bad);
    }
}

#[test]
fn toml_install_keeps_comments_and_other_servers() {
    let h = tempfile::tempdir().unwrap();
    let e = env(h.path(), true, &[]);
    let path = h.path().join(".codex/config.toml");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let before = "# my settings\nmodel = \"o3\"\n\n[mcp_servers.other]\ncommand = \"x\"\n";
    std::fs::write(&path, before).unwrap();
    install_in(&e, &install(McpClient::Codex)).unwrap();
    let after = std::fs::read_to_string(&path).unwrap();
    assert!(
        after.starts_with("# my settings\nmodel = \"o3\"\n"),
        "{after}"
    );
    assert!(
        after.contains("[mcp_servers.other]\ncommand = \"x\""),
        "{after}"
    );
    assert!(
        after.contains(
            "[mcp_servers.mulchie]\ncommand = \"/opt/mulchie/bin/mulchie\"\nargs = [\"mcp\"]"
        ),
        "{after}"
    );
    assert!(install_in(&e, &install(McpClient::Codex))
        .unwrap()
        .contains("already set up"));
    let mut ro = install(McpClient::Codex);
    ro.read_only = true;
    assert!(install_in(&e, &ro).unwrap_err().contains("--force"));

    uninstall_in(
        &e,
        &McpUninstallArgs {
            client: Some(McpClient::Codex),
            dry_run: false,
        },
    )
    .unwrap();
    let after = std::fs::read_to_string(&path).unwrap();
    assert!(!after.contains("mulchie"), "{after}");
    assert!(after.contains("[mcp_servers.other]"), "{after}");

    std::fs::write(&path, "model = = 3").unwrap();
    assert!(install_in(&e, &install(McpClient::Codex))
        .unwrap_err()
        .contains("not valid TOML"));
}

#[test]
fn overview_lists_every_client_and_changes_nothing() {
    let h = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(h.path().join(".cursor")).unwrap();
    let e = env(h.path(), true, &["claude"]);
    let out = install_in(&e, &McpInstallArgs::default()).unwrap();
    for c in ALL {
        assert!(out.contains(label(c)), "{out}");
    }
    assert!(out.contains("cursor          found"), "{out}");
    assert!(out.contains("claude-desktop  not found"), "{out}");
    assert!(!h.path().join(".cursor/mcp.json").exists());
    assert!(uninstall_in(&e, &McpUninstallArgs::default()).is_err());
}

#[test]
fn snippet_shape() {
    let h = tempfile::tempdir().unwrap();
    let e = env(h.path(), true, &[]);
    assert_eq!(
        snippet(&e, true),
        json!({ "mcpServers": { "mulchie": { "command": "/opt/mulchie/bin/mulchie", "args": ["mcp", "--read-only"] } } })
    );
}

#[test]
fn a_json_config_that_would_change_on_rewrite_is_refused() {
    let want = json!({ "command": "/bin/mulchie", "args": ["mcp"] });
    let p = Path::new("/tmp/x.json");
    let e = edit_json(
        Some(r#"{"n": 12345678901234567890123}"#),
        p,
        Some(&want),
        false,
    )
    .unwrap_err();
    assert!(
        e.contains("left as it is") && e.contains("12345678901234567890123"),
        "{e}"
    );
    let e = edit_json(
        Some(r#"{"mcpServers": {}, "mcpServers": {}}"#),
        p,
        Some(&want),
        false,
    )
    .unwrap_err();
    assert!(e.contains("twice"), "{e}");
}
