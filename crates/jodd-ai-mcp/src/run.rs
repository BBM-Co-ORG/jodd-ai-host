//! One Claude Code / Codex run, bounded and cancellable. Wrapper v2 (Jodd
//! `src-tauri/src/llm/remote/wrapper.rs`, commit d514d03d) is the behavioural bar:
//! a fresh private scratch dir per run (prompt, schema, out, an empty `work/`
//! cwd), the CLI in its own process group, TERM on timeout or cancel and KILL
//! 5 s later even if the CLI ignores TERM, and the scratch removed however the
//! run ends.
//!
//! `run` returns `Cancelled` / `TimedOut` as soon as the TERM is sent; the
//! rest (the grace, the KILL, the scratch removal) is a "reap" task that
//! outlives the call, like wrapper v2's detached `reap`. A process about to
//! exit must `drain_reaps().await` first, or a CLI that ignores TERM survives
//! it (ruling R9).

use std::ffi::OsStr;
use std::fs::{self, File};
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{ExitStatus, Stdio};
use std::sync::Mutex;
use std::time::Duration;

use nix::sys::signal::{killpg, Signal};
use nix::unistd::{access, AccessFlags, Pid};
use tempfile::TempDir;
use tokio::process::{Child, Command};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::harness::{Arg, HarnessDef};
use crate::policy::HarnessPolicy;

/// How long a TERMed group gets before the KILL (wrapper v2: `sleep 5`).
const KILL_GRACE: Duration = Duration::from_secs(5);
/// After the KILL: how long to wait for the group to be gone before the
/// scratch is removed anyway (a KILLed process can linger as a zombie until
/// init reaps it).
const GONE_WAIT: Duration = Duration::from_secs(2);
const POLL: Duration = Duration::from_millis(20);

pub struct RunRequest<'a> {
    pub def: &'static HarnessDef,
    pub cfg: &'a HarnessPolicy,
    pub stdin: Vec<u8>,
    pub schema: Option<String>,
    pub timeout: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunOutcome {
    Exited {
        code: i32,
        stdout: String,
        stderr: String,
        out_file: Option<String>,
    },
    TimedOut {
        stderr: String,
    },
    Cancelled,
    /// `cfg.path` is not an executable file.
    Missing,
}

/// Run one CLI to completion, timeout or cancel. Scratch lives under
/// `std::env::temp_dir()` (so `TMPDIR` is honoured).
pub async fn run(req: RunRequest<'_>, cancel: CancellationToken) -> io::Result<RunOutcome> {
    run_in(req, cancel, &std::env::temp_dir()).await
}

/// Wait for every pending reap: each TERMed run's KILL and scratch removal.
/// Bounded by `KILL_GRACE + GONE_WAIT` (plus a little) per reap.
pub async fn drain_reaps() {
    loop {
        let pending = std::mem::take(&mut *reaps());
        if pending.is_empty() {
            return;
        }
        for h in pending {
            let _ = h.await;
        }
    }
}

static REAPS: Mutex<Vec<JoinHandle<()>>> = Mutex::new(Vec::new());

fn reaps() -> std::sync::MutexGuard<'static, Vec<JoinHandle<()>>> {
    REAPS.lock().unwrap_or_else(|e| e.into_inner())
}

