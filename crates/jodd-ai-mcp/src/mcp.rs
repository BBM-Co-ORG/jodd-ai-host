//! The MCP server (spec §7): `infer` and `forget_key` over stdin/stdout.
//!
//! `ServerHandler` is implemented by hand: the `harness` enum in `infer`'s
//! input schema is built per session from the policy, which rmcp's macros
//! (static schemas) cannot express.
//!
//! The wire contract here is what `docs/PROTOCOL.md` documents and every
//! client depends on:
//! - `serverInfo.name` is `jodd-ai-host`, `serverInfo.version` this crate's version;
//! - `tools/list` holds `infer` only while a harness is enabled, `forget_key`
//!   always; each tool's `_meta` carries `jodd-ai-host/tool-version`;
//! - a tool that ran and failed is a `CallToolResult` with `isError: true` and
//!   `structuredContent: {kind, exit_code?, stdout?, stderr?}`; only
//!   malformed arguments are JSON-RPC invalid-params errors. Clients can only
//!   narrow, so an argument this host does not know is refused, never ignored.
//!
//! A run is cancelled (its CLI's process group TERMed, KILLed 5 s later, its
//! scratch removed) by the client's `notifications/cancelled` for that request,
//! by stdin closing (the SSH channel going away), and by TERM, HUP or INT to
//! the server (a container stop, a `pkill`), which also ends the session with
//! exit 143 as the legacy shim does. Before the process exits every pending
//! reap is awaited (ruling R9).
//!
//! `tools/list` shows the policy as it was when the session started, but
//! every `infer` re-reads the policy file and obeys that (ruling R19): an
//! administrator's `disable` or lowered limit reaches a live session at its
//! next call.
//!
//! Each incoming line is capped at `4 × max_request_bytes + 64 KiB` (ruling
//! R20): a longer line ends the connection, so a client cannot make the
//! server buffer without bound before `too_large` could even be decided.

use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use futures::StreamExt;

