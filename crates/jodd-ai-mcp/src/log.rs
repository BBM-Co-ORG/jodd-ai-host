//! The one metadata line per call: key id, tool, harness, duration, outcome.
//! Never the prompt, the system text, the schema or the answer.
//!
//! Never stderr: under a forced command, stderr is the SSH channel back to
//! the client. Where it goes:
//! - syslog (ident `jodd-ai-mcp`) when a syslog socket exists (`/dev/log`,
//!   `/var/run/syslog`);
//! - else appended to `/var/log/jodd-ai-host/calls.log` (the Docker image runs
//!   no syslog daemon; the installer or entrypoint creates the directory,
//!   owned by the AI account, mode 0700);
//! - in debug builds only (a release build ignores every test override),
//!   `JODD_AI_MCP_LOG_FILE` overrides both, so tests never touch the system log.
//!
//! Logging failures are ignored: a call never fails because it could not be
//! logged.

use std::ffi::CString;
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::Once;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::policy::HarnessId;

const SYSLOG_SOCKETS: [&str; 2] = ["/dev/log", "/var/run/syslog"];
const FALLBACK_FILE: &str = "/var/log/jodd-ai-host/calls.log";

/// Record one call. `tool` and `outcome` must be names this binary chose (never
/// client text); `key` is printed only when it is a well-formed key id.
pub fn call(
    key: Option<&str>,
    tool: &str,
    harness: Option<HarnessId>,
    dur: Duration,
    outcome: &str,
) {
    let line = format_line(key, tool, harness, dur, outcome);
    if let Some(file) = crate::test_override("JODD_AI_MCP_LOG_FILE") {
        append(Path::new(&file), &line);
    } else if SYSLOG_SOCKETS.iter().any(|s| Path::new(s).exists()) {
        syslog(&line);
    } else {
        append(Path::new(FALLBACK_FILE), &line);
    }
}

fn format_line(
    key: Option<&str>,
    tool: &str,
    harness: Option<HarnessId>,
    dur: Duration,
    outcome: &str,
) -> String {
    let key = match key {
        Some(k) if crate::legacy::is_key_id(k) => k,
        Some(_) => "invalid",
        None => "-",
    };
    format!(
        "key={key} tool={} harness={} dur_ms={} outcome={}",
        word(tool),
        harness.map(|h| h.as_str()).unwrap_or("-"),
        dur.as_millis(),
        word(outcome),
    )
}

/// Belt and braces: whatever a caller passes, one line of plain words.
fn word(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '?'
            }
        })
        .collect()
}

fn syslog(line: &str) {
    static OPEN: Once = Once::new();
    let Ok(msg) = CString::new(line) else { return };
    OPEN.call_once(|| {
        // SAFETY: the ident is a 'static C string (openlog keeps the pointer).
        unsafe { libc::openlog(c"jodd-ai-mcp".as_ptr(), libc::LOG_PID, libc::LOG_USER) }
    });
    // SAFETY: a constant "%s" format with one NUL-terminated argument.
    unsafe { libc::syslog(libc::LOG_INFO, c"%s".as_ptr(), msg.as_ptr()) }
}

/// One `write` of a whole line in append mode, never through a symlink.
fn append(path: &Path, line: &str) {
    let text = format!("{} {line}\n", utc_now());
    let file = OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path);
    if let Ok(mut f) = file {
        let _ = f.write_all(text.as_bytes());
    }
}

/// `YYYY-MM-DDThh:mm:ssZ`, without a date crate.
fn utc_now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let (y, m, d) = civil_from_days(days as i64);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// Howard Hinnant's `civil_from_days`: days since 1970-01-01 to (y, m, d).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_has_only_metadata() {
        let l = format_line(
            Some("3f2a9b1c"),
            "infer",
            Some(HarnessId::Codex),
            Duration::from_millis(1234),
            "ok",
        );
        assert_eq!(
            l,
            "key=3f2a9b1c tool=infer harness=codex dur_ms=1234 outcome=ok"
        );
        let l = format_line(None, "forget_key", None, Duration::ZERO, "not_found");
        assert_eq!(
            l,
            "key=- tool=forget_key harness=- dur_ms=0 outcome=not_found"
        );
    }

    #[test]
    fn malformed_key_and_words_cannot_break_the_line() {
        let l = format_line(Some("bad key\nx"), "in fer\n", None, Duration::ZERO, "a=b");
        assert_eq!(l, "key=invalid tool=in?fer? harness=- dur_ms=0 outcome=a?b");
        assert!(!l.contains('\n'));
    }

    #[test]
    fn civil_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
        // 2026-10-30
        assert_eq!(civil_from_days(20_756), (2026, 10, 30));
    }

    #[test]
    fn append_writes_whole_lines_and_refuses_symlinks() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("calls.log");
        append(&p, "one");
        append(&p, "two");
        let text = std::fs::read_to_string(&p).unwrap();
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].ends_with("Z one"), "{text}");
        assert!(lines[1].ends_with("Z two"), "{text}");

        let target = d.path().join("elsewhere");
        let link = d.path().join("link.log");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        append(&link, "x");
        assert!(!target.exists(), "followed a planted symlink");
    }
}