/// `run`, with the scratch parent chosen by the caller (tests use their own,
/// so parallel tests never count each other's scratch dirs).
async fn run_in(
    req: RunRequest<'_>,
    cancel: CancellationToken,
    scratch_parent: &Path,
) -> io::Result<RunOutcome> {
    if !is_runnable(&req.cfg.path) {
        return Ok(RunOutcome::Missing);
    }

    let scratch = tempfile::Builder::new()
        .prefix("jodd-ai.")
        .tempdir_in(scratch_parent)?;
    let d = scratch.path();
    fs::set_permissions(d, fs::Permissions::from_mode(0o700))?;
    fs::write(d.join("prompt"), &req.stdin)?;
    if let Some(schema) = &req.schema {
        fs::write(d.join("schema"), schema)?;
    }
    fs::create_dir(d.join("work"))?;

    let out = d.join("out");
    let schema_file = d.join("schema");
    let argv = req
        .def
        .args
        .iter()
        .chain(if req.schema.is_some() {
            req.def.schema_args
        } else {
            &[]
        })
        .map(|a| match a {
            Arg::Lit(s) => OsStr::new(s),
            Arg::OutFile => out.as_os_str(),
            Arg::SchemaFile => schema_file.as_os_str(),
        });

    let mut cmd = Command::new(&req.cfg.path);
    cmd.args(argv)
        // Wrapper v2 parity (ruling R11): inherit the server's environment
        // (HOME and the CLIs' own variables among it), override only PATH.
        .env("PATH", &req.cfg.path_env)
        .current_dir(d.join("work"))
        .stdin(Stdio::from(File::open(d.join("prompt"))?))
        // Files, not pipes, like wrapper v2: a grandchild that keeps a pipe
        // open cannot hold the run hostage, and nothing needs draining.
        .stdout(Stdio::from(File::create(d.join("stdout"))?))
        .stderr(Stdio::from(File::create(d.join("stderr"))?))
        .process_group(0);
    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied
            ) =>
        {
            return Ok(RunOutcome::Missing)
        }
        Err(e) => return Err(e),
    };
    let pgid = Pid::from_raw(
        child
            .id()
            .ok_or_else(|| io::Error::other("child has no pid"))? as i32,
    );
    // From here on, however this future ends (even dropped), the group is
    // TERMed and reaped unless it exited on its own.
    let mut live = Live {
        child: Some(child),
        pgid,
        scratch: Some(scratch),
    };

    enum End {
        Exited(ExitStatus),
        TimedOut,
        Cancelled,
    }
    let end = {
        let child = live.child.as_mut().expect("live child");
        tokio::select! {
            biased;
            st = child.wait() => End::Exited(st?),
            _ = cancel.cancelled() => End::Cancelled,
            _ = tokio::time::sleep(req.timeout) => End::TimedOut,
        }
    };

    let d = live
        .scratch
        .as_ref()
        .expect("live scratch")
        .path()
        .to_path_buf();
    match end {
        End::Exited(status) => {
            live.child = None; // reaped by wait(); nothing to TERM
            let out_file = match fs::read(d.join("out")) {
                Ok(b) => Some(lossy(b)),
                Err(e) if e.kind() == io::ErrorKind::NotFound => None,
                Err(e) => return Err(e),
            };
            Ok(RunOutcome::Exited {
                code: exit_code(status),
                stdout: lossy(fs::read(d.join("stdout"))?),
                stderr: lossy(fs::read(d.join("stderr"))?),
                out_file,
            })
            // `live` drops here: the scratch goes with it.
        }
        End::Cancelled => {
            live.abandon();
            Ok(RunOutcome::Cancelled)
        }
        End::TimedOut => {
            // What the CLI said so far; `abandon` TERMs it and the reap
            // removes the file, so read it first.
            let stderr = lossy(fs::read(d.join("stderr")).unwrap_or_default());
            live.abandon();
            Ok(RunOutcome::TimedOut { stderr })
        }
    }
}

/// `[ -x "$bin" ]`, but stricter: absolute, a regular file (after symlinks),
/// executable by us. A relative path is never looked up on any PATH.
pub(crate) fn is_runnable(path: &str) -> bool {
    let p = Path::new(path);
    p.is_absolute()
        && fs::metadata(p).map(|m| m.is_file()).unwrap_or(false)
        && access(p, AccessFlags::X_OK).is_ok()
}

