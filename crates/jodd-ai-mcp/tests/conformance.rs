//! The MCP surface (`jodd-ai-mcp` over stdio), driven through the built binary
//! exactly as sshd runs it: the fixed argv `--key 3f2a9b1c` and the client's
//! word in SSH_ORIGINAL_COMMAND. This is the conformance suite of spec §6
//! rule 3 and §7: any client author can run it against the host.
//!
//! Most tests speak MCP through rmcp's own client. Four drive raw JSON-RPC
//! lines instead, because they need what rmcp's child-process transport hides:
//! sending `notifications/cancelled` for one request, closing the server's
//! stdin while it keeps running, and watching the process exit on its own
//! (rmcp's transport kills the child when dropped).
//!
//! Every test runs the server with HOME, the policy (`JODD_AI_MCP_POLICY`),
//! the slots dir (`JODD_AI_MCP_SLOTS_DIR`), the call log
//! (`JODD_AI_MCP_LOG_FILE`) and TMPDIR inside its own temp dir, so no test
//! touches the real ~/.ssh, /etc, /run or /var/log (or syslog), and parallel
//! tests never share a slot or count each other's scratch dirs.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use rmcp::model::{
    CallToolRequest, CallToolRequestParams, CallToolResult, ClientRequest, ErrorCode, JsonObject,
    Tool,
};
use rmcp::service::{PeerRequestOptions, RunningService, ServiceError};
use rmcp::transport::TokioChildProcess;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{json, Value};

const BIN: &str = env!("CARGO_BIN_EXE_jodd-ai-mcp");
const KEY: &str = "3f2a9b1c";
const PATH_ENV: &str = "/usr/bin:/bin";
/// Generous bound for a call that should answer at once.
const QUICK: Duration = Duration::from_secs(20);

fn fake(name: &str) -> String {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fakes")
        .join(name)
        .to_string_lossy()
        .into_owned()
}

// ---------------------------------------------------------------- fixture

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

fn off(path: &str) -> Option<H> {
    Some(H {
        enabled: false,
        path: path.into(),
        timeout: 120,
    })
}

struct Fx {
    dir: tempfile::TempDir,
}

