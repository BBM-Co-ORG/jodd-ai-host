//! The legacy `jodd-ai-run 2 …` request, so today's released Jodd keeps
//! working against this server. The bar is wrapper v2 (Jodd
//! `src-tauri/src/llm/remote/wrapper.rs`, commit d514d03d): every message,
//! exit code and byte of stdout below is the wrapper's, and the comments name
//! the template line each one comes from.
//!
//! Protocol v2: stdin carries the schema line (`-` or base64), the prompt's
//! length in bytes, then exactly that many bytes. Jodd never closes stdin;
//! sshd closes it when Jodd goes away, so EOF after the prompt is the cancel.

use std::time::{Duration, Instant};

use base64::Engine as _;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio_util::sync::CancellationToken;

use crate::harness::{self, Answer};
use crate::keys::Forgot;
use crate::policy::{HarnessId, Policy};
use crate::run::{self, RunOutcome, RunRequest};
use crate::slots::Slots;

/// `JODD_AI_RUN_VERSION`.
pub const VERSION: &str = "2";

const UNKNOWN: &str = "unknown request\n";
/// Ours, not the wrapper's (it had no limit): the run is refused at once
/// rather than queued.
const BUSY: &str = "the AI host is busy\n";
/// Exit codes, as wrapper v2 uses them.
const EX_USAGE: i32 = 64;
const EX_VERSION: i32 = 65;
const EX_SOFTWARE: i32 = 70;
const EX_TIMEOUT: i32 = 124;
const EX_MISSING: i32 = 127;
const EX_PIPE: i32 = 141;
const EX_TERM: i32 = 143;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyRequest {
    pub version: String,
    pub verb: String,
    pub preset: Option<String>,
}

/// `jodd-ai-run <ver> <verb> [preset]`; `None` unless `words[0]` is
/// `jodd-ai-run`. Like the wrapper's `ver=$2 verb=$3 preset=$4`, a missing
/// word is empty and words past the fourth are ignored.
pub fn parse(words: &[&str]) -> Option<LegacyRequest> {
    if words.first() != Some(&"jodd-ai-run") {
        return None;
    }
    let word = |i: usize| words.get(i).copied().unwrap_or("").to_string();
    Some(LegacyRequest {
        version: word(1),
        verb: word(2),
        preset: words.get(3).map(|p| p.to_string()),
    })
}

/// Answer one legacy request; returns the process exit code.
pub async fn serve(
    req: LegacyRequest,
    key: Option<&str>,
    policy: &Policy,
    slots: &Slots,
    stdin: impl AsyncRead + Unpin,
    mut stdout: impl AsyncWrite + Unpin,
    mut stderr: impl AsyncWrite + Unpin,
) -> i32 {
    match req.verb.as_str() {
        // `status` and `forget` answer any version (they come before the check).
        "status" => status(policy, &mut stdout).await,
        "forget" => {
            // One metadata line, like MCP's `forget_key`: key revocation
            // belongs in the audit trail.
            let started = Instant::now();
            let (code, outcome) = forget(key, &mut stderr).await;
            crate::log::call(key, "jodd-ai-run-forget", None, started.elapsed(), outcome);
            code
        }
        "run" => {
            // One metadata line per run, as for an MCP call.
            let started = Instant::now();
            let mut harness = None;
            let (code, outcome) = run_verb(
                req,
                policy,
                slots,
                stdin,
                &mut stdout,
                &mut stderr,
                &mut harness,
            )
            .await;
            crate::log::call(key, "jodd-ai-run", harness, started.elapsed(), outcome);
            code
        }
        _ => refuse(&mut stderr, UNKNOWN, EX_USAGE).await,
    }
}