use rmcp::model::{
    CallToolRequestParams, CallToolResult, ContentBlock, Implementation, JsonObject,
    ListToolsResult, Meta, PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::{RequestContext, RxJsonRpcMessage, TxJsonRpcMessage};
use rmcp::transport::async_rw::{JsonRpcMessageCodec, JsonRpcMessageCodecError};
use rmcp::{ErrorData, RoleServer, ServerHandler, ServiceExt};
use serde_json::{json, Value};
use tokio::io::{AsyncRead, ReadBuf};
use tokio_util::bytes::BytesMut;
use tokio_util::codec::{Decoder, FramedRead, FramedWrite};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::harness;
use crate::policy::{self, HarnessId, HarnessPolicy, LoadOutcome, Policy};
use crate::run::{self, RunOutcome, RunRequest};
use crate::slots::Slots;

pub const SERVER_NAME: &str = "jodd-ai-host";
/// `_meta` key naming each tool's contract version (permanent from v1).
pub const TOOL_VERSION_KEY: &str = "jodd-ai-host/tool-version";
/// `infer` and `forget_key` are both at version 1.
const TOOL_VERSION: u64 = 1;
/// The client never completed an MCP handshake (`EX_PROTOCOL`).
const EX_PROTOCOL: i32 = 76;
/// Stopped by TERM, HUP or INT (wrapper v2's `exit 143`).
const EX_TERM: i32 = 143;
/// How long a stopping session waits for its in-flight calls to notice.
const CALLS_STOP_WAIT: Duration = Duration::from_secs(3);

const INFER_ARGS: [&str; 5] = ["harness", "prompt", "system", "schema", "timeout_secs"];
const DOCTOR: &str = "On the host, run `jodd-ai-host doctor`.";

/// The longest request line accepted, from the session's policy (ruling R20).
pub fn line_cap(max_request_bytes: u64) -> usize {
    let cap = max_request_bytes
        .saturating_mul(4)
        .saturating_add(64 * 1024);
    usize::try_from(cap).unwrap_or(usize::MAX)
}

/// Serve one MCP session on stdin/stdout; returns the process exit code.
pub async fn serve(key: Option<&str>, policy_path: &Path) -> i32 {
    // Registered first, for the whole session: from here on these signals no
    // longer kill the process outright, so they must always end it this way.
    let signals = run::StopSignals::register();
    let listed = Policy::effective(&policy::load(policy_path));
    // Cancelled when stdin ends (or a line is over the cap) or a stop signal
    // arrives: every run of this session stops with it.
    let session = CancellationToken::new();
    // Cancelled by `forget_key`: the server exits once its answer is out.
    let quit = CancellationToken::new();
    // Cancelled by TERM, HUP or INT, after `session`.
    let stopped = CancellationToken::new();
    {
        let (session, stopped) = (session.clone(), stopped.clone());
        tokio::spawn(async move {
            signals.any().await;
            session.cancel();
            stopped.cancel();
        });
    }
    // Every `infer` in flight, so the session can wait for each to see the
    // cancel (and hand its CLI to a reap) before the reaps are drained.
    let calls = TaskTracker::new();
    let cap = line_cap(listed.max_request_bytes);
    let server = Server {
        key: key.map(str::to_string),
        policy_path: policy_path.to_path_buf(),
        listed,
        session: session.clone(),
        quit: quit.clone(),
        calls: calls.clone(),
    };
    // rmcp's own stdio transport reads with an unbounded `read_until` (its
    // codec's limit applies only to `FramedRead`), so read through
    // `FramedRead` with the capped codec ourselves.
    let stdin = EofCancels {
        inner: tokio::io::stdin(),
        on_eof: session.clone(),
    };
    let over_cap = session.clone();
    let incoming = FramedRead::new(stdin, CappedCodec::new(cap)).filter_map(move |m| {
        // `FramedRead` ends after its first error: an over-cap line (or a
        // read error) closes the connection.
        std::future::ready(match m {
            Ok(m) => Some(m),
            Err(_) => {
                over_cap.cancel();
                None
            }
        })
    });
    let outgoing = FramedWrite::new(
        tokio::io::stdout(),
        JsonRpcMessageCodec::<TxJsonRpcMessage<RoleServer>>::default(),
    );
    let code = tokio::select! {
        served = server.serve((outgoing, incoming)) => match served {
            Ok(service) => {
                let stop = service.cancellation_token();
                // rmcp's cancel path still sends every response already made
                // (forget_key's among them) before it closes stdout.
                let on_stop = stopped.clone();
                let watcher = tokio::spawn(async move {
                    tokio::select! {
                        _ = quit.cancelled() => {}
                        _ = on_stop.cancelled() => {}
                    }
                    stop.cancel();
                });
                let _ = service.waiting().await;
                watcher.abort();
                0
            }
            // stdin closed, or the first message was not `initialize`.
            Err(_) => EX_PROTOCOL,
        },
        // A stop signal before the handshake finished: no run has started.
        _ = stopped.cancelled() => EX_TERM,
    };
    let code = if stopped.is_cancelled() {
        EX_TERM
    } else {
        code
    };
    session.cancel();
    calls.close();
    let _ = tokio::time::timeout(CALLS_STOP_WAIT, calls.wait()).await;
    run::drain_reaps().await;
    code
}

struct Server {
    key: Option<String>,
    /// Re-read by every `infer` (ruling R19).
    policy_path: PathBuf,
    /// The policy at session start: what `tools/list` shows.
    listed: Policy,
    session: CancellationToken,
    quit: CancellationToken,
    /// Each `infer` holds a token of this while it runs.
    calls: TaskTracker,
}

/// One finished call: what the client gets and what the log says.
struct Done {
    harness: Option<HarnessId>,
    outcome: &'static str,
    result: Result<CallToolResult, ErrorData>,
}

impl Done {
    fn invalid(harness: Option<HarnessId>, msg: impl Into<String>) -> Done {
        Done {
            harness,
            outcome: "invalid_params",
            result: Err(ErrorData::invalid_params(msg.into(), None)),
        }
    }

    /// A tool-level error: `isError: true`, `structuredContent.kind = kind`.
    fn failed(harness: Option<HarnessId>, kind: &'static str, text: String, extra: Value) -> Done {
        let mut structured = json!({ "kind": kind });
        if let (Value::Object(s), Value::Object(e)) = (&mut structured, extra) {
            s.extend(e);
        }
        let mut r = CallToolResult::error(vec![ContentBlock::text(text)]);
        r.structured_content = Some(structured);
        Done {
            harness,
            outcome: kind,
            result: Ok(r),
        }
    }

    fn ok(
        harness: Option<HarnessId>,
        outcome: &'static str,
        text: String,
        structured: Value,
    ) -> Done {
        let mut r = CallToolResult::success(vec![ContentBlock::text(text)]);
        r.structured_content = Some(structured);
        Done {
            harness,
            outcome,
            result: Ok(r),
        }
    }
}

impl ServerHandler for Server {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(SERVER_NAME, env!("CARGO_PKG_VERSION")))
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let enabled: Vec<HarnessId> = HarnessId::all()
            .into_iter()
            .filter(|h| self.listed.enabled(*h).is_some())
            .collect();
        let mut tools = Vec::new();
        if !enabled.is_empty() {
            tools.push(infer_tool(&enabled));
        }
        tools.push(forget_key_tool());
        Ok(ListToolsResult::with_all_items(tools))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let started = Instant::now();
        let args = request.arguments.as_ref();
        let (tool, done) = match request.name.as_ref() {
            "infer" => ("infer", self.infer(args, &context.ct).await),
            "forget_key" => ("forget_key", self.forget_key(args)),
            // Never log the client's text: an unknown name is just "unknown".
            _ => ("unknown", Done::invalid(None, "unknown tool")),
        };
        crate::log::call(
            self.key.as_deref(),
            tool,
            done.harness,
            started.elapsed(),
            done.outcome,
        );
        done.result
    }
}

