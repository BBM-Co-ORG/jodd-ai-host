//! jodd-ai-mcp: the server half of jodd-ai-host, run by sshd as the AI
//! account's forced command (`command="jodd-ai-mcp --key <id>"`).
//!
//! The request words come from `SSH_ORIGINAL_COMMAND` (what the client asked
//! for) when sshd set it, else from the argv after `--key <id>`. `--key` is
//! read from the fixed argv only, never from the client's words.

mod admin;
mod harness;
mod keys;
mod legacy;
mod log;
mod mcp;
mod policy;
mod run;
mod slots;

use std::path::PathBuf;

const UNKNOWN: &str = "unknown request";
const EX_USAGE: i32 = 64;

/// A `JODD_AI_MCP_*` runtime override, honoured only in debug builds: a
/// release build must not let the AI account point a root-installed
/// server at a policy or lock dir of its own via its shell rc.
pub(crate) fn test_override(name: &str) -> Option<String> {
    if cfg!(debug_assertions) {
        std::env::var(name).ok().filter(|v| !v.is_empty())
    } else {
        None
    }
}

fn policy_path() -> PathBuf {
    test_override("JODD_AI_MCP_POLICY")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(policy::DEFAULT_POLICY_PATH))
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--version") {
        println!("jodd-ai-mcp {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    let Ok(rt) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        std::process::exit(70);
    };
    let code = rt.block_on(dispatch(args));
    // Exit with the runtime still alive: dropping it would wait for the
    // blocking thread still reading stdin, and Jodd never closes stdin.
    std::process::exit(code)
}

async fn dispatch(args: Vec<String>) -> i32 {
    let (key, rest) = match args.split_first() {
        Some((flag, rest)) if flag == "--key" => match rest.split_first() {
            Some((k, rest)) => (Some(k.as_str()), rest),
            None => (None, &[][..]),
        },
        _ => (None, &args[..]),
    };
    // `--key` in argv means the restricted key's forced command.
    let keyed = args.first().map(String::as_str) == Some("--key");
    let original =
        std::env::var_os("SSH_ORIGINAL_COMMAND").map(|s| s.to_string_lossy().into_owned());
    let forced = original.is_some();
    let words: Vec<&str> = match &original {
        // `set -f; set -- $SSH_ORIGINAL_COMMAND`: split on the shell's IFS,
        // with no glob, quote or variable expansion.
        Some(text) => text
            .split([' ', '\t', '\n'])
            .filter(|w| !w.is_empty())
            .collect(),
        None => rest.iter().map(String::as_str).collect(),
    };

    match words.first().copied() {
        Some("jodd-ai-run") => {
            let Some(req) = legacy::parse(&words) else {
                return unknown();
            };
            let policy = policy::Policy::effective(&policy::load(&policy_path()));
            let slots = slots::Slots::new(policy.max_concurrent_runs);
            let code = legacy::serve(
                req,
                key,
                &policy,
                &slots,
                tokio::io::stdin(),
                tokio::io::stdout(),
                tokio::io::stderr(),
            )
            .await;
            // Every pending reap before the exit, or a CLI that ignores TERM
            // outlives us (serve drains too; this covers any path that did not).
            run::drain_reaps().await;
            code
        }
        // Exactly the one word: a request this server does not know (a
        // future client's extra word) must fail loudly, not be served as
        // something else.
        Some("jodd-ai-mcp") if words.len() == 1 => {
            let code = mcp::serve(key, &policy_path()).await;
            // Every pending reap before the exit, or a CLI that ignores TERM
            // outlives us (serve drains too; this covers any path that did not).
            run::drain_reaps().await;
            code
        }
        // The admin surface, never through the restricted key: not under a
        // forced command (SSH_ORIGINAL_COMMAND) and not with `--key` in argv.
        Some("admin") if !forced && !keyed => admin::run(&words[1..], &policy_path()),
        _ => unknown(),
    }
}

fn unknown() -> i32 {
    eprintln!("{UNKNOWN}");
    EX_USAGE
}