/// `echo "version=$JODD_AI_RUN_VERSION"`, then per preset
/// `printf 'preset=%s path=%s\n'`. A path is shown only for a harness the
/// policy enables AND that is installed there: wrapper v2 baked `-` for a CLI
/// the install probe did not find, and Jodd shows such a preset as missing.
async fn status(policy: &Policy, stdout: &mut (impl AsyncWrite + Unpin)) -> i32 {
    let mut text = format!("version={VERSION}\n");
    for id in HarnessId::all() {
        let path = policy
            .enabled(id)
            .map(|h| h.path.as_str())
            .filter(|p| run::is_runnable(p))
            .unwrap_or("-");
        text.push_str(&format!("preset={} path={path}\n", id.as_str()));
    }
    match write_out(stdout, text.as_bytes()).await {
        Ok(()) => 0,
        Err(_) => EX_PIPE,
    }
}

/// `case $key in [0-9a-f]{8}) ;; *) echo "no key id" >&2; exit 64 ;; esac`;
/// any failure editing the keys file is a bare `exit 70`. A missing keys
/// file is a failure too, for byte parity with wrapper v2: its `grep` exits 2 on it,
/// printing GNU grep's message, and the wrapper then exits 70.
/// Returns the exit code and the outcome the call log records.
async fn forget(key: Option<&str>, stderr: &mut (impl AsyncWrite + Unpin)) -> (i32, &'static str) {
    let Some(key) = key.filter(|k| is_key_id(k)) else {
        return (refuse(stderr, "no key id\n", EX_USAGE).await, "no_key");
    };
    match crate::keys::forget_detail(key) {
        Ok(Forgot::Removed) => (0, "removed"),
        Ok(Forgot::NotFound) => (0, "not_found"),
        Ok(Forgot::NoFile(path)) => {
            let msg = format!("grep: {}: No such file or directory\n", path.display());
            (refuse(stderr, &msg, EX_SOFTWARE).await, "no_file")
        }
        Err(_) => (EX_SOFTWARE, "error"),
    }
}