impl Server {
    async fn infer(&self, args: Option<&JsonObject>, cancelled: &CancellationToken) -> Done {
        let _in_flight = self.calls.token();
        let a = match parse_infer(args) {
            Ok(a) => a,
            Err(msg) => return Done::invalid(None, msg),
        };
        // Every call obeys the policy as it is now, not as it was listed.
        let load = policy::load(&self.policy_path);
        let policy = Policy::effective(&load);
        let known = HarnessId::from_name(&a.harness);
        let (id, cfg) = match resolve(&load, &policy, known) {
            Ok(x) => x,
            Err(text) => return Done::failed(known, "not_enabled", text, json!({})),
        };

        let stdin = harness::join_prompt(a.system.as_deref(), &a.prompt);
        let size = stdin.len() as u64 + a.schema.as_ref().map_or(0, |s| s.len() as u64);
        let max = policy.max_request_bytes;
        if size > max {
            let text = format!(
                "the request is {size} bytes; this AI host accepts at most {max} \
                 (prompt, system text and schema together)"
            );
            return Done::failed(Some(id), "too_large", text, json!({}));
        }

        // Only now, right before the CLI starts; a full house is refused at
        // once, never queued.
        // Slots are counted per call, so a lowered limit applies at once.
        let Some(_slot) = Slots::new(policy.max_concurrent_runs).try_acquire() else {
            let text = format!(
                "the AI host is busy: its {} run slot(s) are all taken. Try again shortly.",
                policy.max_concurrent_runs
            );
            return Done::failed(Some(id), "busy", text, json!({}));
        };

        // A request may only lower the policy's timeout, never raise it.
        let secs = a
            .timeout_secs
            .map_or(cfg.timeout_secs, |t| t.min(cfg.timeout_secs));
        let def = harness::def(id);
        let run_ct = self.session.child_token();
        let run_fut = run::run(
            RunRequest {
                def,
                cfg,
                stdin: stdin.into_bytes(),
                schema: a.schema,
                timeout: Duration::from_secs(secs),
            },
            run_ct.clone(),
        );
        tokio::pin!(run_fut);
        let outcome = tokio::select! {
            biased;
            r = &mut run_fut => r,
            // `notifications/cancelled` for this request.
            _ = cancelled.cancelled() => {
                run_ct.cancel();
                run_fut.await
            }
        };

        let name = id.as_str();
        match outcome {
            Ok(RunOutcome::Exited {
                code: 0,
                stdout,
                stderr,
                out_file,
            }) => match harness::extract_answer(def, &stdout, out_file.as_deref()) {
                Ok(answer) => Done::ok(
                    Some(id),
                    "ok",
                    answer,
                    json!({ "harness": name, "exit_code": 0 }),
                ),
                Err(reason) => Done::failed(
                    Some(id),
                    "cli_failed",
                    format!("{name} finished, but its answer could not be read: {reason}"),
                    json!({ "exit_code": 0, "stdout": stdout, "stderr": stderr }),
                ),
            },
            Ok(RunOutcome::Exited {
                code,
                stdout,
                stderr,
                ..
            }) => Done::failed(
                Some(id),
                "cli_failed",
                format!("{name} exited with code {code}"),
                json!({ "exit_code": code, "stdout": stdout, "stderr": stderr }),
            ),
            Ok(RunOutcome::TimedOut { stderr }) => Done::failed(
                Some(id),
                "timeout",
                format!("{name} timed out after {secs}s"),
                json!({ "stderr": stderr }),
            ),
            Ok(RunOutcome::Missing) => Done::failed(
                Some(id),
                "cli_missing",
                format!("{name} is not installed where this AI host's policy says. {DOCTOR}"),
                json!({}),
            ),
            // Nobody reads this: rmcp drops the response to a cancelled
            // request, and a closed stdin means the client is gone.
            Ok(RunOutcome::Cancelled) => Done {
                harness: Some(id),
                outcome: "cancelled",
                result: Err(ErrorData::internal_error("the request was cancelled", None)),
            },
            // The scratch dir could not be made or read back.
            Err(e) => Done::failed(
                Some(id),
                "cli_failed",
                format!("the AI host could not run {name}: {e}. {DOCTOR}"),
                json!({}),
            ),
        }
    }

