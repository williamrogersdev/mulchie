use super::*;

fn env(home: &Path, mac: bool, on_path: &'static [&'static str]) -> Env {
    Env {
        home: home.to_path_buf(),
        xdg_config: None,
        codex_home: None,
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

#[test]
fn cli_commands_are_exact() {
    let h = tempfile::tempdir().unwrap();
    let e = env(h.path(), true, &["claude", "codex"]);
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
    a.client = Some(McpClient::Codex);
    a.read_only = false;
    a.force = true;
    let out = install_in(&e, &a).unwrap();
    assert!(out.contains("codex mcp remove mulchie\n"), "{out}");
    assert!(
        out.contains("codex mcp add mulchie -- /opt/mulchie/bin/mulchie mcp"),
        "{out}"
    );
    let out = uninstall_in(
        &e,
        &McpUninstallArgs {
            client: Some(McpClient::ClaudeCode),
            dry_run: true,
        },
    )
    .unwrap();
    assert!(
        out.contains("claude mcp remove --scope user mulchie"),
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
    assert!(out.contains("would set mcpServers.mulchie"), "{out}");
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
