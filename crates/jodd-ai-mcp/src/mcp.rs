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
//! and by stdin closing (the SSH channel going away). Before the process exits
//! every pending reap is awaited (ruling R9).

use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use rmcp::model::{
    CallToolRequestParams, CallToolResult, ContentBlock, Implementation, JsonObject,
    ListToolsResult, Meta, PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler, ServiceExt};
use serde_json::{json, Value};
use tokio::io::{AsyncRead, ReadBuf};
use tokio_util::sync::CancellationToken;

use crate::harness;
use crate::policy::{HarnessId, LoadOutcome, Policy};
use crate::run::{self, RunOutcome, RunRequest};
use crate::slots::Slots;

pub const SERVER_NAME: &str = "jodd-ai-host";
/// `_meta` key naming each tool's contract version (permanent from v1).
pub const TOOL_VERSION_KEY: &str = "jodd-ai-host/tool-version";
/// `infer` and `forget_key` are both at version 1.
const TOOL_VERSION: u64 = 1;
/// The client never completed an MCP handshake (`EX_PROTOCOL`).
const EX_PROTOCOL: i32 = 76;

const INFER_ARGS: [&str; 5] = ["harness", "prompt", "system", "schema", "timeout_secs"];
const DOCTOR: &str = "On the host, run `jodd-ai-host doctor`.";

/// Serve one MCP session on stdin/stdout; returns the process exit code.
pub async fn serve(key: Option<&str>, load: LoadOutcome) -> i32 {
    let policy = Policy::effective(&load);
    // Cancelled when stdin ends: every run of this session stops with it.
    let session = CancellationToken::new();
    // Cancelled by `forget_key`: the server exits once its answer is out.
    let quit = CancellationToken::new();
    let server = Server {
        key: key.map(str::to_string),
        slots: Slots::new(policy.max_concurrent_runs),
        policy,
        load,
        session: session.clone(),
        quit: quit.clone(),
    };
    let stdin = EofCancels {
        inner: tokio::io::stdin(),
        on_eof: session.clone(),
    };
    let code = match server.serve((stdin, tokio::io::stdout())).await {
        Ok(service) => {
            let stop = service.cancellation_token();
            // rmcp's cancel path still sends every response already made
            // (forget_key's among them) before it closes stdout.
            let watcher = tokio::spawn(async move {
                quit.cancelled().await;
                stop.cancel();
            });
            let _ = service.waiting().await;
            watcher.abort();
            0
        }
        // stdin closed, or the first message was not `initialize`.
        Err(_) => EX_PROTOCOL,
    };
    session.cancel();
    run::drain_reaps().await;
    code
}

struct Server {
    key: Option<String>,
    load: LoadOutcome,
    policy: Policy,
    slots: Slots,
    session: CancellationToken,
    quit: CancellationToken,
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
            .filter(|h| self.policy.enabled(*h).is_some())
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
        let a = match parse_infer(args) {
            Ok(a) => a,
            Err(msg) => return Done::invalid(None, msg),
        };
        let known = HarnessId::from_name(&a.harness);
        let (id, cfg) = match self.resolve(known) {
            Ok(x) => x,
            Err(text) => return Done::failed(known, "not_enabled", text, json!({})),
        };

        let stdin = harness::join_prompt(a.system.as_deref(), &a.prompt);
        let size = stdin.len() as u64 + a.schema.as_ref().map_or(0, |s| s.len() as u64);
        let max = self.policy.max_request_bytes;
        if size > max {
            let text = format!(
                "the request is {size} bytes; this AI host accepts at most {max} \
                 (prompt, system text and schema together)"
            );
            return Done::failed(Some(id), "too_large", text, json!({}));
        }

        // Only now, right before the CLI starts; a full house is refused at
        // once, never queued.
        let Some(_slot) = self.slots.try_acquire() else {
            let text = format!(
                "the AI host is busy: its {} run slot(s) are all taken. Try again shortly.",
                self.policy.max_concurrent_runs
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

    /// The enabled harness's policy, or the `not_enabled` text: doctor for a
    /// policy that did not load, the enable command for a known harness.
    fn resolve(
        &self,
        known: Option<HarnessId>,
    ) -> Result<(HarnessId, &crate::policy::HarnessPolicy), String> {
        match (&self.load, known) {
            (LoadOutcome::Missing, _) => Err(format!(
                "this AI host has no policy file, so nothing is enabled. {DOCTOR}"
            )),
            (LoadOutcome::Invalid(_), _) => Err(format!(
                "this AI host's policy file is invalid, so nothing is enabled. {DOCTOR}"
            )),
            (LoadOutcome::Loaded(_), None) => Err(format!(
                "this AI host does not offer that harness. {DOCTOR}"
            )),
            (LoadOutcome::Loaded(_), Some(id)) => match self.policy.enabled(id) {
                Some(cfg) => Ok((id, cfg)),
                None => Err(format!(
                    "{0} is not enabled on this AI host. Ask its administrator to run \
                     `sudo jodd-ai-host enable {0}` (`jodd-ai-host doctor` shows why).",
                    id.as_str()
                )),
            },
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
                "description": "Optional JSON Schema for the harness's structured-output mode."
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