    /// Always available, never subject to policy: a client must always be able
    /// to revoke itself. The server exits once the answer is out.
    fn forget_key(&self, args: Option<&JsonObject>) -> Done {
        if args.is_some_and(|a| !a.is_empty()) {
            return Done::invalid(None, "forget_key takes no arguments");
        }
        let done = match self.key.as_deref().filter(|k| crate::legacy::is_key_id(k)) {
            None => Done::failed(
                None,
                "no_key",
                format!(
                    "this connection carries no key id (the forced command lacks \
                     `--key <id>`), so there is nothing to forget. {DOCTOR}"
                ),
                json!({}),
            ),
            Some(k) => match crate::keys::forget(k) {
                Ok(true) => Done::ok(
                    None,
                    "removed",
                    "this device's key was removed from the AI host".into(),
                    json!({ "removed": true }),
                ),
                Ok(false) => Done::ok(
                    None,
                    "not_found",
                    "the AI host has no key line for this device".into(),
                    json!({ "removed": false }),
                ),
                Err(e) => Done::failed(
                    None,
                    "forget_failed",
                    format!("could not edit authorized_keys: {e}. {DOCTOR}"),
                    json!({}),
                ),
            },
        };
        self.quit.cancel();
        done
    }
}

/// The enabled harness's policy, or the `not_enabled` text: doctor for a
/// policy that did not load, the enable command for a known harness.
fn resolve<'p>(
    load: &LoadOutcome,
    policy: &'p Policy,
    known: Option<HarnessId>,
) -> Result<(HarnessId, &'p HarnessPolicy), String> {
    match (load, known) {
        (LoadOutcome::Missing, _) => Err(format!(
            "this AI host has no policy file, so nothing is enabled. {DOCTOR}"
        )),
        (LoadOutcome::Invalid(_), _) => Err(format!(
            "this AI host's policy file is invalid, so nothing is enabled. {DOCTOR}"
        )),
        (LoadOutcome::Loaded(_), None) => Err(format!(
            "this AI host does not offer that harness. {DOCTOR}"
        )),
        (LoadOutcome::Loaded(_), Some(id)) => match policy.enabled(id) {
            Some(cfg) => Ok((id, cfg)),
            None => Err(format!(
                "{0} is not enabled on this AI host. Ask its administrator to run \
                 `sudo jodd-ai-host enable {0}` (`jodd-ai-host doctor` shows why).",
                id.as_str()
            )),
        },
    }
}

