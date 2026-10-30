//! Claude Code and Codex invocation definitions, at byte parity with Jodd's
//! shipped presets (`src-tauri/src/llm/presets.rs`, commit d514d03d).
//!
//! These are DATA: the runner (not this module) spawns the process. The flag
//! rationale lives in `docs/HARNESS-FLAGS.md`; do not reorder or "tidy" the
//! argv without re-reading it.

use crate::policy::HarnessId;

/// One argv element. The two placeholders are rendered by the runner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arg {
    Lit(&'static str),
    /// Path of the file the harness writes its final message to.
    OutFile,
    /// Path of the JSON schema file handed to the harness.
    SchemaFile,
}

/// Where the answer is read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    /// Stdout, optionally unwrapped from a JSON envelope's field.
    Stdout { unwrap_field: Option<&'static str> },
    /// The file named by `Arg::OutFile`.
    LastMessageFile,
}

#[derive(Debug)]
pub struct HarnessDef {
    #[allow(dead_code)] // pins each def to its id in the tests
    pub id: HarnessId,
    pub args: &'static [Arg],
    pub schema_args: &'static [Arg],
    pub answer: Answer,
}

// `--tools <tools...>` is VARIADIC and MUST stay last in `args`: anything
// appended after it (schema_args today) is swallowed as another tool name
// unless it starts with `-`. See docs/HARNESS-FLAGS.md.
static CLAUDE: HarnessDef = HarnessDef {
    id: HarnessId::Claude,
    args: &[
        Arg::Lit("-p"),
        Arg::Lit("--output-format"),
        Arg::Lit("json"),
        Arg::Lit("--strict-mcp-config"),
        Arg::Lit("--setting-sources"),
        Arg::Lit(""),
        Arg::Lit("--tools"),
        Arg::Lit(""),
    ],
    schema_args: &[],
    answer: Answer::Stdout {
        unwrap_field: Some("result"),
    },
};

static CODEX: HarnessDef = HarnessDef {
    id: HarnessId::Codex,
    args: &[
        Arg::Lit("exec"),
        Arg::Lit("--ignore-user-config"),
        Arg::Lit("--ephemeral"),
        Arg::Lit("--sandbox"),
        Arg::Lit("read-only"),
        Arg::Lit("--skip-git-repo-check"),
        Arg::Lit("--output-last-message"),
        Arg::OutFile,
        Arg::Lit("-"),
    ],
    schema_args: &[Arg::Lit("--output-schema"), Arg::SchemaFile],
    answer: Answer::LastMessageFile,
};

pub fn def(id: HarnessId) -> &'static HarnessDef {
    match id {
        HarnessId::Claude => &CLAUDE,
        HarnessId::Codex => &CODEX,
    }
}

/// The text fed to the harness on stdin: Jodd's `{system}\n\n---\n\n{payload}`.
pub fn join_prompt(system: Option<&str>, prompt: &str) -> String {
    match system {
        Some(system) => format!("{system}\n\n---\n\n{prompt}"),
        None => prompt.to_string(),
    }
}

/// Pull the model's answer out of a finished run.
///
/// Mirrors Jodd's `finish_output`: ANSI is stripped first, silence is an
/// error, and a declared JSON envelope is unwrapped with `dig_unwrap` (so
/// non-JSON stdout is an `Err`, never passed through). The returned answer is
/// stripped once more, so escapes that only appear after JSON decoding go too.
pub fn extract_answer(
    def: &HarnessDef,
    stdout: &str,
    out_file: Option<&str>,
) -> Result<String, String> {
    match def.answer {
        Answer::Stdout { unwrap_field } => {
            let text = strip_ansi(stdout);
            if text.trim().is_empty() {
                return Err("CLI produced no output".into());
            }
            let answer = match unwrap_field {
                Some(field) => dig_unwrap(text.trim(), field)?,
                None => text,
            };
            Ok(strip_ansi(&answer))
        }
        Answer::LastMessageFile => match out_file {
            Some(content) if !content.trim().is_empty() => Ok(strip_ansi(content)),
            _ => Err("CLI wrote no final message".into()),
        },
    }
}