fn lossy(b: Vec<u8>) -> String {
    String::from_utf8(b).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

/// The shell's convention, as wrapper v2's `wait` reported it: 128+n for a signal.
fn exit_code(st: ExitStatus) -> i32 {
    st.code().unwrap_or_else(|| 128 + st.signal().unwrap_or(0))
}

/// True while any process is left in the group (one we may signal).
fn group_alive(pgid: Pid) -> bool {
    killpg(pgid, None).is_ok()
}

/// A started CLI that has not been reaped yet.
struct Live {
    child: Option<Child>,
    pgid: Pid,
    scratch: Option<TempDir>,
}

impl Live {
    /// TERM the group and hand the rest to a reap task; returns at once.
    fn abandon(&mut self) {
        let (Some(child), Some(scratch)) = (self.child.take(), self.scratch.take()) else {
            return;
        };
        let _ = killpg(self.pgid, Signal::SIGTERM);
        let guard = ReapGuard {
            pgid: self.pgid,
            armed: true,
            _scratch: scratch,
        };
        match tokio::runtime::Handle::try_current() {
            Ok(h) => {
                let task = h.spawn(reap(child, guard));
                let mut all = reaps();
                all.retain(|t| !t.is_finished());
                all.push(task);
            }
            // No runtime to wait in: KILL now (the guard does) and remove the scratch.
            Err(_) => drop((child, guard)),
        }
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        self.abandon();
    }
}

/// Owns the scratch dir of a TERMed run. Dropped while still armed (the reap
/// was cut short, e.g. the runtime shut down) it KILLs the group at once;
/// either way the scratch goes with it.
struct ReapGuard {
    pgid: Pid,
    armed: bool,
    _scratch: TempDir,
}

impl Drop for ReapGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = killpg(self.pgid, Signal::SIGKILL);
        }
    }
}

