//! The legacy `jodd-ai-run 2 …` shim, driven through the built binary exactly
//! as sshd runs it. The bar is Jodd's wrapper v2 (`src-tauri/src/llm/remote/
//! wrapper.rs`, commit d514d03d): its `shell_tests` are ported here by name,
//! and every message and exit code is the wrapper's.
//!
//! Every test runs the binary with HOME, the policy (`JODD_AI_MCP_POLICY`),
//! the slots dir (`JODD_AI_MCP_SLOTS_DIR`) and TMPDIR inside its own temp dir,
//! so no test touches the real ~/.ssh, /etc or /run, and parallel tests never
//! share a slot or count each other's scratch dirs.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_jodd-ai-mcp");
const PATH_ENV: &str = "/usr/bin:/bin";

fn fake(name: &str) -> String {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fakes")
        .join(name)
        .to_string_lossy()
        .into_owned()
}

/// One harness entry in the test policy.
#[derive(Clone)]
struct H {
    enabled: bool,
    path: String,
    timeout: u64,
}

fn on(path: &str, timeout: u64) -> Option<H> {
    Some(H {
        enabled: true,
        path: path.into(),
        timeout,
    })
}

struct Fx {
    dir: tempfile::TempDir,
}

impl Fx {
    /// A policy with codex enabled on the fake (timeout 180, as shipped) and
    /// claude enabled on the fake too when `with_claude`.
    fn new(with_claude: bool) -> Fx {
        Fx::with(
            with_claude.then(|| on(&fake("fake-claude"), 120)).flatten(),
            on(&fake("fake-codex"), 180),
            "",
        )
    }

    /// `extra` is spliced in as top-level TOML (limits).
    fn with(claude: Option<H>, codex: Option<H>, extra: &str) -> Fx {
        let fx = Fx::empty();
        let mut text = String::from(extra);
        for (id, h) in [("claude", claude), ("codex", codex)] {
            if let Some(h) = h {
                text.push_str(&format!(
                    "\n[harness.{id}]\nenabled = {}\npath = \"{}\"\npath_env = \"{PATH_ENV}\"\ntimeout_secs = {}\n",
                    h.enabled, h.path, h.timeout
                ));
            }
        }
        std::fs::write(fx.policy(), text).unwrap();
        fx
    }

    /// No policy file at all.
    fn empty() -> Fx {
        let dir = tempfile::tempdir().unwrap();
        for d in ["home", "slots", "tmp"] {
            std::fs::create_dir(dir.path().join(d)).unwrap();
        }
        Fx { dir }
    }

    fn policy(&self) -> PathBuf {
        self.dir.path().join("policy.toml")
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    /// An executable script of the test's own, for a CLI the fakes cannot play.
    fn script(&self, name: &str, body: &str) -> String {
        let p = self.path(name);
        std::fs::write(&p, format!("#!/bin/sh\n{body}")).unwrap();
        Command::new("chmod").arg("755").arg(&p).status().unwrap();
        p.to_string_lossy().into_owned()
    }

    fn leftover_scratch(&self) -> usize {
        std::fs::read_dir(self.path("tmp"))
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with("jodd-ai.")
            })
            .count()
    }
}

/// `jodd-ai-run 2 <verb> [preset]`.
fn req(verb: &str, preset: &str) -> Vec<String> {
    let mut v = vec!["jodd-ai-run".to_string(), "2".to_string(), verb.to_string()];
    if !preset.is_empty() {
        v.push(preset.to_string());
    }
    v
}

/// Protocol v2 stdin: schema line, the prompt's length in bytes, the prompt.
fn v2(schema_line: &str, prompt: &str) -> Vec<u8> {
    format!("{schema_line}\n{}\n{prompt}", prompt.len()).into_bytes()
}