/// Remove ANSI escape sequences (CSI and OSC) from CLI output.
///
/// Iterates over `char`s, not bytes: content is Thai-first and a byte-wise
/// scanner would split multi-byte sequences.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.peek().copied() {
            // CSI: ESC [ ... final byte in 0x40..=0x7E
            Some('[') => {
                chars.next();
                for c2 in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&c2) {
                        break;
                    }
                }
            }
            // OSC: ESC ] ... terminated by BEL or by ESC \
            Some(']') => {
                chars.next();
                loop {
                    match chars.peek().copied() {
                        None => break,
                        Some('\u{07}') => {
                            chars.next();
                            break;
                        }
                        // Leave the ESC unconsumed: the outer loop strips it,
                        // whether it starts the ST (ESC \) or a new sequence.
                        Some('\u{1b}') => break,
                        Some(_) => {
                            chars.next();
                        }
                    }
                }
            }
            // Two-character escapes (ESC c, ESC M, ESC =) are always ASCII.
            // A non-ASCII char after ESC is real content: keep it.
            Some(c2) if c2.is_ascii() => {
                chars.next();
            }
            _ => {}
        }
    }
    out
}

/// Unwrap a CLI's JSON envelope: a single object `{"result": "..."}` or an
/// array of events, where the LAST element carrying the field wins. A field
/// that exists but is not a string comes back as its JSON text.
fn dig_unwrap(text: &str, field: &str) -> Result<String, String> {
    let v: serde_json::Value =
        serde_json::from_str(text).map_err(|e| format!("wrapper json: {e}"))?;

    let found = match &v {
        serde_json::Value::Array(items) => items.iter().rev().find_map(|item| item.get(field)),
        other => other.get(field),
    };

    match found {
        None => Err(format!(
            "wrapper has no string field '{field}' ({})",
            match &v {
                serde_json::Value::Array(i) => format!("searched {} array elements", i.len()),
                _ => "top level is an object".to_string(),
            }
        )),
        Some(serde_json::Value::String(s)) => Ok(s.clone()),
        // Re-stringifying fails safe: a JSON string containing JSON is what
        // the caller's lenient envelope parser already knows how to unwrap.
        Some(other) => serde_json::to_string(other)
            .map_err(|e| format!("re-stringifying field '{field}': {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lits(args: &[Arg]) -> Vec<&'static str> {
        args.iter()
            .map(|a| match a {
                Arg::Lit(s) => *s,
                Arg::OutFile => "{OutFile}",
                Arg::SchemaFile => "{SchemaFile}",
            })
            .collect()
    }

    #[test]
    fn claude_argv_matches_jodd_preset() {
        let d = def(HarnessId::Claude);
        assert_eq!(d.id, HarnessId::Claude);
        assert_eq!(
            lits(d.args),
            [
                "-p",
                "--output-format",
                "json",
                "--strict-mcp-config",
                "--setting-sources",
                "",
                "--tools",
                ""
            ]
        );
        // `--tools ""` is variadic and must be last.
        assert_eq!(d.args[d.args.len() - 2], Arg::Lit("--tools"));
        assert_eq!(d.args[d.args.len() - 1], Arg::Lit(""));
        assert!(d.schema_args.is_empty());
        assert_eq!(
            d.answer,
            Answer::Stdout {
                unwrap_field: Some("result")
            }
        );
    }

    #[test]
    fn codex_argv_matches_jodd_preset() {
        let d = def(HarnessId::Codex);
        assert_eq!(d.id, HarnessId::Codex);
        assert_eq!(
            lits(d.args),
            [
                "exec",
                "--ignore-user-config",
                "--ephemeral",
                "--sandbox",
                "read-only",
                "--skip-git-repo-check",
                "--output-last-message",
                "{OutFile}",
                "-"
            ]
        );
        assert_eq!(lits(d.schema_args), ["--output-schema", "{SchemaFile}"]);
        assert_eq!(d.answer, Answer::LastMessageFile);
    }

    #[test]
    fn every_harness_has_a_matching_def() {
        for id in HarnessId::all() {
            assert_eq!(def(id).id, id);
        }
    }

    #[test]
    fn join_with_system_matches_jodd() {
        assert_eq!(join_prompt(Some("S"), "P"), "S\n\n---\n\nP");
        assert_eq!(join_prompt(None, "P"), "P");
    }

    #[test]
    fn claude_answer_unwraps_result() {
        let d = def(HarnessId::Claude);
        // single object
        assert_eq!(
            extract_answer(d, r#"{"type":"result","result":"hello"}"#, None).unwrap(),
            "hello"
        );
        // event array: qwen-style shape, last matching event wins
        let raw = r#"[{"type":"init"},{"type":"result","result":"hello","is_error":false}]"#;
        assert_eq!(extract_answer(d, raw, None).unwrap(), "hello");
        assert_eq!(
            extract_answer(d, r#"[{"result":"first"},{"result":"final"}]"#, None).unwrap(),
            "final"
        );
        // missing field is an error, not a guess
        let err = extract_answer(d, r#"[{"type":"init"}]"#, None).unwrap_err();
        assert!(err.contains("no string field 'result'"), "got: {err}");
        // non-string result comes back as its JSON text
        assert_eq!(
            extract_answer(d, r#"{"type":"result","result":{"a":1,"b":2}}"#, None).unwrap(),
            r#"{"a":1,"b":2}"#
        );
    }

    #[test]
    fn claude_answer_rejects_non_json_and_silence() {
        let d = def(HarnessId::Claude);
        let err = extract_answer(d, "plain text, not json", None).unwrap_err();
        assert!(err.contains("wrapper json"), "got: {err}");
        assert!(extract_answer(d, "  \n", None).is_err());
    }

    #[test]
    fn codex_answer_reads_out_file() {
        let d = def(HarnessId::Codex);
        // stdout is hook chatter and is ignored.
        assert_eq!(
            extract_answer(d, "tokens used: 12", Some("the answer")).unwrap(),
            "the answer"
        );
        assert!(extract_answer(d, "chatter", Some("")).is_err());
        assert!(extract_answer(d, "chatter", Some("  \n\t")).is_err());
        assert!(extract_answer(d, "chatter", None).is_err());
    }

    #[test]
    fn ansi_is_stripped_from_answers() {
        let claude = def(HarnessId::Claude);
        let wrapped = "\u{1b}[1m{\"result\":\"ok\"}\u{1b}[0m";
        assert_eq!(extract_answer(claude, wrapped, None).unwrap(), "ok");
        // escapes that only exist after JSON decoding are removed too
        assert_eq!(
            extract_answer(claude, r#"{"result":"\u001b[32mgreen\u001b[0m"}"#, None).unwrap(),
            "green"
        );
        let codex = def(HarnessId::Codex);
        assert_eq!(
            extract_answer(codex, "", Some("\u{1b}[1mบทเรียน\u{1b}[0m")).unwrap(),
            "บทเรียน"
        );
    }

    #[test]
    fn strips_sgr_colour_codes() {
        assert_eq!(strip_ansi("\u{1b}[32mgreen\u{1b}[0m"), "green");
    }

    #[test]
    fn strips_cursor_movement_and_erase_sequences() {
        assert_eq!(strip_ansi("a\u{1b}[2K\u{1b}[1Gb"), "ab");
    }

    #[test]
    fn strips_osc_title_sequences_terminated_by_bel() {
        assert_eq!(strip_ansi("\u{1b}]0;title\u{07}text"), "text");
    }

    #[test]
    fn strips_osc_sequences_terminated_by_string_terminator() {
        assert_eq!(strip_ansi("\u{1b}]8;;http://x\u{1b}\\link"), "link");
    }

    #[test]
    fn leaves_clean_text_untouched() {
        assert_eq!(
            strip_ansi("{\"lessons_markdown\":\"x\"}"),
            "{\"lessons_markdown\":\"x\"}"
        );
    }

    #[test]
    fn preserves_multibyte_text() {
        assert_eq!(strip_ansi("\u{1b}[1mบทเรียน\u{1b}[0m"), "บทเรียน");
    }

    #[test]
    fn bare_escape_does_not_consume_multibyte_content() {
        assert_eq!(strip_ansi("\u{1b}บทเรียน"), "บทเรียน");
    }

    #[test]
    fn osc_aborted_by_a_new_escape_still_strips_that_sequence() {
        assert_eq!(strip_ansi("\u{1b}]0;title\u{1b}[31mred\u{1b}[0m"), "red");
    }
}
