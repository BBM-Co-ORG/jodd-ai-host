//! `jodd-ai-mcp admin …`, driven through the built binary the way the
//! `jodd-ai-host` helper runs it: plain argv, no SSH_ORIGINAL_COMMAND.
//!
//! Every test runs the binary with HOME, the policy (`JODD_AI_MCP_POLICY`),
//! the slots dir, the call log and TMPDIR inside its own temp dir, so no test
//! touches the real ~/.ssh, /etc, /run or /var/log. Root is faked with the
//! debug-only `JODD_AI_MCP_FAKE_UID` (ruling R12), so no test needs root.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_jodd-ai-mcp");
const ROOT: &str = "0";
const ACCOUNT: &str = "1000";
const SELF: &str = "/usr/local/bin/jodd-ai-mcp";
const OTHER: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOtherOtherOther me@laptop";

struct Fx {
    dir: tempfile::TempDir,
}

impl Fx {
    fn new() -> Fx {
        let dir = tempfile::tempdir().unwrap();
        for d in ["home", "home/.ssh", "slots", "tmp", "bin"] {
            fs::create_dir(dir.path().join(d)).unwrap();
        }
        fs::set_permissions(
            dir.path().join("home/.ssh"),
            fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        Fx { dir }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    /// The policy lives one directory down, so `enable` can be seen creating it.
    fn policy(&self) -> PathBuf {
        self.path("etc/jodd-ai-host/policy.toml")
    }

    fn write_policy(&self, text: &str) {
        fs::create_dir_all(self.policy().parent().unwrap()).unwrap();
        fs::write(self.policy(), text).unwrap();
    }

    fn read_policy(&self) -> String {
        fs::read_to_string(self.policy()).unwrap()
    }

    fn keys(&self) -> PathBuf {
        self.path("home/.ssh/authorized_keys")
    }

    fn write_keys(&self, text: &str) {
        fs::write(self.keys(), text).unwrap();
        fs::set_permissions(self.keys(), fs::Permissions::from_mode(0o600)).unwrap();
    }

    /// An executable CLI stand-in.
    fn cli(&self, name: &str) -> String {
        let p = self.path(&format!("bin/{name}"));
        fs::write(&p, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        p.to_string_lossy().into_owned()
    }

    /// Legacy line exactly as Jodd's install.rs (commit d514d03d) writes it.
    fn legacy(&self, id: &str) -> String {
        format!(
            "restrict,command=\"{}/.jodd/ai-run-v2 --key {id}\" ssh-ed25519 \
             AAAAC3NzaC1lZDI1NTE5AAAAIDummyDummyDummyDummyDummyDummyDummyDu jodd-ai-{id}",
            self.path("home").display()
        )
    }

    /// `jodd-ai-mcp admin <args>` as `uid`.
    fn admin(&self, uid: &str, args: &[&str]) -> (i32, String, String) {
        self.run(uid, &["admin"], args, None)
    }

    fn run(
        &self,
        uid: &str,
        pre: &[&str],
        args: &[&str],
        original: Option<&str>,
    ) -> (i32, String, String) {
        let mut cmd = Command::new(BIN);
        cmd.args(pre)
            .args(args)
            .env_remove("SSH_ORIGINAL_COMMAND")
            .env("HOME", self.path("home"))
            .env("JODD_AI_MCP_POLICY", self.policy())
            .env("JODD_AI_MCP_SLOTS_DIR", self.path("slots"))
            .env("JODD_AI_MCP_LOG_FILE", self.path("calls.log"))
            .env("JODD_AI_MCP_FAKE_UID", uid)
            .env("JODD_AI_MCP_SELF_PATH", SELF)
            .env("TMPDIR", self.path("tmp"));
        if let Some(o) = original {
            cmd.env("SSH_ORIGINAL_COMMAND", o);
        }
        let out = cmd.output().unwrap();
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into(),
            String::from_utf8_lossy(&out.stderr).into(),
        )
    }
}

fn entry(enabled: bool, path: &str, path_env: &str, timeout: u64) -> String {
    format!(
        "enabled = {enabled}\npath = \"{path}\"\npath_env = \"{path_env}\"\ntimeout_secs = {timeout}\n"
    )
}

/// The harness table of the policy on disk, parsed.
fn harness(fx: &Fx, name: &str) -> Option<toml::Value> {
    let v: toml::Value = toml::from_str(&fx.read_policy()).unwrap();
    v.get("harness").and_then(|h| h.get(name)).cloned()
}

// ------------------------------------------------------------ the gate

#[test]
fn admin_is_refused_under_forced_command() {
    let fx = Fx::new();
    let cli = fx.cli("claude");
    // The restricted key's forced command: words from SSH_ORIGINAL_COMMAND.
    for original in [
        "admin list".to_string(),
        "admin adopt-keys".to_string(),
        format!("admin enable claude --path {cli} --path-env /usr/bin"),
    ] {
        let (code, out, err) = fx.run(ROOT, &["--key", "3f2a9b1c"], &[], Some(&original));
        assert_eq!((code, out.as_str()), (64, ""), "{original}: {err}");
        // Even with `admin` in the argv itself.
        let (code, _, err) = fx.run(ROOT, &["admin", "list"], &[], Some(&original));
        assert_eq!(code, 64, "{original}: {err}");
    }
    // A `--key` argv is the forced command too, even without the variable.
    let (code, _, err) = fx.run(ROOT, &["--key", "3f2a9b1c", "admin", "list"], &[], None);
    assert_eq!(code, 64, "{err}");
    assert!(!fx.policy().exists(), "nothing was written");
}

#[test]
fn usage_errors_exit_2_and_name_the_next_command() {
    let fx = Fx::new();
    let cli = fx.cli("claude");
    let cases: Vec<Vec<&str>> = vec![
        vec![],
        vec!["bogus"],
        vec!["list", "extra"],
        vec!["enable"],
        vec!["enable", "claude"],
        vec!["enable", "claude", "--path", &cli],
        vec!["enable", "claude", "--path-env", "/usr/bin"],
        vec!["enable", "pi", "--path", &cli, "--path-env", "/usr/bin"],
        vec![
            "enable",
            "claude",
            "--path",
            &cli,
            "--path-env",
            "/usr/bin",
            "--x",
            "1",
        ],
        vec![
            "enable",
            "claude",
            "--path",
            &cli,
            "--path",
            &cli,
            "--path-env",
            "/b",
        ],
        vec!["enable", "claude", "--path"],
        vec!["auto-enable", "codex"],
        vec!["disable"],
        vec!["disable", "pi"],
        vec!["disable", "claude", "extra"],
        vec!["adopt-keys", "extra"],
        vec!["scan-keys", "extra"],
    ];
    for args in cases {
        let (code, out, err) = fx.admin(ROOT, &args);
        assert_eq!((code, out.as_str()), (2, ""), "{args:?}: {err}");
        assert!(err.contains("jodd-ai-host"), "{args:?}: {err}");
    }
    assert!(!fx.policy().exists());
}

// ------------------------------------------------------------- policy

#[test]
fn enable_writes_the_entry_and_keeps_the_rest() {
    let fx = Fx::new();
    let claude = fx.cli("claude");
    // No policy and no directory yet: enable creates both.
    let (code, out, err) = fx.admin(
        ROOT,
        &[
            "enable",
            "claude",
            "--path",
            &claude,
            "--path-env",
            "/opt/bin:/usr/bin",
        ],
    );
    assert_eq!(code, 0, "{err}");
    assert!(
        out.contains("jodd-ai-host"),
        "names the next command: {out}"
    );
    let dir = fx.policy().parent().unwrap().to_path_buf();
    assert_eq!(
        fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
        0o755
    );
    assert_eq!(
        fs::metadata(fx.policy()).unwrap().permissions().mode() & 0o777,
        0o644
    );
    let h = harness(&fx, "claude").unwrap();
    assert_eq!(h["enabled"].as_bool(), Some(true));
    assert_eq!(h["path"].as_str(), Some(claude.as_str()));
    assert_eq!(h["path_env"].as_str(), Some("/opt/bin:/usr/bin"));
    assert_eq!(h["timeout_secs"].as_integer(), Some(120));
    assert!(harness(&fx, "codex").is_none(), "codex stays undecided");

    // Over an existing entry: the timeout and the other harness and limits stay.
    let codex = fx.cli("codex");
    fx.write_policy(&format!(
        "max_concurrent_runs = 5\n[harness.claude]\n{}[harness.codex]\n{}",
        entry(true, &claude, "/usr/bin", 120),
        entry(false, "/old/codex", "/old", 45)
    ));
    let (code, _, err) = fx.admin(
        ROOT,
        &["enable", "codex", "--path", &codex, "--path-env", "/new"],
    );
    assert_eq!(code, 0, "{err}");
    let v: toml::Value = toml::from_str(&fx.read_policy()).unwrap();
    assert_eq!(v["max_concurrent_runs"].as_integer(), Some(5));
    let h = harness(&fx, "codex").unwrap();
    assert_eq!(h["enabled"].as_bool(), Some(true));
    assert_eq!(h["path"].as_str(), Some(codex.as_str()));
    assert_eq!(h["path_env"].as_str(), Some("/new"));
    assert_eq!(h["timeout_secs"].as_integer(), Some(45));
    assert_eq!(
        harness(&fx, "claude").unwrap()["path"].as_str(),
        Some(claude.as_str())
    );
}

#[test]
fn enable_refuses_relative_or_non_executable_path() {
    let fx = Fx::new();
    let not_exec = fx.path("bin/claude-noexec");
    fs::write(&not_exec, "#!/bin/sh\n").unwrap();
    fs::set_permissions(&not_exec, fs::Permissions::from_mode(0o644)).unwrap();
    let not_exec = not_exec.to_string_lossy().into_owned();
    let missing = fx.path("bin/nope").to_string_lossy().into_owned();
    let a_dir = fx.path("bin").to_string_lossy().into_owned();
    let good = fx.cli("claude");
    let before = format!("[harness.claude]\n{}", entry(false, "/x", "/y", 99));
    fx.write_policy(&before);
    for (cmd, path, path_env) in [
        ("enable", "claude", "/usr/bin"),
        ("enable", "bin/claude", "/usr/bin"),
        ("enable", not_exec.as_str(), "/usr/bin"),
        ("enable", missing.as_str(), "/usr/bin"),
        ("enable", a_dir.as_str(), "/usr/bin"),
        ("enable", good.as_str(), ""),
        ("auto-enable", "claude", "/usr/bin"),
        ("auto-enable", not_exec.as_str(), "/usr/bin"),
    ] {
        // codex is undecided, so auto-enable would act on it.
        let name = if cmd == "auto-enable" {
            "codex"
        } else {
            "claude"
        };
        let (code, out, err) = fx.admin(ROOT, &[cmd, name, "--path", path, "--path-env", path_env]);
        assert_eq!((code, out.as_str()), (1, ""), "{cmd} {path:?} {path_env:?}");
        assert!(
            err.contains("jodd-ai-host"),
            "names the next command: {err}"
        );
        assert_eq!(fx.read_policy(), before, "{cmd} {path:?}");
    }
}

#[test]
fn policy_edits_need_root() {
    let fx = Fx::new();
    let cli = fx.cli("claude");
    for args in [
        vec!["enable", "claude", "--path", &cli, "--path-env", "/usr/bin"],
        vec![
            "auto-enable",
            "claude",
            "--path",
            &cli,
            "--path-env",
            "/usr/bin",
        ],
        vec!["disable", "claude"],
    ] {
        let (code, _, err) = fx.admin(ACCOUNT, &args);
        assert_eq!(code, 1, "{args:?}");
        assert!(
            err.contains(&format!("sudo jodd-ai-host {}", args[0])),
            "{err}"
        );
    }
    assert!(!fx.policy().exists());
}

#[test]
fn an_invalid_policy_is_never_overwritten() {
    let fx = Fx::new();
    let cli = fx.cli("claude");
    fx.write_policy("max_request_bytes = 'x'\n");
    for args in [
        vec!["enable", "claude", "--path", &cli, "--path-env", "/usr/bin"],
        vec![
            "auto-enable",
            "claude",
            "--path",
            &cli,
            "--path-env",
            "/usr/bin",
        ],
        vec!["disable", "claude"],
    ] {
        let (code, _, err) = fx.admin(ROOT, &args);
        assert_eq!(code, 1, "{args:?}");
        assert!(err.contains("jodd-ai-host doctor"), "{err}");
        assert_eq!(fx.read_policy(), "max_request_bytes = 'x'\n");
    }
}

#[test]
fn disable_creates_or_flips_the_entry() {
    let fx = Fx::new();
    let (code, out, err) = fx.admin(ROOT, &["disable", "codex"]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("jodd-ai-host"), "{out}");
    let h = harness(&fx, "codex").unwrap();
    assert_eq!(h["enabled"].as_bool(), Some(false));
    assert_eq!(h["timeout_secs"].as_integer(), Some(180));

    let claude = fx.cli("claude");
    fx.write_policy(&format!(
        "[harness.claude]\n{}",
        entry(true, &claude, "/p", 77)
    ));
    let (code, _, err) = fx.admin(ROOT, &["disable", "claude"]);
    assert_eq!(code, 0, "{err}");
    let h = harness(&fx, "claude").unwrap();
    assert_eq!(h["enabled"].as_bool(), Some(false));
    assert_eq!(h["path"].as_str(), Some(claude.as_str()));
    assert_eq!(h["path_env"].as_str(), Some("/p"));
    assert_eq!(h["timeout_secs"].as_integer(), Some(77));
}

#[test]
fn auto_enable_leaves_explicit_disable_alone() {
    let fx = Fx::new();
    let cli = fx.cli("claude");
    let other = fx.cli("claude2");
    for before in [
        format!("[harness.claude]\n{}", entry(false, &cli, "/usr/bin", 120)),
        format!("[harness.claude]\n{}", entry(true, &other, "/opt", 60)),
    ] {
        fx.write_policy(&before);
        let (code, out, err) = fx.admin(
            ROOT,
            &[
                "auto-enable",
                "claude",
                "--path",
                &cli,
                "--path-env",
                "/usr/bin",
            ],
        );
        assert_eq!(code, 0, "{err}");
        assert!(out.contains("jodd-ai-host"), "{out}");
        assert_eq!(fx.read_policy(), before, "a decision survives auto-enable");
    }
}

#[test]
fn auto_enable_fills_absent_entry() {
    let fx = Fx::new();
    let codex = fx.cli("codex");
    // No policy at all, no directory.
    let (code, _, err) = fx.admin(
        ROOT,
        &[
            "auto-enable",
            "codex",
            "--path",
            &codex,
            "--path-env",
            "/usr/bin:/bin",
        ],
    );
    assert_eq!(code, 0, "{err}");
    let h = harness(&fx, "codex").unwrap();
    assert_eq!(h["enabled"].as_bool(), Some(true));
    assert_eq!(h["path"].as_str(), Some(codex.as_str()));
    assert_eq!(h["path_env"].as_str(), Some("/usr/bin:/bin"));
    assert_eq!(h["timeout_secs"].as_integer(), Some(180));
    // Another harness's entry: the absent one is filled, the other kept.
    let claude = fx.cli("claude");
    let (code, _, err) = fx.admin(
        ROOT,
        &[
            "auto-enable",
            "claude",
            "--path",
            &claude,
            "--path-env",
            "/usr/bin",
        ],
    );
    assert_eq!(code, 0, "{err}");
    assert_eq!(
        harness(&fx, "claude").unwrap()["enabled"].as_bool(),
        Some(true)
    );
    assert_eq!(
        harness(&fx, "codex").unwrap()["path"].as_str(),
        Some(codex.as_str())
    );
}

#[test]
fn list_shows_each_harness_for_any_user() {
    let fx = Fx::new();
    let claude = fx.cli("claude");
    fx.write_policy(&format!(
        "[harness.claude]\n{}[harness.codex]\n{}",
        entry(true, &claude, "/usr/bin", 120),
        entry(false, "/x/codex", "/usr/bin", 180)
    ));
    let (code, out, err) = fx.admin(ACCOUNT, &["list"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(
        out,
        format!("claude  enabled  {claude}\ncodex  disabled  /x/codex\n")
    );

    fx.write_policy(&format!(
        "[harness.codex]\n{}",
        entry(true, "/x/codex", "/b", 180)
    ));
    let (code, out, _) = fx.admin(ROOT, &["list"]);
    assert_eq!(code, 0);
    assert_eq!(out, "claude  undecided  -\ncodex  enabled  /x/codex\n");
}

#[test]
fn list_with_a_missing_or_invalid_policy_names_doctor() {
    let fx = Fx::new();
    let (code, out, err) = fx.admin(ACCOUNT, &["list"]);
    assert_eq!(code, 0, "a missing policy is a state, not a fault: {err}");
    assert_eq!(out, "claude  undecided  -\ncodex  undecided  -\n");
    assert!(err.contains("jodd-ai-host doctor"), "{err}");

    fx.write_policy("[harness.claude]\nenabled = 'yes'\n");
    let (code, out, err) = fx.admin(ACCOUNT, &["list"]);
    assert_eq!(code, 1, "an invalid policy is a fault");
    assert_eq!(out, "claude  undecided  -\ncodex  undecided  -\n");
    assert!(err.contains("jodd-ai-host doctor"), "{err}");
}

// --------------------------------------------------------------- keys

#[test]
fn adopt_keys_refused_as_root() {
    let fx = Fx::new();
    let before = format!("{OTHER}\n{}\n", fx.legacy("11111111"));
    fx.write_keys(&before);
    // Each names the helper command that does the job as the account;
    // scan-keys is read-only, so that is `status`.
    for (cmd, next) in [
        ("adopt-keys", "jodd-ai-host adopt-keys"),
        ("scan-keys", "jodd-ai-host status"),
    ] {
        let (code, out, err) = fx.admin(ROOT, &[cmd]);
        assert_eq!((code, out.as_str()), (1, ""), "{cmd}");
        assert!(err.contains(next), "{cmd}: {err}");
    }
    let (_, _, err) = fx.admin(ROOT, &["scan-keys"]);
    assert!(!err.contains("adopt-keys"), "{err}");
    assert_eq!(fs::read_to_string(fx.keys()).unwrap(), before);
}

#[test]
fn adopt_keys_then_scan_keys() {
    let fx = Fx::new();
    let old = fx.legacy("11111111");
    fx.write_keys(&format!(
        "{OTHER}\n{old}\nssh-ed25519 AAAAx jodd-ai-22222222\n"
    ));

    let (code, out, err) = fx.admin(ACCOUNT, &["scan-keys"]);
    assert_eq!(code, 0, "{err}");
    let home = fx.path("home");
    assert_eq!(
        out,
        format!(
            "11111111  legacy  {}/.jodd/ai-run-v2 --key 11111111\n22222222  other  -\n",
            home.display()
        )
    );

    let (code, out, err) = fx.admin(ACCOUNT, &["adopt-keys"]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains('1') && out.contains("jodd-ai-host"), "{out}");
    let new = fs::read_to_string(fx.keys()).unwrap();
    assert_eq!(
        new,
        format!(
            "{OTHER}\nrestrict,command=\"{SELF} --key 11111111\" ssh-ed25519 \
             AAAAC3NzaC1lZDI1NTE5AAAAIDummyDummyDummyDummyDummyDummyDummyDu jodd-ai-11111111\n\
             ssh-ed25519 AAAAx jodd-ai-22222222\n"
        )
    );

    let (code, out, _) = fx.admin(ACCOUNT, &["scan-keys"]);
    assert_eq!(code, 0);
    assert_eq!(
        out,
        format!("11111111  mcp  {SELF} --key 11111111\n22222222  other  -\n")
    );
    // Re-runnable.
    let (code, _, err) = fx.admin(ACCOUNT, &["adopt-keys"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(fs::read_to_string(fx.keys()).unwrap(), new);
}

#[test]
fn adopt_keys_refuses_symlinked_file() {
    // Review Focus 3, through the binary: the target is untouched.
    let fx = Fx::new();
    let target = fx.path("shadow");
    let before = fx.legacy("11111111");
    fs::write(&target, &before).unwrap();
    std::os::unix::fs::symlink(&target, fx.keys()).unwrap();
    for cmd in ["adopt-keys", "scan-keys"] {
        let (code, out, err) = fx.admin(ACCOUNT, &[cmd]);
        assert_eq!((code, out.as_str()), (1, ""), "{cmd}");
        assert!(err.contains("symlink"), "{cmd}: {err}");
        assert!(err.contains("jodd-ai-host"), "{cmd}: {err}");
    }
    assert_eq!(fs::read_to_string(&target).unwrap(), before);
    assert!(fs::symlink_metadata(fx.keys())
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(fs::read_dir(fx.path("home/.ssh")).unwrap().count(), 1);
}

#[test]
fn keys_commands_need_an_absolute_home() {
    let fx = Fx::new();
    for home in ["", "relative"] {
        let out = Command::new(BIN)
            .args(["admin", "adopt-keys"])
            .env_remove("SSH_ORIGINAL_COMMAND")
            .env("HOME", home)
            .env("JODD_AI_MCP_POLICY", fx.policy())
            .env("JODD_AI_MCP_FAKE_UID", ACCOUNT)
            .env("TMPDIR", fx.path("tmp"))
            .current_dir(fx.path("home"))
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(1), "{home:?}");
        assert!(String::from_utf8_lossy(&out.stderr).contains("HOME"));
    }
}