#[derive(Debug, PartialEq)]
struct InferArgs {
    harness: String,
    prompt: String,
    system: Option<String>,
    /// The schema as the JSON text the CLI gets.
    schema: Option<String>,
    timeout_secs: Option<u64>,
}

/// Malformed arguments are the only invalid-params errors. A `harness` that
/// is a string but not enabled is NOT malformed (a stale client): that is
/// `not_enabled`, decided later. `null` counts as absent for optional ones.
fn parse_infer(args: Option<&JsonObject>) -> Result<InferArgs, String> {
    let empty = JsonObject::new();
    let a = args.unwrap_or(&empty);
    if let Some(k) = a.keys().find(|k| !INFER_ARGS.contains(&k.as_str())) {
        return Err(format!(
            "unknown argument `{}`: this AI host does not support it",
            k.chars().take(64).collect::<String>()
        ));
    }
    let string = |name: &str| -> Result<Option<String>, String> {
        match a.get(name) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) => Ok(Some(s.clone())),
            Some(_) => Err(format!("`{name}` must be a string")),
        }
    };
    let required = |name: &str| -> Result<String, String> {
        string(name)?.ok_or_else(|| format!("`{name}` is required"))
    };
    let schema = match a.get("schema") {
        None | Some(Value::Null) => None,
        Some(v @ Value::Object(_)) => Some(v.to_string()),
        Some(_) => return Err("`schema` must be a JSON object (a JSON Schema)".into()),
    };
    let timeout_secs = match a.get("timeout_secs") {
        None | Some(Value::Null) => None,
        Some(v) => match v.as_u64() {
            Some(n) if n >= 1 => Some(n),
            _ => return Err("`timeout_secs` must be an integer of at least 1".into()),
        },
    };
    Ok(InferArgs {
        harness: required("harness")?,
        prompt: required("prompt")?,
        system: string("system")?,
        schema,
        timeout_secs,
    })
}

fn tool_meta() -> Meta {
    let mut m = Meta::new();
    m.insert(TOOL_VERSION_KEY.into(), json!(TOOL_VERSION));
    m
}

fn object(v: Value) -> JsonObject {
    match v {
        Value::Object(m) => m,
        _ => unreachable!("tool schemas are objects"),
    }
}

fn infer_tool(enabled: &[HarnessId]) -> Tool {
    let names: Vec<&str> = enabled.iter().map(|h| h.as_str()).collect();
    let schema = json!({
        "type": "object",
        "properties": {
            "harness": {
                "type": "string",
                "enum": names,
                "description": "Which CLI runs the prompt; only the harnesses this host enables."
            },
            "prompt": {
                "type": "string",
                "description": "The prompt, fed to the CLI on stdin."
            },
            "system": {
                "type": "string",
                "description": "Optional system text, sent ahead of the prompt as \
                                `{system}\\n\\n---\\n\\n{prompt}`."
            },
            "schema": {
                "type": "object",
                "description": "Optional JSON Schema for the harness's structured-output mode. Used by Codex; Claude Code ignores it (it still counts toward max_request_bytes)."
            },
            "timeout_secs": {
                "type": "integer",
                "minimum": 1,
                "description": "Optional; may only lower this host's limit for the harness."
            }
        },
        "required": ["harness", "prompt"],
        "additionalProperties": false
    });
    Tool::new(
        "infer",
        "Run one prompt through Claude Code or Codex on this host and return the answer text.",
        object(schema),
    )
    .with_meta(tool_meta())
}

fn forget_key_tool() -> Tool {
    let schema = json!({
        "type": "object",
        "properties": {},
        "additionalProperties": false
    });
    Tool::new(
        "forget_key",
        "Remove this device's key from the AI host, then end the session.",
        object(schema),
    )
    .with_meta(tool_meta())
}

/// rmcp's line codec, with unparsable lines skipped as rmcp's own stdio
/// transport does (an invalid line is not worth a dropped session); only an
/// over-cap line or a read error is an error, and that ends the stream.
struct CappedCodec {
    inner: JsonRpcMessageCodec<RxJsonRpcMessage<RoleServer>>,
}