/// Exactly 8 lowercase hex digits (`jodd-ai-<id>` in the key comment).
pub fn is_key_id(k: &str) -> bool {
    k.len() == 8
        && k.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

async fn run_verb(
    req: LegacyRequest,
    policy: &Policy,
    slots: &Slots,
    stdin: impl AsyncRead + Unpin,
    stdout: &mut (impl AsyncWrite + Unpin),
    stderr: &mut (impl AsyncWrite + Unpin),
    harness: &mut Option<HarnessId>,
) -> (i32, &'static str) {
    if req.version != VERSION {
        let msg = format!(
            "version mismatch: this server has {VERSION}, Jodd sent {}\n",
            req.version
        );
        return (refuse(stderr, &msg, EX_VERSION).await, "version_mismatch");
    }
    // Refuse before reading stdin: Jodd never closes it, so a refusal that
    // waited for the prompt would hang until Jodd's deadline.
    *harness = req.preset.as_deref().and_then(HarnessId::from_name);
    let Some((id, cfg)) = harness.and_then(|id| policy.enabled(id).map(|cfg| (id, cfg))) else {
        return (refuse(stderr, UNKNOWN, EX_USAGE).await, "not_enabled");
    };

    let mut stdin = BufReader::new(stdin);
    let (schema_line, n) = match read_header(&mut stdin, policy.max_request_bytes).await {
        Some(h) => h,
        None => return (refuse(stderr, UNKNOWN, EX_USAGE).await, "bad_request"),
    };
    let mut prompt = Vec::with_capacity(n.min(1 << 20) as usize);
    let read = (&mut stdin).take(n).read_to_end(&mut prompt).await;
    if read.is_err() || prompt.len() as u64 != n {
        let msg = "the prompt was incomplete: Jodd went away during the upload\n";
        return (refuse(stderr, msg, EX_SOFTWARE).await, "incomplete");
    }
    // `printf '%s' "$schema_line" | $b64d > "$d/schema" || exit 70`
    let schema = if schema_line == b"-" {
        None
    } else {
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&schema_line)
            .ok()
            .and_then(|b| String::from_utf8(b).ok());
        match decoded {
            Some(s) => Some(s),
            None => return (EX_SOFTWARE, "bad_request"),
        }
    };
    // Only now, with the whole prompt in hand: an upload that stalls (a phone
    // losing signal; sshd may not notice for hours) must hold no slot, or two
    // stalls lock every run out. Wrapper v2 held nothing during the upload.
    let Some(_slot) = slots.try_acquire() else {
        return (refuse(stderr, BUSY, EX_SOFTWARE).await, "busy");
    };

    let def = harness::def(id);
    let timeout_secs = cfg.timeout_secs;
    let cancel = CancellationToken::new();
    // `trap 'stop; exit 143' TERM HUP INT`: registered before the CLI starts.
    let signals = run::StopSignals::register();
    let run_fut = run::run(
        RunRequest {
            def,
            cfg,
            stdin: prompt,
            schema,
            timeout: Duration::from_secs(timeout_secs),
        },
        cancel.clone(),
    );
    tokio::pin!(run_fut);
    // The watcher: EOF on stdin (or a stop signal) means the client went away.
    let client_gone = async {
        tokio::select! {
            _ = until_eof(&mut stdin) => {}
            _ = signals.any() => {}
        }
    };
    let (outcome, stopped) = tokio::select! {
        biased;
        r = &mut run_fut => (r, false),
        _ = client_gone => {
            cancel.cancel();
            ((&mut run_fut).await, true)
        }
    };

    let logged = match &outcome {
        _ if stopped => "cancelled",
        Ok(RunOutcome::Missing) => "cli_missing",
        Ok(RunOutcome::TimedOut { .. }) => "timeout",
        Ok(RunOutcome::Exited { code: 0, .. }) => "ok",
        Ok(RunOutcome::Exited { .. }) => "cli_failed",
        Ok(RunOutcome::Cancelled) => "cancelled",
        Err(_) => "error",
    };
    let code = if stopped {
        // The trap prints nothing; nobody is reading.
        EX_TERM
    } else {
        match outcome {
            Ok(RunOutcome::Missing) => {
                let msg = format!("{} is not installed on this server\n", id.as_str());
                refuse(stderr, &msg, EX_MISSING).await
            }
            // `cat "$d/stderr" >&3`, then `echo "timed out after ${timeout}s" >&3`.
            Ok(RunOutcome::TimedOut { stderr: cli_err }) => {
                let text = format!("{cli_err}timed out after {timeout_secs}s\n");
                match write_out(stderr, text.as_bytes()).await {
                    Ok(()) => EX_TIMEOUT,
                    Err(_) => EX_PIPE,
                }
            }
            // `cat "$d/stderr" >&3`, then the answer file on success of a
            // file preset, else the CLI's stdout. Raw: old Jodd unwraps it.
            Ok(RunOutcome::Exited {
                code,
                stdout: out,
                stderr: cli_err,
                out_file,
            }) => {
                let answer = if code == 0 && matches!(def.answer, Answer::LastMessageFile) {
                    out_file.unwrap_or_default()
                } else {
                    out
                };
                let wrote = async {
                    write_out(stderr, cli_err.as_bytes()).await?;
                    write_out(stdout, answer.as_bytes()).await
                };
                match wrote.await {
                    Ok(()) => code,
                    Err(_) => EX_PIPE,
                }
            }
            Ok(RunOutcome::Cancelled) => EX_TERM,
            // Scratch could not be made or read back: the wrapper's bare `exit 70`.
            Err(_) => EX_SOFTWARE,
        }
    };
    // A TERMed CLI is KILLed and its scratch removed before the process
    // exits (else a CLI that ignores TERM outlives it); the slot stays held
    // until then.
    run::drain_reaps().await;
    (code, logged)
}