fn spawn(fx: &Fx, args: &[String], env: &[(&str, &str)]) -> Child {
    let mut cmd = Command::new(BIN);
    cmd.args(args)
        .env_remove("SSH_ORIGINAL_COMMAND")
        .env("HOME", fx.path("home"))
        .env("JODD_AI_MCP_POLICY", fx.policy())
        .env("JODD_AI_MCP_SLOTS_DIR", fx.path("slots"))
        .env("TMPDIR", fx.path("tmp"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.spawn().unwrap()
}

/// Collects the exit and output, killing a shim still running after 30 s (it
/// is waiting on a stdin that never ends): a test then fails on the code
/// instead of hanging the suite.
fn finish(mut child: Child) -> (i32, String, String) {
    let started = Instant::now();
    while child.try_wait().unwrap().is_none() {
        if started.elapsed() > Duration::from_secs(30) {
            let _ = child.kill();
            drop((child.stdout.take(), child.stderr.take()));
            let _ = child.wait();
            return (-2, String::new(), "HUNG: killed after 30s".into());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let out = child.wait_with_output().unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into(),
        String::from_utf8_lossy(&out.stderr).into(),
    )
}

/// Like Jodd: writes `stdin` and keeps the pipe OPEN until the shim has exited
/// (v2: stdin EOF means the client went away).
fn run_open(
    fx: &Fx,
    args: &[String],
    env: &[(&str, &str)],
    stdin: &[u8],
) -> ((i32, String, String), ChildStdin) {
    let mut child = spawn(fx, args, env);
    let mut sin = child.stdin.take().unwrap();
    // A refused request exits without reading: a failed write is fine.
    let _ = sin.write_all(stdin);
    (finish(child), sin)
}

fn run(fx: &Fx, args: &[String], env: &[(&str, &str)], stdin: &[u8]) -> (i32, String, String) {
    run_open(fx, args, env, stdin).0
}

/// Writes `stdin`, then closes it: the client went away right after.
fn run_then_eof(
    fx: &Fx,
    args: &[String],
    env: &[(&str, &str)],
    stdin: &[u8],
) -> (i32, String, String) {
    let mut child = spawn(fx, args, env);
    let _ = child.stdin.take().unwrap().write_all(stdin);
    finish(child)
}

/// Runs a request the way sshd does for the restricted key: the fixed argv
/// `--key <id>` and the client's words in SSH_ORIGINAL_COMMAND.
fn forced(fx: &Fx, key: Option<&str>, original: &str, stdin: &[u8]) -> (i32, String, String) {
    let args: Vec<String> = match key {
        Some(k) => vec!["--key".into(), k.into()],
        None => vec![],
    };
    run(fx, &args, &[("SSH_ORIGINAL_COMMAND", original)], stdin)
}

fn pid_alive(pid: &str) -> bool {
    Command::new("kill")
        .args(["-0", pid])
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .success()
}

/// True once the process whose pid the fake CLI recorded in `pidfile` is gone,
/// polling for up to `secs`.
fn fake_is_gone_within(pidfile: &Path, secs: u64) -> bool {
    let pid = std::fs::read_to_string(pidfile)
        .expect("the fake CLI must have recorded its pid")
        .trim()
        .to_string();
    assert!(!pid.is_empty(), "empty pidfile");
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(secs) {
        if !pid_alive(&pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

fn fake_is_gone(pidfile: &Path) -> bool {
    fake_is_gone_within(pidfile, 2)
}

fn wait_for_file(p: &Path) {
    for _ in 0..200 {
        if p.exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("{} never appeared", p.display());
}

fn wait_for_pidfile(p: &Path) {
    for _ in 0..200 {
        if std::fs::read_to_string(p)
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false)
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("{} never appeared", p.display());
}

fn s(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

// ---- ported from wrapper v2's shell_tests, by name -------------------------

#[test]
fn run_passes_the_prompt_through_byte_for_byte() {
    let fx = Fx::new(false);
    // Thai (multi-byte), a line that is just `-`, and a trailing newline.
    let prompt = "สวัสดี\n-\nline three\n";
    let (code, out, err) = run(&fx, &req("run", "codex"), &[], &v2("-", prompt));
    assert_eq!(code, 0, "stderr: {err}");
    assert_eq!(
        out,
        format!("PROMPT<{prompt}>SCHEMA<>FILES<0>"),
        "answer file, not the CLI's stdout noise; empty cwd"
    );
    assert_eq!(err, "codex stderr line\n", "the CLI's stderr only");
    assert_eq!(fx.leftover_scratch(), 0);
}

#[test]
fn an_empty_prompt_is_a_zero_length() {
    let fx = Fx::new(false);
    let (code, out, err) = run(&fx, &req("run", "codex"), &[], b"-\n0\n");
    assert_eq!(code, 0, "stderr: {err}");
    assert_eq!(out, "PROMPT<>SCHEMA<>FILES<0>");
}

#[test]
fn the_watcher_does_not_outlive_a_normal_run() {
    // After a normal exit nothing may still be reading the channel's stdin.
    let fx = Fx::new(false);
    let ((code, _, err), mut sin) = run_open(&fx, &req("run", "codex"), &[], &v2("-", "x"));
    assert_eq!(code, 0, "{err}");
    let mut broken = false;
    for _ in 0..40 {
        if sin.write_all(b"x").and_then(|_| sin.flush()).is_err() {
            broken = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        broken,
        "something still holds the shim's stdin open after it exited"
    );
}

#[test]
fn a_prompt_shorter_than_its_length_never_starts_the_cli() {
    let fx = Fx::new(false);
    let started = fx.path("started");
    let mut child = spawn(&fx, &req("run", "codex"), &[("FAKE_STARTED", &s(&started))]);
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"-\n100\nonly part of it")
        .unwrap(); // dropped: EOF
    let (code, out, err) = finish(child);
    assert_eq!(code, 70, "{err}");
    assert_eq!(
        err,
        "the prompt was incomplete: Jodd went away during the upload\n"
    );
    assert_eq!(out, "");
    assert!(
        !started.exists(),
        "the CLI must not start on a truncated prompt"
    );
    assert_eq!(fx.leftover_scratch(), 0);
}

#[test]
fn closing_stdin_mid_run_stops_the_cli_and_cleans_up() {
    let fx = Fx::new(false);
    let pidfile = fx.path("fake.pid");
    let mut child = spawn(
        &fx,
        &req("run", "codex"),
        &[("FAKE_SLEEP", "40"), ("FAKE_PIDFILE", &s(&pidfile))],
    );
    let mut sin = child.stdin.take().unwrap();
    sin.write_all(&v2("-", "secret prompt")).unwrap();
    wait_for_pidfile(&pidfile);
    let started = Instant::now();
    drop(sin);
    let (code, out, err) = finish(child);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "took {:?}",
        started.elapsed()
    );
    assert_eq!(code, 143, "{err}");
    assert_eq!(
        (out.as_str(), err.as_str()),
        ("", ""),
        "wrapper v2's TERM trap prints nothing"
    );
    assert!(fake_is_gone(&pidfile), "the CLI must be killed");
    assert_eq!(
        fx.leftover_scratch(),
        0,
        "the prompt must not outlive the call"
    );
}

#[test]
fn a_cli_that_ignores_term_is_killed_after_stop() {
    // Ruling R9: the shim waits for the delayed KILL before it exits (wrapper
    // v2 detached it instead), so only the CLI's fate is bounded here, not
    // how quickly the shim's exit status reaches a client that has gone.
    let fx = Fx::new(false);
    let pidfile = fx.path("fake.pid");
    let mut child = spawn(
        &fx,
        &req("run", "codex"),
        &[
            ("FAKE_SLEEP", "40"),
            ("FAKE_STUBBORN", "all"),
            ("FAKE_PIDFILE", &s(&pidfile)),
        ],
    );
    let mut sin = child.stdin.take().unwrap();
    sin.write_all(&v2("-", "x")).unwrap();
    wait_for_pidfile(&pidfile);
    drop(sin);
    let (code, _, err) = finish(child);
    assert_eq!(code, 143, "{err}");
    assert!(
        fake_is_gone_within(&pidfile, 8),
        "a CLI that ignores TERM outlived Stop"
    );
    assert_eq!(fx.leftover_scratch(), 0);
}

#[test]
fn a_length_over_nine_digits_is_refused() {
    let fx = Fx::new(false);
    let started = fx.path("started");
    let (code, _, err) = run(
        &fx,
        &req("run", "codex"),
        &[("FAKE_STARTED", &s(&started))],
        b"-\n1000000000\nx",
    );
    assert_eq!(code, 64, "{err}");
    assert_eq!(err, "unknown request\n");
    assert!(!started.exists());
    assert_eq!(fx.leftover_scratch(), 0);
}

#[test]
fn a_length_that_is_not_a_number_is_refused() {
    let fx = Fx::new(false);
    let started = fx.path("started");
    let env = [("FAKE_STARTED", &*s(&started))];
    for stdin in [
        &b"-\nabc\nxyz"[..],
        b"-\n-1\nx",
        b"-\n\nx",
        b"-\n 3\nabc",
        b"-\n3x\nabc",
    ] {
        let (code, _, err) = run(&fx, &req("run", "codex"), &env, stdin);
        assert_eq!(code, 64, "{:?}: {err}", String::from_utf8_lossy(stdin));
        assert_eq!(err, "unknown request\n");
    }
    // stdin that ends before the length line: the client went away.
    for stdin in [&b"-"[..], b"-\n", b"-\n12"] {
        let (code, _, err) = run_then_eof(&fx, &req("run", "codex"), &env, stdin);
        assert_eq!(code, 64, "{:?}: {err}", String::from_utf8_lossy(stdin));
    }
    assert!(!started.exists());
}

#[test]
fn a_schema_is_decoded_and_passed_with_the_schema_args() {
    use base64::Engine as _;
    let fx = Fx::new(false);
    let schema = r#"{"type":"object","additionalProperties":false}"#;
    let line = base64::engine::general_purpose::STANDARD.encode(schema);
    let (code, out, err) = run(&fx, &req("run", "codex"), &[], &v2(&line, "p"));
    assert_eq!(code, 0, "{err}");
    assert_eq!(out, format!("PROMPT<p>SCHEMA<{schema}>FILES<0>"));
}

#[test]
fn a_stdout_preset_passes_stdout_through_and_keeps_empty_args() {
    let fx = Fx::new(true);
    let (code, out, err) = run(&fx, &req("run", "claude"), &[], &v2("-", "hi"));
    assert_eq!(code, 0, "{err}");
    // Legacy returns the CLI's raw stdout (old Jodd unwraps the envelope
    // itself). claude's argv has 8 elements, two of them "" (--setting-sources
    // "" and --tools ""): the count proves they survived.
    assert_eq!(out, r#"{"result":"hi","args":8}"#);
}

#[test]
fn the_forced_command_path_reads_ssh_original_command() {
    let fx = Fx::new(false);
    let (code, out, err) = forced(
        &fx,
        Some("abcd1234"),
        "jodd-ai-run 2 run codex",
        &v2("-", "x"),
    );
    assert_eq!(code, 0, "{err}");
    assert!(out.starts_with("PROMPT<x>"), "{out}");
}

#[test]
fn anything_that_is_not_a_jodd_request_is_refused() {
    // stdin stays open and empty, as from Jodd: a refusal must not wait for a
    // prompt (it would hang until Jodd's deadline).
    let fx = Fx::new(true);
    for original in [
        "id",
        "sh -c id",
        "jodd-ai-run 2 run custom",
        "jodd-ai-run 2 exec codex",
        "jodd-ai-run 2 run",
        "jodd-ai-run",
        "",
        // The admin surface is never reachable through the restricted key.
        "admin keys list",
        // `--key` is read from the fixed argv only, never from the client.
        "--key abcd1234 jodd-ai-run 2 status",
        "--version",
    ] {
        let (code, out, err) = forced(&fx, Some("abcd1234"), original, b"");
        assert_eq!(code, 64, "request {original:?} must be refused: {err}");
        assert_eq!(err, "unknown request\n", "{original:?}");
        assert!(out.is_empty(), "{original:?}: {out}");
    }
    assert_eq!(fx.leftover_scratch(), 0);
}

#[test]
fn a_glob_in_the_request_is_not_expanded() {
    let fx = Fx::new(false);
    let (code, _, err) = forced(&fx, Some("abcd1234"), "jodd-ai-run 2 run *", b"");
    assert_eq!(code, 64, "{err}");
}

#[test]
fn a_version_mismatch_is_65_but_status_answers_any_version() {
    let fx = Fx::new(true);
    let old = |verb: &str, preset: &str| {
        let mut r = req(verb, preset);
        r[1] = "999".into();
        r
    };
    let (code, _, err) = run(&fx, &old("run", "codex"), &[], &v2("-", "x"));
    assert_eq!(code, 65);
    assert_eq!(err, "version mismatch: this server has 2, Jodd sent 999\n");
    // A v1 client (no length line) is refused by version before its stdin is read.
    let mut v1 = req("run", "codex");
    v1[1] = "1".into();
    let (code, _, err) = run(&fx, &v1, &[], b"-\nx");
    assert_eq!(code, 65, "{err}");
    let (code, out, _) = run(&fx, &old("status", ""), &[], b"");
    assert_eq!(code, 0);
    assert!(out.starts_with("version=2\n"), "{out}");
    assert!(out.contains("preset=claude path=/"), "{out}");
    assert!(out.contains("preset=codex path=/"), "{out}");
}

#[test]
fn a_missing_cli_is_127() {
    let fx = Fx::with(
        on("/nonexistent/jodd-test/claude", 120),
        on(&fake("fake-codex"), 180),
        "",
    );
    let (code, _, err) = run(&fx, &req("run", "claude"), &[], &v2("-", "x"));
    assert_eq!(code, 127);
    assert_eq!(err, "claude is not installed on this server\n");
    let (_, out, _) = run(&fx, &req("status", ""), &[], b"");
    assert!(out.contains("preset=claude path=-\n"), "{out}");
}

#[test]
fn a_failing_cli_reports_its_exit_and_its_stdout() {
    let fx = Fx::new(false);
    let (code, out, err) = run(
        &fx,
        &req("run", "codex"),
        &[("FAKE_EXIT", "3")],
        &v2("-", "x"),
    );
    assert_eq!(code, 3);
    assert_eq!(
        out, "noise on stdout\n",
        "on failure stdout is the CLI's own, for failure_detail"
    );
    assert_eq!(err, "codex stderr line\n");
}

#[test]
fn the_watchdog_kills_a_hung_cli_and_cleans_up() {
    let fx = Fx::with(None, on(&fake("fake-codex"), 2), "");
    let started = Instant::now();
    let pidfile = fx.path("fake.pid");
    let (code, out, err) = run(
        &fx,
        &req("run", "codex"),
        &[("FAKE_SLEEP", "40"), ("FAKE_PIDFILE", &s(&pidfile))],
        &v2("-", "x"),
    );
    assert_eq!(code, 124, "{err}");
    assert_eq!(err, "timed out after 2s\n");
    assert_eq!(out, "");
    assert!(started.elapsed() < Duration::from_secs(15));
    assert_eq!(fx.leftover_scratch(), 0);
    assert!(fake_is_gone(&pidfile), "the CLI must be killed too");
}

// ---- added for jodd-ai-host ------------------------------------------------

#[test]
fn a_timeout_replays_the_cli_stderr_before_its_own_line() {
    // Ruling R13: what the CLI said before the TERM, then the shim's message.
    let fx = Fx::empty();
    let cli = fx.script(
        "cli",
        "cat >/dev/null\necho 'warming up' >&2\nexec sleep 40\n",
    );
    std::fs::write(
        fx.policy(),
        format!("[harness.codex]\nenabled = true\npath = \"{cli}\"\npath_env = \"{PATH_ENV}\"\ntimeout_secs = 1\n"),
    )
    .unwrap();
    let (code, _, err) = run(&fx, &req("run", "codex"), &[], &v2("-", "x"));
    assert_eq!(code, 124, "{err}");
    assert_eq!(err, "warming up\ntimed out after 1s\n");
}

#[test]
fn status_with_bad_policy_reports_dashes() {
    // Review Focus 2: a hand-edited bad policy enables nothing.
    let golden = "version=2\npreset=claude path=-\npreset=codex path=-\n";
    let bad = Fx::empty();
    std::fs::write(bad.policy(), "max_request_bytes = 'x'\n").unwrap();
    let missing = Fx::empty();
    for fx in [&bad, &missing] {
        let (code, out, err) = run(fx, &req("status", ""), &[], b"");
        assert_eq!((code, out.as_str(), err.as_str()), (0, golden, ""));
        let (code, _, err) = run(fx, &req("run", "codex"), &[], b"");
        assert_eq!((code, err.as_str()), (64, "unknown request\n"));
    }
}

#[test]
fn disabled_preset_is_64_before_reading_stdin() {
    // stdin stays open and EMPTY: a refusal that waited for the prompt would
    // hang until the 30 s guard in `finish`.
    let started = tempfile::tempdir().unwrap();
    let marker = started.path().join("started");
    let fx = Fx::with(
        None, // claude: undecided
        Some(H {
            enabled: false,
            path: fake("fake-codex"),
            timeout: 180,
        }),
        "",
    );
    for preset in ["codex", "claude"] {
        let t = Instant::now();
        let (code, out, err) = run(
            &fx,
            &req("run", preset),
            &[("FAKE_STARTED", &s(&marker))],
            b"",
        );
        assert_eq!(code, 64, "{preset}: {err}");
        assert_eq!((out.as_str(), err.as_str()), ("", "unknown request\n"));
        assert!(
            t.elapsed() < Duration::from_secs(5),
            "{preset}: waited {:?}",
            t.elapsed()
        );
    }
    assert!(!marker.exists());
    // ...and status does not report a disabled preset's path.
    let (_, out, _) = run(&fx, &req("status", ""), &[], b"");
    assert_eq!(
        out,
        "version=2\npreset=claude path=-\npreset=codex path=-\n"
    );
}

#[test]
fn a_length_over_max_request_bytes_is_refused() {
    // The policy's limit binds legacy too: a client cannot raise it.
    let fx = Fx::with(
        None,
        on(&fake("fake-codex"), 180),
        "max_request_bytes = 10\n",
    );
    let started = fx.path("started");
    let env = [("FAKE_STARTED", &*s(&started))];
    let (code, _, err) = run(&fx, &req("run", "codex"), &env, &v2("-", "01234567890"));
    assert_eq!((code, err.as_str()), (64, "unknown request\n"));
    assert!(!started.exists());
    let (code, out, err) = run(&fx, &req("run", "codex"), &env, &v2("-", "0123456789"));
    assert_eq!(code, 0, "{err}");
    assert_eq!(out, "PROMPT<0123456789>SCHEMA<>FILES<0>");
}

#[test]
fn status_output_is_byte_identical_to_wrapper_v2() {
    let fx = Fx::new(true);
    let (claude, codex) = (fake("fake-claude"), fake("fake-codex"));
    // The golden text comes from wrapper v2's own status lines, run by `sh`:
    // `echo "version=$JODD_AI_RUN_VERSION"`, then `generate`'s
    // `printf 'preset=%s path=%s\n' <id> <path>` per REMOTE_PRESETS entry.
    let template = "JODD_AI_RUN_VERSION=2\n\
                    echo \"version=$JODD_AI_RUN_VERSION\"\n\
                    \x20 printf 'preset=%s path=%s\\n' 'claude' \"$1\"\n\
                    \x20 printf 'preset=%s path=%s\\n' 'codex' \"$2\"\n\
                    \x20 exit 0\n";
    let golden = Command::new("sh")
        .arg("-c")
        .arg(template)
        .arg("sh")
        .arg(&claude)
        .arg(&codex)
        .output()
        .unwrap();
    let golden = String::from_utf8(golden.stdout).unwrap();
    assert_eq!(
        golden,
        format!("version=2\npreset=claude path={claude}\npreset=codex path={codex}\n")
    );

    for args in [req("status", ""), {
        let mut r = req("status", "");
        r[1] = "7".into();
        r
    }] {
        let (code, out, err) = run(&fx, &args, &[], b"");
        assert_eq!((code, err.as_str()), (0, ""));
        assert_eq!(out, golden, "byte-for-byte");
    }
    let (code, out, _) = forced(&fx, Some("3f2a9b1c"), "jodd-ai-run 2 status", b"");
    assert_eq!((code, out.as_str()), (0, golden.as_str()));
}

#[test]
fn third_concurrent_legacy_run_is_busy() {
    // A CLI that hangs until its own release file appears, so the test decides
    // when each run ends. Configured as claude: a stdout preset, so its
    // `done` is the answer.
    let fx = Fx::empty();
    let cli = fx.script(
        "cli",
        "cat >/dev/null\n: > \"$T_STARTED\"\nwhile [ ! -e \"$T_RELEASE\" ]; do sleep 0.05; done\nprintf done\n",
    );
    std::fs::write(
        fx.policy(),
        format!(
            "max_concurrent_runs = 2\n[harness.claude]\nenabled = true\npath = \"{cli}\"\npath_env = \"{PATH_ENV}\"\ntimeout_secs = 60\n"
        ),
    )
    .unwrap();
    let start = |name: &str| -> (Child, ChildStdin, PathBuf, PathBuf) {
        let (st, rel) = (
            fx.path(&format!("{name}.started")),
            fx.path(&format!("{name}.release")),
        );
        let mut c = spawn(
            &fx,
            &req("run", "claude"),
            &[("T_STARTED", &s(&st)), ("T_RELEASE", &s(&rel))],
        );
        let mut sin = c.stdin.take().unwrap();
        let _ = sin.write_all(&v2("-", name));
        (c, sin, st, rel)
    };

    let (a, a_in, a_st, a_rel) = start("a");
    let (b, b_in, b_st, b_rel) = start("b");
    wait_for_file(&a_st);
    wait_for_file(&b_st);

    // The third is refused at once instead of queueing, and never starts.
    let t = Instant::now();
    let (c, c_in, c_st, _) = start("c");
    let (code, out, err) = finish(c);
    drop(c_in);
    assert_eq!(code, 70, "{err}");
    assert_eq!((out.as_str(), err.as_str()), ("", "the AI host is busy\n"));
    assert!(
        t.elapsed() < Duration::from_secs(5),
        "waited {:?}",
        t.elapsed()
    );
    assert!(!c_st.exists(), "a busy refusal must not start the CLI");

    // One of the first two finishes: its slot comes back.
    std::fs::write(&a_rel, "").unwrap();
    let (code, out, err) = finish(a);
    drop(a_in);
    assert_eq!((code, out.as_str()), (0, "done"), "{err}");

    let (d, d_in, d_st, d_rel) = start("d");
    wait_for_file(&d_st);
    std::fs::write(&d_rel, "").unwrap();
    let (code, out, err) = finish(d);
    drop(d_in);
    assert_eq!((code, out.as_str()), (0, "done"), "{err}");

    std::fs::write(&b_rel, "").unwrap();
    let (code, _, err) = finish(b);
    drop(b_in);
    assert_eq!(code, 0, "{err}");
    assert_eq!(fx.leftover_scratch(), 0);
}

#[test]
fn forget_needs_an_eight_hex_key_id() {
    let fx = Fx::new(false);
    for key in [
        None,
        Some("ABCD1234"),
        Some("abcd123"),
        Some("abcd12345"),
        Some("abcd123g"),
        Some(""),
    ] {
        let (code, out, err) = forced(&fx, key, "jodd-ai-run 2 forget", b"");
        assert_eq!(
            (code, out.as_str(), err.as_str()),
            (64, "", "no key id\n"),
            "{key:?}"
        );
    }
    // Any version: forget, like status, is answered before the version check.
    for original in ["jodd-ai-run 2 forget", "jodd-ai-run 9 forget"] {
        let (code, out, err) = forced(&fx, Some("3f2a9b1c"), original, b"");
        assert_eq!(
            (code, out.as_str(), err.as_str()),
            (0, "", ""),
            "{original}"
        );
    }
}

#[test]
fn a_term_to_the_shim_mid_run_stops_the_cli() {
    // wrapper v2: `trap 'stop; exit 143' TERM HUP INT`.
    let fx = Fx::new(false);
    let pidfile = fx.path("fake.pid");
    let mut child = spawn(
        &fx,
        &req("run", "codex"),
        &[("FAKE_SLEEP", "40"), ("FAKE_PIDFILE", &s(&pidfile))],
    );
    let mut sin = child.stdin.take().unwrap();
    sin.write_all(&v2("-", "x")).unwrap();
    wait_for_pidfile(&pidfile);
    Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .unwrap();
    let (code, _, err) = finish(child);
    drop(sin);
    assert_eq!(code, 143, "{err}");
    assert!(fake_is_gone(&pidfile), "the CLI must be killed");
    assert_eq!(fx.leftover_scratch(), 0);
}

#[test]
fn admin_is_only_for_a_shell_without_a_forced_command() {
    let fx = Fx::new(false);
    let (code, _, err) = run(&fx, &["admin".into(), "keys".into()], &[], b"");
    assert_eq!((code, err.as_str()), (1, "admin: not implemented\n"));
    let (code, _, err) = forced(&fx, None, "admin keys", b"");
    assert_eq!((code, err.as_str()), (64, "unknown request\n"));
}