impl CappedCodec {
    fn new(max_line: usize) -> Self {
        CappedCodec {
            inner: JsonRpcMessageCodec::new_with_max_length(max_line),
        }
    }
}

impl Decoder for CappedCodec {
    type Item = RxJsonRpcMessage<RoleServer>;
    type Error = JsonRpcMessageCodecError;

    fn decode(&mut self, buf: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        loop {
            match self.inner.decode(buf) {
                // The bad line is already consumed: try the next one.
                Err(JsonRpcMessageCodecError::Serde(_)) => continue,
                // A skipped (non-MCP) notification returns `None` even when
                // more whole lines are buffered: go on to them.
                Ok(None) if buf.contains(&b'\n') => continue,
                other => return other,
            }
        }
    }

    fn decode_eof(&mut self, buf: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        loop {
            match self.inner.decode_eof(buf) {
                Err(JsonRpcMessageCodecError::Serde(_)) => continue,
                Ok(None) if buf.contains(&b'\n') => continue,
                other => return other,
            }
        }
    }
}

/// stdin that cancels `on_eof` when it ends (or fails): the client is gone,
/// so every run of the session stops now, not after rmcp's drain.
struct EofCancels<R> {
    inner: R,
    on_eof: CancellationToken,
}

impl<R: AsyncRead + Unpin> AsyncRead for EofCancels<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let r = Pin::new(&mut self.inner).poll_read(cx, buf);
        match &r {
            Poll::Ready(Ok(())) if buf.filled().len() == before && buf.remaining() > 0 => {
                self.on_eof.cancel()
            }
            Poll::Ready(Err(_)) => self.on_eof.cancel(),
            _ => {}
        }
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    fn obj(v: Value) -> JsonObject {
        object(v)
    }

    #[test]
    fn parse_takes_the_documented_arguments() {
        let a = parse_infer(Some(&obj(json!({
            "harness": "codex", "prompt": "p", "system": "s",
            "schema": {"type": "object"}, "timeout_secs": 30
        }))))
        .unwrap();
        assert_eq!(
            a,
            InferArgs {
                harness: "codex".into(),
                prompt: "p".into(),
                system: Some("s".into()),
                schema: Some(r#"{"type":"object"}"#.into()),
                timeout_secs: Some(30),
            }
        );
        let a = parse_infer(Some(&obj(json!({
            "harness": "pi", "prompt": "", "system": null, "schema": null, "timeout_secs": null
        }))))
        .unwrap();
        assert_eq!(a.harness, "pi", "an unknown harness is not malformed");
        assert_eq!((a.system, a.schema, a.timeout_secs), (None, None, None));
    }

    #[test]
    fn parse_refuses_malformed_arguments() {
        for bad in [
            json!({}),
            json!({"harness": "codex"}),
            json!({"harness": "codex", "prompt": ["p"]}),
            json!({"harness": "codex", "prompt": "p", "schema": "{}"}),
            json!({"harness": "codex", "prompt": "p", "timeout_secs": 0}),
            json!({"harness": "codex", "prompt": "p", "timeout_secs": 2.5}),
            json!({"harness": "codex", "prompt": "p", "extra": 1}),
        ] {
            assert!(parse_infer(Some(&obj(bad.clone()))).is_err(), "{bad}");
        }
        assert!(parse_infer(None).is_err());
    }

    #[test]
    fn tools_carry_their_version() {
        for t in [infer_tool(&[HarnessId::Codex]), forget_key_tool()] {
            assert_eq!(t.meta.unwrap().get(TOOL_VERSION_KEY), Some(&json!(1)));
        }
    }

    #[tokio::test]
    async fn eof_cancels_the_session() {
        let token = CancellationToken::new();
        let mut r = EofCancels {
            inner: &b"abc"[..],
            on_eof: token.clone(),
        };
        let mut buf = [0u8; 2];
        assert_eq!(r.read(&mut buf).await.unwrap(), 2);
        assert!(!token.is_cancelled());
        assert_eq!(r.read(&mut buf).await.unwrap(), 1);
        assert!(!token.is_cancelled());
        assert_eq!(r.read(&mut buf).await.unwrap(), 0);
        assert!(token.is_cancelled());
    }
}