/// `IFS= read -r schema_line && IFS= read -r n || n=`, then
/// `case $n in ''|*[!0-9]*|??????????*) … exit 64`. Each line must end in a
/// newline (an unterminated one is the client going away). The length is
/// also held to the policy's `max_request_bytes`.
async fn read_header(
    stdin: &mut (impl tokio::io::AsyncBufRead + Unpin),
    max: u64,
) -> Option<(Vec<u8>, u64)> {
    let schema_line = read_line(stdin, max).await?;
    let n = read_line(stdin, 16).await?;
    if n.is_empty() || n.len() > 9 || !n.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let n: u64 = std::str::from_utf8(&n).ok()?.parse().ok()?;
    (n <= max).then_some((schema_line, n))
}

/// One `\n`-terminated line of at most `limit` bytes, without the newline.
async fn read_line(r: &mut (impl tokio::io::AsyncBufRead + Unpin), limit: u64) -> Option<Vec<u8>> {
    let mut line = Vec::new();
    r.take(limit.saturating_add(1))
        .read_until(b'\n', &mut line)
        .await
        .ok()?;
    if line.pop() != Some(b'\n') {
        return None;
    }
    Some(line)
}

/// Reads and discards until EOF (or a read error).
async fn until_eof(r: &mut (impl AsyncRead + Unpin)) {
    let mut buf = [0u8; 4096];
    while let Ok(n) = r.read(&mut buf).await {
        if n == 0 {
            break;
        }
    }
}

async fn write_out(w: &mut (impl AsyncWrite + Unpin), bytes: &[u8]) -> std::io::Result<()> {
    w.write_all(bytes).await?;
    w.flush().await
}

/// A message on stderr, then `code`. A client that is no longer reading
/// changes nothing: the exit code is the answer.
async fn refuse(stderr: &mut (impl AsyncWrite + Unpin), msg: &str, code: i32) -> i32 {
    let _ = write_out(stderr, msg.as_bytes()).await;
    code
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_takes_the_wrappers_positional_words() {
        assert_eq!(parse(&["id"]), None);
        assert_eq!(parse(&[]), None);
        assert_eq!(
            parse(&["jodd-ai-run", "2", "run", "codex", "extra"]),
            Some(LegacyRequest {
                version: "2".into(),
                verb: "run".into(),
                preset: Some("codex".into())
            })
        );
        assert_eq!(
            parse(&["jodd-ai-run"]),
            Some(LegacyRequest {
                version: "".into(),
                verb: "".into(),
                preset: None
            })
        );
    }

    #[test]
    fn key_ids_are_eight_lowercase_hex() {
        assert!(is_key_id("3f2a9b1c"));
        for bad in [
            "3F2A9B1C",
            "3f2a9b1",
            "3f2a9b1c0",
            "3f2a9b1g",
            "",
            "../../xx",
        ] {
            assert!(!is_key_id(bad), "{bad}");
        }
    }

    #[tokio::test]
    async fn header_lengths_follow_the_wrappers_case() {
        async fn h(input: &[u8], max: u64) -> Option<u64> {
            read_header(&mut BufReader::new(input), max)
                .await
                .map(|(_, n)| n)
        }
        assert_eq!(h(b"-\n0\n", 10).await, Some(0));
        assert_eq!(h(b"-\n007\nabcdefg", 10).await, Some(7));
        assert_eq!(h(b"-\n999999999\n", u64::MAX).await, Some(999_999_999));
        for bad in [
            &b"-\n1000000000\n"[..],
            b"-\n\n",
            b"-\n 3\n",
            b"-\n3x\n",
            b"-\n-1\n",
            b"-\n12",
            b"-",
            b"",
        ] {
            assert_eq!(
                h(bad, u64::MAX).await,
                None,
                "{:?}",
                String::from_utf8_lossy(bad)
            );
        }
        assert_eq!(h(b"-\n11\n", 10).await, None, "over max_request_bytes");
    }
}