impl Fx {
    /// Both harnesses enabled on the fakes, shipped timeouts.
    fn both() -> Fx {
        Fx::with(
            on(&fake("fake-claude"), 120),
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

    fn log_lines(&self) -> Vec<String> {
        std::fs::read_to_string(self.path("calls.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
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

    fn scratch_gone_within(&self, secs: u64) -> bool {
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(secs) {
            if self.leftover_scratch() == 0 {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }

    /// The server's environment, as sshd would give it plus the test overrides.
    fn env(&self) -> Vec<(String, String)> {
        let p = |n: &str| self.path(n).to_string_lossy().into_owned();
        vec![
            ("SSH_ORIGINAL_COMMAND".into(), "jodd-ai-mcp".into()),
            ("HOME".into(), p("home")),
            ("JODD_AI_MCP_POLICY".into(), p("policy.toml")),
            ("JODD_AI_MCP_SLOTS_DIR".into(), p("slots")),
            ("JODD_AI_MCP_LOG_FILE".into(), p("calls.log")),
            ("TMPDIR".into(), p("tmp")),
        ]
    }
}

// ---------------------------------------------------------- rmcp client

type Client = RunningService<RoleClient, ()>;

/// An rmcp client over the server's stdio; `env` reaches the fake CLIs (the
/// server passes its environment through, ruling R11). The server's stderr
/// goes to `<name>.stderr` in the fixture.
async fn client_named(fx: &Fx, name: &str, env: &[(&str, &str)]) -> Client {
    let mut cmd = tokio::process::Command::new(BIN);
    cmd.args(["--key", KEY]);
    for (k, v) in fx.env() {
        cmd.env(k, v);
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    let stderr = std::fs::File::create(fx.path(&format!("{name}.stderr"))).unwrap();
    let (transport, _) = TokioChildProcess::builder(cmd)
        .stderr(stderr)
        .spawn()
        .unwrap();
    tokio::time::timeout(QUICK, ().serve(transport))
        .await
        .expect("initialize timed out")
        .expect("initialize failed")
}

async fn client(fx: &Fx, env: &[(&str, &str)]) -> Client {
    client_named(fx, "server", env).await
}

fn args(v: Value) -> JsonObject {
    match v {
        Value::Object(m) => m,
        other => panic!("arguments must be an object, got {other}"),
    }
}

async fn call(c: &Client, name: &str, a: Value) -> Result<CallToolResult, ServiceError> {
    tokio::time::timeout(
        QUICK,
        c.call_tool(CallToolRequestParams::new(name.to_string()).with_arguments(args(a))),
    )
    .await
    .expect("tools/call timed out")
}

async fn infer(c: &Client, a: Value) -> CallToolResult {
    call(c, "infer", a).await.expect("infer: protocol error")
}

async fn tools(c: &Client) -> Vec<Tool> {
    tokio::time::timeout(QUICK, c.list_all_tools())
        .await
        .expect("tools/list timed out")
        .expect("tools/list failed")
}

fn tool<'a>(ts: &'a [Tool], name: &str) -> Option<&'a Tool> {
    ts.iter().find(|t| t.name == name)
}

fn harness_enum(t: &Tool) -> Vec<String> {
    t.input_schema["properties"]["harness"]["enum"]
        .as_array()
        .expect("harness enum")
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect()
}

fn text(r: &CallToolResult) -> String {
    r.content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .expect("content[0] is text")
}

fn structured(r: &CallToolResult) -> &Value {
    r.structured_content.as_ref().expect("structuredContent")
}

fn is_error(r: &CallToolResult) -> bool {
    r.is_error == Some(true)
}

fn kind(r: &CallToolResult) -> String {
    assert!(is_error(r), "expected a tool error, got {r:?}");
    structured(r)["kind"].as_str().unwrap().to_string()
}

fn invalid_params(r: Result<CallToolResult, ServiceError>) {
    match r {
        Err(ServiceError::McpError(e)) => {
            assert_eq!(e.code, ErrorCode::INVALID_PARAMS, "{e:?}")
        }
        other => panic!("expected invalid params, got {other:?}"),
    }
}

// --------------------------------------------------------- raw JSON-RPC

/// A server driven by hand-written JSON-RPC lines.
struct Raw {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: mpsc::Receiver<Value>,
    /// Every response id read so far.
    seen: Vec<Value>,
}

impl Raw {
    fn start(fx: &Fx, env: &[(&str, &str)]) -> Raw {
        let mut cmd = Command::new(BIN);
        cmd.args(["--key", KEY])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(std::fs::File::create(fx.path("raw.stderr")).unwrap());
        for (k, v) in fx.env() {
            cmd.env(k, v);
        }
        for (k, v) in env {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                let v: Value = serde_json::from_str(&line)
                    .unwrap_or_else(|e| panic!("server wrote a non-JSON line {line:?}: {e}"));
                if tx.send(v).is_err() {
                    break;
                }
            }
        });
        let mut raw = Raw {
            stdin: child.stdin.take(),
            child,
            lines,
            seen: Vec::new(),
        };
        raw.send(json!({
            "jsonrpc": "2.0", "id": 0, "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "conformance", "version": "0"}
            }
        }));
        let init = raw.response(0, QUICK).expect("no initialize response");
        assert!(init.get("result").is_some(), "{init}");
        raw.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        raw
    }

    fn send(&mut self, v: Value) {
        let stdin = self.stdin.as_mut().expect("stdin already closed");
        writeln!(stdin, "{v}").unwrap();
        stdin.flush().unwrap();
    }

    fn call(&mut self, id: u64, name: &str, arguments: Value) {
        self.send(json!({
            "jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": {"name": name, "arguments": arguments}
        }));
    }

    fn cancel(&mut self, id: u64) {
        self.send(json!({
            "jsonrpc": "2.0", "method": "notifications/cancelled",
            "params": {"requestId": id, "reason": "test"}
        }));
    }

    /// The response to request `id`; everything read is recorded in `seen`.
    fn response(&mut self, id: u64, within: Duration) -> Option<Value> {
        let deadline = Instant::now() + within;
        loop {
            let left = deadline.checked_duration_since(Instant::now())?;
            let v = self.lines.recv_timeout(left).ok()?;
            if let Some(got) = v.get("id") {
                self.seen.push(got.clone());
            }
            if v.get("id") == Some(&json!(id)) {
                return Some(v);
            }
        }
    }

    /// Reads whatever arrives for `within`, recording it in `seen`.
    fn drain(&mut self, within: Duration) {
        let _ = self.response(u64::MAX, within);
    }

    fn saw(&self, id: u64) -> bool {
        self.seen.contains(&json!(id))
    }

    fn close_stdin(&mut self) {
        self.stdin = None;
    }

    fn exited_within(&mut self, within: Duration) -> Option<ExitStatus> {
        let started = Instant::now();
        while started.elapsed() < within {
            if let Some(st) = self.child.try_wait().unwrap() {
                return Some(st);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        None
    }
}

impl Drop for Raw {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn pid_alive(pid: &str) -> bool {
    Command::new("kill")
        .args(["-0", pid])
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .success()
}

/// Waits for a pid file with something in it.
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

/// True once the process whose pid the fake CLI recorded in `pidfile` is gone.
fn fake_is_gone_within(pidfile: &Path, secs: u64) -> bool {
    let pid = std::fs::read_to_string(pidfile)
        .expect("the fake CLI must have recorded its pid")
        .trim()
        .to_string();
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(secs) {
        if !pid_alive(&pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

// ------------------------------------------------------------- discovery

#[tokio::test]
async fn initialize_reports_version() {
    let fx = Fx::both();
    let c = client(&fx, &[]).await;
    let info = c.peer_info().expect("server info");
    assert_eq!(info.server_info.name, "jodd-ai-host");
    assert_eq!(info.server_info.version, env!("CARGO_PKG_VERSION"));
    assert!(info.capabilities.tools.is_some());
}

#[tokio::test]
async fn tools_list_has_only_enabled_harnesses() {
    let fx = Fx::with(off(&fake("fake-claude")), on(&fake("fake-codex"), 180), "");
    let c = client(&fx, &[]).await;
    let ts = tools(&c).await;
    let mut names: Vec<_> = ts.iter().map(|t| t.name.to_string()).collect();
    names.sort();
    assert_eq!(names, ["forget_key", "infer"]);
    assert_eq!(harness_enum(tool(&ts, "infer").unwrap()), ["codex"]);

    let fx = Fx::both();
    let c = client(&fx, &[]).await;
    let ts = tools(&c).await;
    assert_eq!(
        harness_enum(tool(&ts, "infer").unwrap()),
        ["claude", "codex"]
    );
    let schema = &tool(&ts, "infer").unwrap().input_schema;
    assert_eq!(schema["required"], json!(["harness", "prompt"]));
    assert_eq!(schema["additionalProperties"], json!(false));
    assert_eq!(schema["properties"]["timeout_secs"]["minimum"], json!(1));

    // Enabled with a relative path is not enabled; nothing enabled lists no infer.
    let fx = Fx::with(on("claude", 120), off(&fake("fake-codex")), "");
    let c = client(&fx, &[]).await;
    let names: Vec<_> = tools(&c).await.iter().map(|t| t.name.to_string()).collect();
    assert_eq!(names, ["forget_key"]);
}

#[tokio::test]
async fn bad_policy_lists_no_tools() {
    for fx in [
        // a typo'd key
        {
            let fx = Fx::both();
            std::fs::write(fx.policy(), "max_request_byte = 5\n").unwrap();
            fx
        },
        // a wrong type
        {
            let fx = Fx::both();
            std::fs::write(fx.policy(), "max_request_bytes = 'x'\n").unwrap();
            fx
        },
        // deleted
        Fx::empty(),
    ] {
        let c = client(
            &fx,
            &[("FAKE_STARTED", &fx.path("cli.started").to_string_lossy())],
        )
        .await;
        let names: Vec<_> = tools(&c).await.iter().map(|t| t.name.to_string()).collect();
        assert_eq!(names, ["forget_key"]);
        // A stale client that still asks gets `not_enabled` naming doctor.
        let r = infer(&c, json!({"harness": "codex", "prompt": "hi"})).await;
        assert_eq!(kind(&r), "not_enabled");
        assert!(text(&r).contains("jodd-ai-host doctor"), "{}", text(&r));
        assert!(!fx.path("cli.started").exists(), "the CLI must never start");
    }
}

#[tokio::test]
async fn tool_meta_carries_tool_version() {
    let fx = Fx::both();
    let c = client(&fx, &[]).await;
    let ts = tools(&c).await;
    assert_eq!(ts.len(), 2);
    for t in &ts {
        let meta = t
            .meta
            .as_ref()
            .unwrap_or_else(|| panic!("{} has no _meta", t.name));
        assert_eq!(
            meta.0.get("jodd-ai-host/tool-version"),
            Some(&json!(1)),
            "{}",
            t.name
        );
    }
}

// ------------------------------------------------------------------ infer

#[tokio::test]
async fn infer_claude_returns_unwrapped_result() {
    let fx = Fx::both();
    let c = client(&fx, &[]).await;
    let r = infer(&c, json!({"harness": "claude", "prompt": "hello"})).await;
    assert!(!is_error(&r), "{r:?}");
    assert_eq!(text(&r), "hello");
    assert_eq!(
        structured(&r),
        &json!({"harness": "claude", "exit_code": 0})
    );
}

#[tokio::test]
async fn infer_codex_returns_last_message() {
    let fx = Fx::both();
    let c = client(&fx, &[]).await;
    let r = infer(&c, json!({"harness": "codex", "prompt": "hello"})).await;
    assert!(!is_error(&r), "{r:?}");
    // The fake writes the prompt and schema into its last-message file; its
    // stdout chatter ("noise on stdout") is not the answer.
    assert!(
        text(&r).starts_with("PROMPT<hello>SCHEMA<>"),
        "{}",
        text(&r)
    );
    assert!(!text(&r).contains("noise"));
    assert_eq!(structured(&r), &json!({"harness": "codex", "exit_code": 0}));

    // With a schema: handed to the CLI as its schema file, as JSON text.
    let r = infer(
        &c,
        json!({"harness": "codex", "prompt": "p", "schema": {"type": "object"}}),
    )
    .await;
    assert!(!is_error(&r), "{r:?}");
    assert!(
        text(&r).starts_with(r#"PROMPT<p>SCHEMA<{"type":"object"}>"#),
        "{}",
        text(&r)
    );
}

#[tokio::test]
async fn infer_joins_system_like_jodd() {
    let fx = Fx::both();
    let c = client(&fx, &[]).await;
    let r = infer(
        &c,
        json!({"harness": "codex", "system": "S", "prompt": "P"}),
    )
    .await;
    assert!(!is_error(&r), "{r:?}");
    assert!(text(&r).contains("PROMPT<S\n\n---\n\nP>"), "{:?}", text(&r));
}

#[tokio::test]
async fn disabled_harness_is_not_enabled_error_and_cli_never_starts() {
    let fx = Fx::with(on(&fake("fake-claude"), 120), off(&fake("fake-codex")), "");
    let started = fx.path("cli.started");
    let c = client(&fx, &[("FAKE_STARTED", &started.to_string_lossy())]).await;

    let r = infer(&c, json!({"harness": "codex", "prompt": "hi"})).await;
    assert_eq!(kind(&r), "not_enabled");
    assert!(
        text(&r).contains("sudo jodd-ai-host enable codex"),
        "{}",
        text(&r)
    );
    assert!(!started.exists(), "the CLI must never start");

    // A harness this host has never heard of (a stale or newer client): the
    // same tool error, not invalid params, naming doctor.
    let r = infer(&c, json!({"harness": "pi", "prompt": "hi"})).await;
    assert_eq!(kind(&r), "not_enabled");
    assert!(text(&r).contains("jodd-ai-host doctor"), "{}", text(&r));
    assert!(!started.exists());
}

#[tokio::test]
async fn oversized_prompt_is_too_large_and_cli_never_starts() {
    let fx = Fx::with(
        None,
        on(&fake("fake-codex"), 180),
        "max_request_bytes = 100\n",
    );
    let started = fx.path("cli.started");
    let c = client(&fx, &[("FAKE_STARTED", &started.to_string_lossy())]).await;

    // Measured as the joined stdin plus the schema text, in UTF-8 bytes.
    let r = infer(&c, json!({"harness": "codex", "prompt": "x".repeat(101)})).await;
    assert_eq!(kind(&r), "too_large");
    let r = infer(
        &c,
        json!({"harness": "codex", "system": "s".repeat(50), "prompt": "p".repeat(50)}),
    )
    .await;
    assert_eq!(kind(&r), "too_large", "the join adds 7 bytes");
    let r = infer(
        &c,
        json!({"harness": "codex", "prompt": "p".repeat(90), "schema": {"type": "object"}}),
    )
    .await;
    assert_eq!(kind(&r), "too_large", "the schema text counts");
    // Thai is 3 bytes a character: 34 characters are 102 bytes.
    let r = infer(&c, json!({"harness": "codex", "prompt": "ก".repeat(34)})).await;
    assert_eq!(kind(&r), "too_large");
    assert!(!started.exists(), "the CLI must never start");

    // Exactly at the limit runs.
    let r = infer(&c, json!({"harness": "codex", "prompt": "x".repeat(100)})).await;
    assert!(!is_error(&r), "{r:?}");
}

#[tokio::test]
async fn non_object_schema_is_invalid_params() {
    let fx = Fx::both();
    let started = fx.path("cli.started");
    let c = client(&fx, &[("FAKE_STARTED", &started.to_string_lossy())]).await;
    for schema in [
        json!("{\"type\":\"object\"}"),
        json!([1]),
        json!(3),
        json!(true),
    ] {
        invalid_params(
            call(
                &c,
                "infer",
                json!({"harness": "codex", "prompt": "p", "schema": schema}),
            )
            .await,
        );
    }
    assert!(!started.exists(), "the CLI must never start");
}

#[tokio::test]
async fn malformed_arguments_are_invalid_params() {
    let fx = Fx::both();
    let started = fx.path("cli.started");
    let c = client(&fx, &[("FAKE_STARTED", &started.to_string_lossy())]).await;
    for a in [
        json!({"harness": "codex"}),
        json!({"prompt": "p"}),
        json!({"harness": "codex", "prompt": 5}),
        json!({"harness": 1, "prompt": "p"}),
        json!({"harness": "codex", "prompt": "p", "system": 5}),
        json!({"harness": "codex", "prompt": "p", "timeout_secs": 0}),
        json!({"harness": "codex", "prompt": "p", "timeout_secs": -3}),
        json!({"harness": "codex", "prompt": "p", "timeout_secs": 1.5}),
        json!({"harness": "codex", "prompt": "p", "timeout_secs": "5"}),
        // Clients can only narrow: an argument this host does not know is
        // refused, never silently ignored.
        json!({"harness": "codex", "prompt": "p", "model": "x"}),
    ] {
        invalid_params(call(&c, "infer", a).await);
    }
    // forget_key takes no arguments.
    invalid_params(call(&c, "forget_key", json!({"key": "00000000"})).await);
    // An unknown tool.
    invalid_params(call(&c, "shell", json!({})).await);
    assert!(!started.exists(), "the CLI must never start");
}

#[tokio::test]
async fn timeout_above_policy_is_clamped() {
    let fx = Fx::with(None, on(&fake("fake-codex"), 1), "");
    let c = client(&fx, &[("FAKE_SLEEP", "30")]).await;
    let started = Instant::now();
    let r = infer(
        &c,
        json!({"harness": "codex", "prompt": "p", "timeout_secs": 999}),
    )
    .await;
    assert_eq!(kind(&r), "timeout");
    assert!(
        started.elapsed() < Duration::from_secs(7),
        "{:?}",
        started.elapsed()
    );
    assert!(text(&r).contains("1s"), "{}", text(&r));

    // A request may lower the policy's value.
    let fx = Fx::with(None, on(&fake("fake-codex"), 180), "");
    let c = client(&fx, &[("FAKE_SLEEP", "30")]).await;
    let started = Instant::now();
    let r = infer(
        &c,
        json!({"harness": "codex", "prompt": "p", "timeout_secs": 1}),
    )
    .await;
    assert_eq!(kind(&r), "timeout");
    assert!(
        started.elapsed() < Duration::from_secs(7),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn cli_failure_carries_raw_stdout_and_stderr() {
    let fx = Fx::both();
    let c = client(&fx, &[("FAKE_EXIT", "3")]).await;
    let r = infer(&c, json!({"harness": "codex", "prompt": "p"})).await;
    assert_eq!(kind(&r), "cli_failed");
    let s = structured(&r);
    assert_eq!(s["exit_code"], json!(3));
    assert_eq!(s["stdout"], json!("noise on stdout\n"));
    assert_eq!(s["stderr"], json!("codex stderr line\n"));

    // Exit 0 but no readable answer: the raw output comes back too.
    let script = fx.path("silent-claude");
    std::fs::write(&script, "#!/bin/sh\necho 'not json'\necho oops >&2\n").unwrap();
    Command::new("chmod")
        .arg("755")
        .arg(&script)
        .status()
        .unwrap();
    let fx2 = Fx::with(on(&script.to_string_lossy(), 120), None, "");
    let c = client(&fx2, &[]).await;
    let r = infer(&c, json!({"harness": "claude", "prompt": "p"})).await;
    assert_eq!(kind(&r), "cli_failed");
    let s = structured(&r);
    assert_eq!(s["exit_code"], json!(0));
    assert_eq!(s["stdout"], json!("not json\n"));
    assert_eq!(s["stderr"], json!("oops\n"));
}

#[tokio::test]
async fn missing_cli_is_cli_missing() {
    let fx = Fx::with(on("/nonexistent/claude", 120), None, "");
    let c = client(&fx, &[]).await;
    let r = infer(&c, json!({"harness": "claude", "prompt": "p"})).await;
    assert_eq!(kind(&r), "cli_missing");
    assert!(text(&r).contains("jodd-ai-host doctor"), "{}", text(&r));
}

// ----------------------------------------------------------- cancellation

#[test]
fn cancelled_notification_kills_cli() {
    let fx = Fx::both();
    let pidfile = fx.path("fake.pid");
    let mut s = Raw::start(
        &fx,
        &[
            ("FAKE_SLEEP", "30"),
            ("FAKE_PIDFILE", &pidfile.to_string_lossy()),
        ],
    );
    s.call(7, "infer", json!({"harness": "codex", "prompt": "p"}));
    wait_for_pidfile(&pidfile);
    assert_eq!(fx.leftover_scratch(), 1);
    s.cancel(7);
    assert!(
        fake_is_gone_within(&pidfile, 3),
        "the CLI outlived the cancel"
    );
    assert!(fx.scratch_gone_within(7), "scratch outlived the cancel");
    // The session goes on: the server still answers.
    s.send(json!({"jsonrpc": "2.0", "id": 8, "method": "tools/list"}));
    let r = s.response(8, QUICK).expect("server stopped answering");
    assert!(r["result"]["tools"].is_array(), "{r}");
    // MCP: a cancelled request gets no response, before id 8's or after it.
    s.drain(Duration::from_millis(500));
    assert!(s.saw(0) && s.saw(8), "the recorder works: {:?}", s.seen);
    assert!(
        !s.saw(7),
        "the cancelled request was answered: {:?}",
        s.seen
    );
    let log = fx.log_lines();
    assert_eq!(log.len(), 1, "{log:?}");
    assert!(log[0].contains("outcome=cancelled"), "{log:?}");
}

#[test]
fn mcp_stdin_close_mid_infer_kills_cli() {
    let fx = Fx::both();
    let pidfile = fx.path("fake.pid");
    let mut s = Raw::start(
        &fx,
        &[
            ("FAKE_SLEEP", "30"),
            ("FAKE_PIDFILE", &pidfile.to_string_lossy()),
        ],
    );
    s.call(3, "infer", json!({"harness": "codex", "prompt": "p"}));
    wait_for_pidfile(&pidfile);
    s.close_stdin();
    assert!(
        fake_is_gone_within(&pidfile, 3),
        "the CLI outlived the client"
    );
    assert!(fx.scratch_gone_within(7), "scratch outlived the client");
    let st = s.exited_within(Duration::from_secs(10));
    assert!(st.is_some(), "the server did not exit after stdin closed");
    assert_eq!(std::fs::read_to_string(fx.path("raw.stderr")).unwrap(), "");
}

#[test]
fn stubborn_cli_is_killed_after_the_grace_when_the_client_goes() {
    let fx = Fx::both();
    let pidfile = fx.path("fake.pid");
    let mut s = Raw::start(
        &fx,
        &[
            ("FAKE_SLEEP", "30"),
            ("FAKE_STUBBORN", "all"),
            ("FAKE_PIDFILE", &pidfile.to_string_lossy()),
        ],
    );
    s.call(3, "infer", json!({"harness": "codex", "prompt": "p"}));
    wait_for_pidfile(&pidfile);
    s.close_stdin();
    // TERM is ignored: KILL comes 5 s later, and the server waits for it.
    assert!(
        fake_is_gone_within(&pidfile, 8),
        "a TERM-ignoring CLI survived"
    );
    assert!(fx.scratch_gone_within(9));
    assert!(s.exited_within(Duration::from_secs(12)).is_some());
}

// ------------------------------------------------------------------ slots

#[tokio::test]
async fn third_concurrent_call_is_busy_and_slots_return() {
    let fx = Fx::with(
        None,
        on(&fake("fake-codex"), 180),
        "max_concurrent_runs = 2\n",
    );
    let marker = |n: &str| {
        fx.path(&format!("{n}.started"))
            .to_string_lossy()
            .into_owned()
    };
    let (ma, mb, mc) = (marker("a"), marker("b"), marker("c"));
    let a = client_named(&fx, "a", &[("FAKE_SLEEP", "30"), ("FAKE_STARTED", &ma)]).await;
    let b = client_named(&fx, "b", &[("FAKE_SLEEP", "30"), ("FAKE_STARTED", &mb)]).await;
    let c = client_named(&fx, "c", &[("FAKE_EXIT", "3"), ("FAKE_STARTED", &mc)]).await;
    let p = json!({"harness": "codex", "prompt": "p"});

    // A holds a slot until it is cancelled; B until it times out (2 s).
    let ha = a
        .peer()
        .send_cancellable_request(
            ClientRequest::CallToolRequest(CallToolRequest::new(
                CallToolRequestParams::new("infer").with_arguments(args(p.clone())),
            )),
            PeerRequestOptions::no_options(),
        )
        .await
        .unwrap();
    let hb = {
        let peer = b.peer().clone();
        tokio::spawn(async move {
            peer.call_tool(CallToolRequestParams::new("infer").with_arguments(args(
                json!({"harness": "codex", "prompt": "p", "timeout_secs": 2}),
            )))
            .await
        })
    };
    for m in [&ma, &mb] {
        let started = Instant::now();
        while !Path::new(m).exists() {
            assert!(started.elapsed() < QUICK, "{m} never started");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    // The third is refused at once, never queued, and its CLI never starts.
    let started = Instant::now();
    let r = infer(&c, p.clone()).await;
    assert_eq!(kind(&r), "busy");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    assert!(
        !Path::new(&mc).exists(),
        "a busy call must not start the CLI"
    );

    // After a cancel, the slot comes back.
    ha.cancel(Some("test".into())).await.unwrap();
    let r = until_not_busy(&c, &p).await;
    assert_eq!(kind(&r), "cli_failed", "C's CLI ran (and failed as told)");

    // After a timeout, too; and C's failure gave its slot back: two at once fit.
    let rb = hb.await.unwrap().unwrap();
    assert_eq!(kind(&rb), "timeout");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let (r1, r2) = tokio::join!(infer(&c, p.clone()), infer(&c, p.clone()));
        if kind(&r1) == "cli_failed" && kind(&r2) == "cli_failed" {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "slots never came back: {r1:?} {r2:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn until_not_busy(c: &Client, p: &Value) -> CallToolResult {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let r = infer(c, p.clone()).await;
        if !(is_error(&r) && kind(&r) == "busy") {
            return r;
        }
        assert!(Instant::now() < deadline, "the slot never came back");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

// ------------------------------------------------------------- forget_key

#[test]
fn forget_key_responds_and_exits() {
    // Ruling R15: keys::forget is a stub until Task 7, which adds the removal
    // assertion under the plan's name.
    let fx = Fx::empty(); // never subject to policy
    let mut s = Raw::start(&fx, &[]);
    s.send(json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}));
    let r = s.response(1, QUICK).unwrap();
    let names: Vec<_> = r["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(names, ["forget_key"]);

    s.call(2, "forget_key", json!({}));
    let r = s.response(2, QUICK).expect("no forget_key response");
    assert_eq!(
        r["result"]["structuredContent"],
        json!({"removed": false}),
        "{r}"
    );
    assert_ne!(r["result"]["isError"], json!(true), "{r}");
    // Exits on its own, though the client keeps stdin open.
    let st = s.exited_within(Duration::from_secs(10));
    assert!(st.is_some(), "the server did not exit after forget_key");
    assert!(s.stdin.is_some());
    assert_eq!(std::fs::read_to_string(fx.path("raw.stderr")).unwrap(), "");
    let log = fx.log_lines();
    assert_eq!(log.len(), 1, "{log:?}");
    assert!(log[0].contains("tool=forget_key"), "{log:?}");
}

// -------------------------------------------------------------------- log

#[tokio::test]
async fn every_call_logs_one_metadata_line_without_text() {
    let fx = Fx::both();
    let c = client(&fx, &[]).await;
    let secret = "SECRET-PROMPT-7f3c";
    // (fake-claude cannot echo newlines as JSON, so the system text goes to codex)
    let r = infer(&c, json!({"harness": "claude", "prompt": secret})).await;
    assert!(!is_error(&r), "{r:?}");
    let r = infer(
        &c,
        json!({"harness": "codex", "system": "SYS-9d1", "prompt": secret}),
    )
    .await;
    assert!(!is_error(&r), "{r:?}");
    let answer = text(&r);
    let r = infer(&c, json!({"harness": "pi", "prompt": secret})).await;
    assert_eq!(kind(&r), "not_enabled");
    invalid_params(
        call(
            &c,
            "infer",
            json!({"harness": "codex", "prompt": secret, "x": 1}),
        )
        .await,
    );
    let _ = tools(&c).await; // not a call: no line

    let log = fx.log_lines();
    assert_eq!(log.len(), 4, "{log:?}");
    let all = log.join("\n");
    for t in [secret, "SYS-9d1", answer.as_str(), "PROMPT<"] {
        assert!(!all.contains(t), "the log carries call text {t:?}: {all}");
    }
    assert!(log[0].contains(&format!("key={KEY}")), "{all}");
    assert!(log[0].contains("tool=infer"), "{all}");
    assert!(log[0].contains("harness=claude"), "{all}");
    assert!(log[0].contains("outcome=ok"), "{all}");
    assert!(log[0].contains("dur_ms="), "{all}");
    assert!(
        log[1].contains("harness=codex") && log[1].contains("outcome=ok"),
        "{all}"
    );
    assert!(
        log[2].contains("harness=-") && log[2].contains("outcome=not_enabled"),
        "{all}"
    );
    assert!(log[3].contains("outcome=invalid_params"), "{all}");
    // Nothing on the SSH channel's stderr.
    assert_eq!(
        std::fs::read_to_string(fx.path("server.stderr")).unwrap(),
        ""
    );
}

// ------------------------------------------------------------------ entry

#[test]
fn mcp_request_with_extra_words_is_unknown() {
    // Exactly `jodd-ai-mcp`: a word this server does not know (a future
    // client's) fails loudly as it does today, exit 64 and `unknown request`.
    let fx = Fx::both();
    let mut cmd = Command::new(BIN);
    cmd.args(["--key", KEY]);
    for (k, v) in fx.env() {
        cmd.env(k, v);
    }
    let out = cmd
        .env("SSH_ORIGINAL_COMMAND", "jodd-ai-mcp --v2")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(64));
    assert_eq!(String::from_utf8_lossy(&out.stderr), "unknown request\n");
    assert!(out.stdout.is_empty());
}

// ------------------------------------------------- policy and line limits

#[tokio::test]
async fn disable_mid_session_reaches_next_infer() {
    // Ruling R19: infer re-reads the policy on every call.
    let fx = Fx::both();
    let started = fx.path("cli.started");
    let c = client(&fx, &[("FAKE_STARTED", &started.to_string_lossy())]).await;
    let r = infer(&c, json!({"harness": "codex", "prompt": "p"})).await;
    assert!(!is_error(&r), "{r:?}");
    std::fs::remove_file(&started).unwrap();

    // The administrator disables codex while the session is open.
    let policy = std::fs::read_to_string(fx.policy()).unwrap();
    let disabled = policy.replacen(
        "[harness.codex]\nenabled = true",
        "[harness.codex]\nenabled = false",
        1,
    );
    assert_ne!(policy, disabled);
    std::fs::write(fx.policy(), &disabled).unwrap();
    let r = infer(&c, json!({"harness": "codex", "prompt": "p"})).await;
    assert_eq!(kind(&r), "not_enabled");
    assert!(
        text(&r).contains("sudo jodd-ai-host enable codex"),
        "{}",
        text(&r)
    );
    assert!(!started.exists(), "the CLI must never start");

    // A lowered limit reaches the session too.
    std::fs::write(fx.policy(), format!("max_request_bytes = 10\n{policy}")).unwrap();
    let r = infer(&c, json!({"harness": "claude", "prompt": "x".repeat(11)})).await;
    assert_eq!(kind(&r), "too_large");
}

#[test]
fn oversized_line_drops_connection_without_running() {
    // Ruling R20: a line over 4 x max_request_bytes + 64 KiB ends the
    // connection; the server never buffers it whole and runs nothing.
    let fx = Fx::with(
        None,
        on(&fake("fake-codex"), 180),
        "max_request_bytes = 100\n",
    );
    let started = fx.path("cli.started");
    let mut s = Raw::start(&fx, &[("FAKE_STARTED", &started.to_string_lossy())]);
    let line = json!({
        "jsonrpc": "2.0", "id": 5, "method": "tools/call",
        "params": {"name": "infer", "arguments": {"harness": "codex", "prompt": "x".repeat(70_000)}}
    })
    .to_string();
    assert!(line.len() > 4 * 100 + 64 * 1024);
    // The server stops reading partway: a broken pipe here is expected.
    let stdin = s.stdin.as_mut().unwrap();
    let _ = stdin
        .write_all(line.as_bytes())
        .and_then(|_| stdin.write_all(b"\n"));
    let st = s.exited_within(Duration::from_secs(10));
    assert!(st.is_some(), "the server kept the connection");
    assert_eq!(st.unwrap().code(), Some(0), "a clean exit after the drain");
    assert!(s.response(5, Duration::from_millis(300)).is_none());
    assert!(!started.exists(), "the CLI must never start");
    assert!(
        fx.log_lines().is_empty(),
        "not a call: {:?}",
        fx.log_lines()
    );
    assert_eq!(std::fs::read_to_string(fx.path("raw.stderr")).unwrap(), "");
}

#[test]
fn a_line_under_the_cap_is_still_served() {
    // The cap leaves room for JSON escaping: a prompt at max_request_bytes
    // whose every byte is escaped (\" is 2 bytes for 1) still arrives.
    let fx = Fx::with(
        None,
        on(&fake("fake-codex"), 180),
        "max_request_bytes = 100\n",
    );
    let mut s = Raw::start(&fx, &[]);
    s.call(
        4,
        "infer",
        json!({"harness": "codex", "prompt": "\"".repeat(100)}),
    );
    let r = s.response(4, QUICK).expect("no response");
    assert_ne!(r["result"]["isError"], json!(true), "{r}");
    // And garbage lines are skipped, not fatal.
    s.send(json!("not a request"));
    s.stdin.as_mut().unwrap().write_all(b"{not json\n").unwrap();
    s.send(json!({"jsonrpc": "2.0", "id": 9, "method": "tools/list"}));
    assert!(
        s.response(9, QUICK).is_some(),
        "a bad line ended the session"
    );
}