/// Wrapper v2's `reap`, for a group that has already been sent TERM: give it
/// `KILL_GRACE`, KILL whatever is left, then remove the scratch.
async fn reap(mut child: Child, mut guard: ReapGuard) {
    let deadline = Instant::now() + KILL_GRACE;
    // The leader first (a zombie leader would keep `group_alive` true), then
    // whatever else of the group heeds the TERM within the grace.
    let _ = tokio::time::timeout_at(deadline, child.wait()).await;
    while group_alive(guard.pgid) && Instant::now() < deadline {
        tokio::time::sleep(POLL).await;
    }
    if group_alive(guard.pgid) {
        let _ = killpg(guard.pgid, Signal::SIGKILL);
    }
    let _ = tokio::time::timeout(GONE_WAIT, child.wait()).await;
    let gone_by = Instant::now() + GONE_WAIT;
    while group_alive(guard.pgid) && Instant::now() < gone_by {
        tokio::time::sleep(POLL).await;
    }
    guard.armed = false;
    // `guard` drops here and removes the scratch.
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::harness::def;
    use crate::policy::HarnessId;
    use nix::sys::signal::kill;
    use nix::unistd::Pid;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Instant;

    const PATH_ENV: &str = "/usr/bin:/bin";

    fn fakes() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fakes")
    }

    /// A test's private directory: `scratch/` is the scratch parent the run
    /// under test uses (so parallel tests cannot see each other's scratch
    /// dirs), the rest holds launchers, pid files and markers.
    struct Fx {
        dir: tempfile::TempDir,
    }

    impl Fx {
        fn new() -> Fx {
            let dir = tempfile::tempdir().unwrap();
            std::fs::create_dir(dir.path().join("scratch")).unwrap();
            Fx { dir }
        }

        fn scratch_parent(&self) -> PathBuf {
            self.dir.path().join("scratch")
        }

        fn path(&self, name: &str) -> PathBuf {
            self.dir.path().join(name)
        }

        /// An executable script with `body`, as the harness's `cfg`.
        fn script(&self, body: &str) -> HarnessPolicy {
            static N: AtomicUsize = AtomicUsize::new(0);
            let p = self
                .dir
                .path()
                .join(format!("cli-{}", N.fetch_add(1, Ordering::SeqCst)));
            std::fs::write(&p, format!("#!/bin/sh\n{body}")).unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
            cfg(&p.to_string_lossy())
        }

        /// The fake CLI behind a launcher that sets `env`: a test's switches
        /// reach its own fake without touching the process-global environment
        /// that parallel tests share.
        fn fake(&self, name: &str, env: &[(&str, &str)]) -> HarnessPolicy {
            let mut body = String::new();
            for (k, v) in env {
                body.push_str(&format!("{k}='{v}'; export {k}\n"));
            }
            body.push_str(&format!(
                "exec '{}' \"$@\"\n",
                fakes().join(name).to_string_lossy()
            ));
            self.script(&body)
        }

        fn leftover_scratch(&self) -> usize {
            std::fs::read_dir(self.scratch_parent())
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

        async fn scratch_gone_within(&self, secs: u64) -> bool {
            let started = Instant::now();
            while started.elapsed() < Duration::from_secs(secs) {
                if self.leftover_scratch() == 0 {
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            false
        }
    }

    fn cfg(path: &str) -> HarnessPolicy {
        HarnessPolicy {
            enabled: true,
            path: path.into(),
            path_env: PATH_ENV.into(),
            timeout_secs: 60,
        }
    }

    fn req<'a>(id: HarnessId, cfg: &'a HarnessPolicy, prompt: &str) -> RunRequest<'a> {
        RunRequest {
            def: def(id),
            cfg,
            stdin: prompt.as_bytes().to_vec(),
            schema: None,
            timeout: Duration::from_secs(30),
        }
    }

    /// Runs with the test's own scratch parent; a hang fails the test instead
    /// of the suite.
    async fn go(fx: &Fx, req: RunRequest<'_>, cancel: CancellationToken) -> RunOutcome {
        tokio::time::timeout(
            Duration::from_secs(40),
            run_in(req, cancel, &fx.scratch_parent()),
        )
        .await
        .expect("run hung")
        .expect("run failed")
    }

    fn exited(o: RunOutcome) -> (i32, String, String, Option<String>) {
        match o {
            RunOutcome::Exited {
                code,
                stdout,
                stderr,
                out_file,
            } => (code, stdout, stderr, out_file),
            other => panic!("expected Exited, got {other:?}"),
        }
    }

    fn alive(pid: i32) -> bool {
        kill(Pid::from_raw(pid), None).is_ok()
    }

    async fn wait_for_pid(pidfile: &Path) -> i32 {
        for _ in 0..200 {
            if let Ok(s) = std::fs::read_to_string(pidfile) {
                if let Ok(pid) = s.trim().parse() {
                    return pid;
                }
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("{} never appeared", pidfile.display());
    }

    async fn gone_within(pid: i32, secs: u64) -> bool {
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(secs) {
            if !alive(pid) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        false
    }

    /// Cancels `cancel` once the fake has recorded its pid; returns the pid.
    fn cancel_when_started(
        pidfile: PathBuf,
        cancel: CancellationToken,
    ) -> tokio::task::JoinHandle<i32> {
        tokio::spawn(async move {
            let pid = wait_for_pid(&pidfile).await;
            cancel.cancel();
            pid
        })
    }

    #[tokio::test]
    async fn passes_prompt_bytes_through() {
        let fx = Fx::new();
        let cfg = fx.fake("fake-codex", &[]);
        // Thai (multi-byte), a line that is just `-`, and a trailing newline.
        let prompt = "สวัสดี\n-\nline three\n";
        let (code, _, err, out) = exited(
            go(
                &fx,
                req(HarnessId::Codex, &cfg, prompt),
                CancellationToken::new(),
            )
            .await,
        );
        assert_eq!(code, 0, "stderr: {err}");
        assert_eq!(
            out.as_deref(),
            Some(&*format!("PROMPT<{prompt}>SCHEMA<>FILES<0>")),
            "empty cwd"
        );
    }

    #[tokio::test]
    async fn codex_answer_comes_from_out_file() {
        let fx = Fx::new();
        let codex = fx.fake("fake-codex", &[]);
        let (code, stdout, _, out) = exited(
            go(
                &fx,
                req(HarnessId::Codex, &codex, "hi"),
                CancellationToken::new(),
            )
            .await,
        );
        assert_eq!(code, 0);
        assert_eq!(stdout, "noise on stdout\n");
        assert_eq!(out.as_deref(), Some("PROMPT<hi>SCHEMA<>FILES<0>"));

        // Claude answers on stdout and writes no file.
        let claude = fx.fake("fake-claude", &[]);
        let (code, stdout, _, out) = exited(
            go(
                &fx,
                req(HarnessId::Claude, &claude, "hi"),
                CancellationToken::new(),
            )
            .await,
        );
        assert_eq!(code, 0);
        assert_eq!(stdout, r#"{"result":"hi","args":8}"#);
        assert_eq!(out, None);
    }

    #[tokio::test]
    async fn schema_args_only_when_schema_given() {
        let fx = Fx::new();
        let codex = fx.fake("fake-codex", &[]);
        let schema = r#"{"type":"object"}"#;
        let mut r = req(HarnessId::Codex, &codex, "q");
        r.schema = Some(schema.into());
        let (_, _, _, out) = exited(go(&fx, r, CancellationToken::new()).await);
        assert_eq!(
            out.as_deref(),
            Some(&*format!("PROMPT<q>SCHEMA<{schema}>FILES<0>"))
        );
        let (_, _, _, out) = exited(
            go(
                &fx,
                req(HarnessId::Codex, &codex, "q"),
                CancellationToken::new(),
            )
            .await,
        );
        assert_eq!(out.as_deref(), Some("PROMPT<q>SCHEMA<>FILES<0>"));

        // Claude has no schema_args: a schema changes nothing in its argv.
        let claude = fx.fake("fake-claude", &[]);
        let mut r = req(HarnessId::Claude, &claude, "q");
        r.schema = Some(schema.into());
        let (_, stdout, _, _) = exited(go(&fx, r, CancellationToken::new()).await);
        assert_eq!(stdout, r#"{"result":"q","args":8}"#);
    }

    /// Wrapper v2 runs `set -- {args}` then `set -- "$@" {schema_args}`, and
    /// Jodd's agent_cli does `template.extend(schema_args)`: the schema args
    /// go AFTER Codex's trailing `-`. Pinned here so parity is deliberate.
    #[tokio::test]
    async fn codex_argv_is_wrapper_v2_order_and_cwd_is_scratch_work() {
        let fx = Fx::new();
        let cli = fx.script("printf '%s\\n' \"$@\"; pwd -P; ls -ld .. | cut -c1-10\n");
        let mut r = req(HarnessId::Codex, &cli, "q");
        r.schema = Some("{}".into());
        let (code, stdout, _, _) = exited(go(&fx, r, CancellationToken::new()).await);
        assert_eq!(code, 0);
        let lines: Vec<&str> = stdout.lines().collect();
        let out = Path::new(lines[7]);
        let scratch = out.parent().unwrap();
        assert!(scratch
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("jodd-ai."));
        assert_eq!(out.file_name().unwrap(), "out");
        let schema = scratch.join("schema").to_string_lossy().into_owned();
        let expected = vec![
            "exec",
            "--ignore-user-config",
            "--ephemeral",
            "--sandbox",
            "read-only",
            "--skip-git-repo-check",
            "--output-last-message",
            lines[7],
            "-",
            "--output-schema",
            &*schema,
        ];
        assert_eq!(&lines[..11], &expected[..]);
        let work = std::fs::canonicalize(fx.scratch_parent())
            .unwrap()
            .join(scratch.file_name().unwrap())
            .join("work");
        assert_eq!(
            lines[11],
            work.to_string_lossy(),
            "cwd is the scratch's work/"
        );
        assert_eq!(lines[12], "drwx------", "scratch is private (0700)");
        assert_eq!(lines.len(), 13);
    }

    #[tokio::test]
    async fn child_env_is_inherited_with_path_overridden() {
        // Wrapper v2 parity (ruling R11): the CLI inherits our environment,
        // and only PATH is replaced, by the policy's path_env.
        let fx = Fx::new();
        let cli = fx.script("env\n");
        let (_, stdout, _, _) = exited(
            go(
                &fx,
                req(HarnessId::Claude, &cli, ""),
                CancellationToken::new(),
            )
            .await,
        );
        let lines: Vec<&str> = stdout.lines().collect();
        let paths: Vec<&&str> = lines.iter().filter(|l| l.starts_with("PATH=")).collect();
        assert_eq!(paths, [&&*format!("PATH={PATH_ENV}")], "{stdout}");
        // cargo test sets CARGO_MANIFEST_DIR in our environment: an unrelated
        // variable that must reach the CLI unchanged.
        let inherited = std::env::var("CARGO_MANIFEST_DIR").expect("set by cargo test");
        assert!(
            lines.contains(&&*format!("CARGO_MANIFEST_DIR={inherited}")),
            "{stdout}"
        );
    }

    #[tokio::test]
    async fn stdout_and_stderr_stay_apart() {
        let fx = Fx::new();
        let codex = fx.fake("fake-codex", &[]);
        let (_, stdout, stderr, _) = exited(
            go(
                &fx,
                req(HarnessId::Codex, &codex, "x"),
                CancellationToken::new(),
            )
            .await,
        );
        assert_eq!(stdout, "noise on stdout\n");
        assert_eq!(stderr, "codex stderr line\n");
    }

    #[tokio::test]
    async fn exit_code_and_signal_are_reported() {
        let fx = Fx::new();
        let codex = fx.fake("fake-codex", &[("FAKE_EXIT", "3")]);
        let (code, _, _, _) = exited(
            go(
                &fx,
                req(HarnessId::Codex, &codex, "x"),
                CancellationToken::new(),
            )
            .await,
        );
        assert_eq!(code, 3);
        let killed = fx.script("kill -TERM $$\n");
        let (code, _, _, _) = exited(
            go(
                &fx,
                req(HarnessId::Claude, &killed, "x"),
                CancellationToken::new(),
            )
            .await,
        );
        assert_eq!(code, 143, "a signal is 128+n, as the shell reports it");
    }

    #[tokio::test]
    async fn timeout_returns_timedout_and_kills_group() {
        let fx = Fx::new();
        let pidfile = fx.path("fake.pid");
        let codex = fx.fake(
            "fake-codex",
            &[
                ("FAKE_SLEEP", "40"),
                ("FAKE_PIDFILE", &pidfile.to_string_lossy()),
            ],
        );
        let mut r = req(HarnessId::Codex, &codex, "x");
        r.timeout = Duration::from_secs(1);
        let started = Instant::now();
        let o = go(&fx, r, CancellationToken::new()).await;
        assert!(matches!(o, RunOutcome::TimedOut { .. }), "{o:?}");
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "{:?}",
            started.elapsed()
        );
        let pid = wait_for_pid(&pidfile).await;
        assert!(gone_within(pid, 7).await, "the CLI's group must be killed");
        assert!(fx.scratch_gone_within(7).await);
    }

    #[tokio::test]
    async fn stubborn_cli_is_killed_after_term() {
        let fx = Fx::new();
        let pidfile = fx.path("fake.pid");
        let codex = fx.fake(
            "fake-codex",
            &[
                ("FAKE_SLEEP", "40"),
                ("FAKE_STUBBORN", "all"),
                ("FAKE_PIDFILE", &pidfile.to_string_lossy()),
            ],
        );
        let cancel = CancellationToken::new();
        let pid = cancel_when_started(pidfile, cancel.clone());
        let o = go(&fx, req(HarnessId::Codex, &codex, "x"), cancel).await;
        assert_eq!(o, RunOutcome::Cancelled);
        let pid = pid.await.unwrap();
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert!(
            alive(pid),
            "TERM was ignored, and the KILL waits out its 5 s grace"
        );
        assert!(
            gone_within(pid, 7).await,
            "a CLI that ignores TERM outlived the KILL"
        );
        assert!(fx.scratch_gone_within(3).await);
    }

    #[tokio::test]
    async fn stubborn_grandchild_is_killed_after_term() {
        // Only the CLI's child ignores TERM: the CLI itself exits at once, and
        // the group's remaining member must still get the KILL.
        let fx = Fx::new();
        let pidfile = fx.path("fake.pid");
        let codex = fx.fake(
            "fake-codex",
            &[
                ("FAKE_SLEEP", "40"),
                ("FAKE_STUBBORN", "child"),
                ("FAKE_PIDFILE", &pidfile.to_string_lossy()),
            ],
        );
        let mut r = req(HarnessId::Codex, &codex, "x");
        r.timeout = Duration::from_secs(1);
        let o = go(&fx, r, CancellationToken::new()).await;
        assert!(matches!(o, RunOutcome::TimedOut { .. }), "{o:?}");
        let pid = wait_for_pid(&pidfile).await;
        assert!(
            gone_within(pid, 8).await,
            "the CLI's child outlived the KILL"
        );
        assert!(fx.scratch_gone_within(3).await);
    }

    #[tokio::test]
    async fn cancel_returns_cancelled_promptly() {
        let fx = Fx::new();
        let pidfile = fx.path("fake.pid");
        // Stubborn, so a runner that waited for the KILL would take 5 s.
        let codex = fx.fake(
            "fake-codex",
            &[
                ("FAKE_SLEEP", "40"),
                ("FAKE_STUBBORN", "all"),
                ("FAKE_PIDFILE", &pidfile.to_string_lossy()),
            ],
        );
        let cancel = CancellationToken::new();
        let (c, p) = (cancel.clone(), pidfile.clone());
        // 200 ms into the run proper (once the fake is up and ignoring TERM),
        // not into a spawn that a loaded machine may not have finished.
        let cancelled_at = tokio::spawn(async move {
            wait_for_pid(&p).await;
            tokio::time::sleep(Duration::from_millis(200)).await;
            c.cancel();
            Instant::now()
        });
        let o = go(&fx, req(HarnessId::Codex, &codex, "x"), cancel).await;
        let returned_at = Instant::now();
        assert_eq!(o, RunOutcome::Cancelled);
        let took = returned_at - cancelled_at.await.unwrap();
        assert!(took < Duration::from_secs(1), "{took:?}");
        let pid = wait_for_pid(&pidfile).await;
        assert!(gone_within(pid, 7).await);
        assert!(fx.scratch_gone_within(3).await);
    }

    /// Review Focus 1: the client vanishes mid-run. The callers turn EOF on
    /// their stdin into `cancel`; this drives the runner exactly that way.
    #[tokio::test]
    async fn stdin_eof_mid_run_kills_group_and_cleans_scratch() {
        use tokio::io::AsyncReadExt;
        let fx = Fx::new();
        let pidfile = fx.path("fake.pid");
        let codex = fx.fake(
            "fake-codex",
            &[
                ("FAKE_SLEEP", "40"),
                ("FAKE_PIDFILE", &pidfile.to_string_lossy()),
            ],
        );
        let (client, mut server_stdin) = tokio::io::duplex(64);
        let cancel = CancellationToken::new();
        let c = cancel.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 64];
            while server_stdin
                .read(&mut buf)
                .await
                .map(|n| n > 0)
                .unwrap_or(false)
            {}
            c.cancel();
        });
        let p = pidfile.clone();
        tokio::spawn(async move {
            wait_for_pid(&p).await;
            drop(client); // the SSH channel closes
        });
        let started = Instant::now();
        let o = go(&fx, req(HarnessId::Codex, &codex, "x"), cancel).await;
        assert_eq!(o, RunOutcome::Cancelled);
        let pid = wait_for_pid(&pidfile).await;
        assert!(gone_within(pid, 7).await, "the CLI outlived its client");
        assert!(fx.scratch_gone_within(1).await);
        assert!(
            started.elapsed() < Duration::from_secs(8),
            "{:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn scratch_removed_on_every_outcome() {
        let fx = Fx::new();
        let codex = fx.fake("fake-codex", &[]);
        assert!(matches!(
            go(
                &fx,
                req(HarnessId::Codex, &codex, "x"),
                CancellationToken::new()
            )
            .await,
            RunOutcome::Exited { code: 0, .. }
        ));
        assert_eq!(fx.leftover_scratch(), 0, "after a clean exit");

        let failing = fx.fake("fake-codex", &[("FAKE_EXIT", "3")]);
        assert!(matches!(
            go(
                &fx,
                req(HarnessId::Codex, &failing, "x"),
                CancellationToken::new()
            )
            .await,
            RunOutcome::Exited { code: 3, .. }
        ));
        assert_eq!(fx.leftover_scratch(), 0, "after a failed exit");

        let hang = fx.fake("fake-codex", &[("FAKE_SLEEP", "40")]);
        let mut r = req(HarnessId::Codex, &hang, "x");
        r.timeout = Duration::from_millis(300);
        assert!(matches!(
            go(&fx, r, CancellationToken::new()).await,
            RunOutcome::TimedOut { .. }
        ));
        assert!(fx.scratch_gone_within(7).await, "after a timeout");

        let cancel = CancellationToken::new();
        let c = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            c.cancel();
        });
        assert_eq!(
            go(&fx, req(HarnessId::Codex, &hang, "x"), cancel).await,
            RunOutcome::Cancelled
        );
        assert!(fx.scratch_gone_within(7).await, "after a cancel");

        let missing = cfg(&fx.path("nope").to_string_lossy());
        assert_eq!(
            go(
                &fx,
                req(HarnessId::Codex, &missing, "x"),
                CancellationToken::new()
            )
            .await,
            RunOutcome::Missing
        );
        assert_eq!(fx.leftover_scratch(), 0, "after Missing");

        // Passes the pre-check, but exec fails (ENOENT: no such interpreter)
        // after the scratch exists.
        let broken = fx.script("");
        std::fs::write(&broken.path, "#!/nonexistent/interpreter\n").unwrap();
        assert_eq!(
            go(
                &fx,
                req(HarnessId::Codex, &broken, "x"),
                CancellationToken::new()
            )
            .await,
            RunOutcome::Missing
        );
        assert_eq!(fx.leftover_scratch(), 0, "after a failed spawn");
    }

    #[tokio::test]
    async fn missing_binary_is_missing() {
        let fx = Fx::new();
        let started = fx.path("started");
        let not_exec = fx.path("not-exec");
        std::fs::write(
            &not_exec,
            "#!/bin/sh\n: > '".to_string() + &started.to_string_lossy() + "'\n",
        )
        .unwrap();
        std::fs::set_permissions(&not_exec, std::fs::Permissions::from_mode(0o644)).unwrap();
        for path in [
            fx.path("nope").to_string_lossy().into_owned(),
            not_exec.to_string_lossy().into_owned(),
            fx.dir.path().to_string_lossy().into_owned(), // a directory is "executable"
            String::new(),
            "fake-codex".into(), // relative: never resolved against any PATH
        ] {
            let c = cfg(&path);
            let r = req(HarnessId::Codex, &c, "x");
            assert_eq!(
                go(&fx, r, CancellationToken::new()).await,
                RunOutcome::Missing,
                "{path:?}"
            );
        }
        assert!(!started.exists(), "nothing ran");
        assert_eq!(fx.leftover_scratch(), 0);
    }

    #[tokio::test]
    async fn non_utf8_output_is_lossy_not_error() {
        let fx = Fx::new();
        let codex = fx.fake("fake-codex", &[("FAKE_BINARY_OUT", "1")]);
        let (code, stdout, stderr, _) = exited(
            go(
                &fx,
                req(HarnessId::Codex, &codex, "x"),
                CancellationToken::new(),
            )
            .await,
        );
        assert_eq!(code, 0);
        assert!(stdout.contains("bin\u{FFFD}\u{FFFD}out\n"), "{stdout:?}");
        assert!(stderr.contains("bin\u{FFFD}err\n"), "{stderr:?}");
    }

    /// Ruling R9: callers exit right after Cancelled; `drain_reaps` is what
    /// still KILLs a CLI that ignores TERM, and removes its scratch, first.
    #[tokio::test]
    async fn drain_reaps_kills_stubborn_cli_before_return() {
        let fx = Fx::new();
        let pidfile = fx.path("fake.pid");
        let codex = fx.fake(
            "fake-codex",
            &[
                ("FAKE_SLEEP", "40"),
                ("FAKE_STUBBORN", "all"),
                ("FAKE_PIDFILE", &pidfile.to_string_lossy()),
            ],
        );
        let cancel = CancellationToken::new();
        let pid = cancel_when_started(pidfile, cancel.clone());
        assert_eq!(
            go(&fx, req(HarnessId::Codex, &codex, "x"), cancel).await,
            RunOutcome::Cancelled
        );
        let pid = pid.await.unwrap();
        assert!(alive(pid), "TERM is ignored: only the KILL ends it");
        tokio::time::timeout(Duration::from_secs(15), drain_reaps())
            .await
            .expect("drain hung");
        assert!(!alive(pid), "drain_reaps returned with the CLI alive");
        assert_eq!(
            fx.leftover_scratch(),
            0,
            "drain_reaps returned with the scratch present"
        );
    }
}
