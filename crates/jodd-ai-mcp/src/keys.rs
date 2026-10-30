//! `authorized_keys` management for the AI account: `forget` (a client
//! revoking itself), `scan` (what `doctor` and `status` report) and `adopt`
//! (re-pointing legacy lines at `jodd-ai-mcp`, spec §10).
//!
//! Every function edits the CURRENT user's file, found through `$HOME` (ruling
//! R3: sshd and `su -` both set it; it must be absolute). Root never runs
//! these on the account's behalf: the helper runs them as the account.
//!
//! The rules, each a refusal with no change:
//! - `~/.ssh` and `~/.ssh/authorized_keys` must not be symlinks (checked with
//!   `symlink_metadata`, and the file is opened `O_NOFOLLOW`), must be a
//!   directory and a regular file, and must be owned by the effective uid.
//! - A write goes to a same-directory temp file (0600), is fsynced and renamed
//!   over the file. Every line not being changed keeps its bytes (CRLF, blank
//!   and comment lines, a missing final newline) and its place.
//! - Nothing changed means nothing written.
//!
//! A line is a `jodd-ai-<id>` line when it ends with ` jodd-ai-<id>` (wrapper
//! v2's `grep -v " jodd-ai-$key\$"`); a trailing CR is ignored, so a revocation
//! also reaches a line saved with Windows line endings.

use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// One `jodd-ai-<id>` line of the keys file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyLine {
    pub id: String,
    /// The forced command runs `…/jodd-ai-mcp`.
    pub points_at_mcp: bool,
    /// The `command="…"` value, unescaped; `None` when the line has none.
    pub command: Option<String>,
}

impl KeyLine {
    /// `mcp`, `legacy` (a wrapper `adopt` re-points) or `other`.
    pub fn kind(&self) -> &'static str {
        if self.points_at_mcp {
            "mcp"
        } else if self.command.as_deref().is_some_and(is_legacy_command) {
            "legacy"
        } else {
            "other"
        }
    }
}

/// Remove every `jodd-ai-<key_id>` line; `Ok(true)` when one was removed.
pub fn forget(key_id: &str) -> io::Result<bool> {
    forget_in(&home()?, euid(), key_id)
}

/// Every `jodd-ai-<id>` key line, in file order. Comment lines are skipped.
pub fn scan() -> io::Result<Vec<KeyLine>> {
    scan_in(&home()?, euid())
}

/// Re-point every legacy line (`command="…/.jodd/ai-run-v<n> …"`, comment
/// exactly `jodd-ai-<id>`) at `<mcp_path> --key <id>`; returns how many.
pub fn adopt(mcp_path: &str) -> io::Result<usize> {
    adopt_in(&home()?, euid(), mcp_path)
}

fn euid() -> u32 {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

fn home() -> io::Result<PathBuf> {
    home_from(std::env::var_os("HOME"))
}

fn home_from(v: Option<std::ffi::OsString>) -> io::Result<PathBuf> {
    v.map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "HOME is unset or not an absolute path; nothing was changed",
            )
        })
}

fn refuse(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, msg)
}

/// The keys file, checked and read. `None` when it (or `~/.ssh`) is absent.
struct KeysFile {
    dir: PathBuf,
    path: PathBuf,
    bytes: Vec<u8>,
}

fn read_keys(home: &Path, uid: u32) -> io::Result<Option<KeysFile>> {
    let dir = home.join(".ssh");
    let m = match fs::symlink_metadata(&dir) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        r => r?,
    };
    if m.file_type().is_symlink() {
        return Err(refuse(format!(
            "{} is a symlink; nothing was changed. Edit it by hand (ls -l {} shows where it points)",
            dir.display(),
            dir.display()
        )));
    }
    if !m.is_dir() {
        return Err(refuse(format!(
            "{} is not a directory; nothing was changed",
            dir.display()
        )));
    }
    if m.uid() != uid {
        return Err(refuse(format!(
            "{} is owned by uid {}, not by this account (uid {uid}); nothing was changed",
            dir.display(),
            m.uid()
        )));
    }

    let path = dir.join("authorized_keys");
    let m = match fs::symlink_metadata(&path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        r => r?,
    };
    if m.file_type().is_symlink() {
        return Err(refuse(format!(
            "{} is a symlink; nothing was changed. Edit its target by hand (ls -l {} shows where it points)",
            path.display(),
            path.display()
        )));
    }
    // O_NOFOLLOW: a link swapped in after the check is refused (ELOOP), and
    // O_NONBLOCK: a FIFO swapped in cannot hang the open.
    let mut f = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&path)
        .map_err(|e| io::Error::new(e.kind(), format!("cannot open {}: {e}", path.display())))?;
    // Judge what was opened, not what was looked at.
    let m = f.metadata()?;
    if !m.is_file() {
        return Err(refuse(format!(
            "{} is not a regular file; nothing was changed",
            path.display()
        )));
    }
    if m.uid() != uid {
        return Err(refuse(format!(
            "{} is owned by uid {}, not by this account (uid {uid}); nothing was changed",
            path.display(),
            m.uid()
        )));
    }
    let mut bytes = Vec::with_capacity(m.len() as usize);
    f.read_to_end(&mut bytes)?;
    Ok(Some(KeysFile { dir, path, bytes }))
}

impl KeysFile {
    /// Same-directory temp file (0600), fsync, rename over the keys file.
    fn replace(&self, bytes: &[u8]) -> io::Result<()> {
        let mut tmp = tempfile::Builder::new()
            .prefix(".jodd-ai-keys.")
            .tempfile_in(&self.dir)?;
        tmp.as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))?;
        tmp.write_all(bytes)?;
        tmp.as_file().sync_all()?;
        tmp.persist(&self.path).map_err(|e| e.error)?;
        // Best effort: make the rename itself durable.
        if let Ok(d) = fs::File::open(&self.dir) {
            let _ = d.sync_all();
        }
        Ok(())
    }
}

/// Each line with its own terminator (`\n`; the last may have none).
fn lines(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    bytes.split_inclusive(|&b| b == b'\n')
}

/// A line's text and its terminator (`""`, `"\n"` or `"\r\n"`).
fn split_end(line: &[u8]) -> (&[u8], &[u8]) {
    let body = line.strip_suffix(b"\n").unwrap_or(line);
    let body = body.strip_suffix(b"\r").unwrap_or(body);
    line.split_at(body.len())
}

fn is_key_id(id: &[u8]) -> bool {
    id.len() == 8
        && id
            .iter()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
}

/// The id of a line ending in ` jodd-ai-<8 lowercase hex>`.
fn trailing_id(text: &[u8]) -> Option<&str> {
    const MARK: &[u8] = b" jodd-ai-";
    let id = text.get(text.len().checked_sub(8)?..)?;
    let before = &text[..text.len() - 8];
    (is_key_id(id) && before.ends_with(MARK)).then(|| std::str::from_utf8(id).unwrap_or_default())
}

fn forget_in(home: &Path, uid: u32, key_id: &str) -> io::Result<bool> {
    if !is_key_id(key_id.as_bytes()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a key id is 8 lowercase hex digits",
        ));
    }
    let Some(file) = read_keys(home, uid)? else {
        return Ok(false);
    };
    let mut kept = Vec::with_capacity(file.bytes.len());
    let mut removed = false;
    for line in lines(&file.bytes) {
        if trailing_id(split_end(line).0) == Some(key_id) {
            removed = true;
        } else {
            kept.extend_from_slice(line);
        }
    }
    if removed {
        file.replace(&kept)?;
    }
    Ok(removed)
}

fn scan_in(home: &Path, uid: u32) -> io::Result<Vec<KeyLine>> {
    let Some(file) = read_keys(home, uid)? else {
        return Ok(Vec::new());
    };
    Ok(lines(&file.bytes)
        .filter_map(|line| {
            let text = split_end(line).0;
            let id = trailing_id(text)?;
            let text = std::str::from_utf8(text).ok()?;
            let parsed = parse(text)?;
            let command = parsed.command();
            Some(KeyLine {
                id: id.to_string(),
                points_at_mcp: command.as_deref().is_some_and(is_mcp_command),
                command,
            })
        })
        .collect())
}

fn adopt_in(home: &Path, uid: u32, mcp_path: &str) -> io::Result<usize> {
    if !mcp_path.starts_with('/')
        || mcp_path
            .chars()
            .any(|c| c == '"' || c == '\\' || c.is_whitespace() || c.is_control())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{mcp_path:?} is not an absolute path that can sit in a forced command"),
        ));
    }
    let Some(file) = read_keys(home, uid)? else {
        return Ok(0);
    };
    let mut out = Vec::with_capacity(file.bytes.len() + 64);
    let mut count = 0;
    for line in lines(&file.bytes) {
        let (text, end) = split_end(line);
        match adopted(text, mcp_path) {
            Some(new) => {
                out.extend_from_slice(new.as_bytes());
                out.extend_from_slice(end);
                count += 1;
            }
            None => out.extend_from_slice(line),
        }
    }
    if count > 0 {
        file.replace(&out)?;
    }
    Ok(count)
}

/// The re-pointed line for a legacy jodd line, else `None`. Options other
/// than `restrict` and `command` (an administrator's `from="…"`) are kept.
fn adopted(text: &[u8], mcp_path: &str) -> Option<String> {
    let id = trailing_id(text)?;
    let text = std::str::from_utf8(text).ok()?;
    let p = parse(text)?;
    if p.comment != format!("jodd-ai-{id}") || !is_legacy_command(&p.command()?) {
        return None;
    }
    let mut opts = format!("restrict,command=\"{mcp_path} --key {id}\"");
    for o in p.options() {
        if !(o.eq_ignore_ascii_case("restrict") || is_command_option(o)) {
            opts.push(',');
            opts.push_str(o);
        }
    }
    Some(format!("{opts} {} {} jodd-ai-{id}", p.key_type, p.key_body))
}

/// `…/.jodd/ai-run-v<digits>` as the command's program.
fn is_legacy_command(command: &str) -> bool {
    const MARK: &str = "/.jodd/ai-run-v";
    let program = command.split_whitespace().next().unwrap_or("");
    program.rfind(MARK).is_some_and(|i| {
        let v = &program[i + MARK.len()..];
        !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit())
    })
}

/// `…/jodd-ai-mcp` as the command's program.
fn is_mcp_command(command: &str) -> bool {
    let program = command.split_whitespace().next().unwrap_or("");
    program.starts_with('/') && Path::new(program).file_name() == Some("jodd-ai-mcp".as_ref())
}

fn is_command_option(o: &str) -> bool {
    o.get(..9)
        .is_some_and(|p| p.eq_ignore_ascii_case("command=\""))
}

/// A key line in sshd's shape: `[options] <type> <body> [comment]`.
struct Parsed<'a> {
    options: &'a str,
    key_type: &'a str,
    key_body: &'a str,
    comment: &'a str,
}

/// Key types sshd reads; a first word that is none of these is options.
fn is_key_type(w: &str) -> bool {
    w.starts_with("ssh-") || w.starts_with("ecdsa-") || w.starts_with("sk-")
}

fn parse(text: &str) -> Option<Parsed<'_>> {
    let t = text.trim_start_matches([' ', '\t']);
    if t.is_empty() || t.starts_with('#') {
        return None;
    }
    let (options, rest) = if is_key_type(first_word(t)) {
        ("", t)
    } else {
        let end = options_end(t)?;
        (&t[..end], &t[end..])
    };
    let rest = rest.trim_start_matches([' ', '\t']);
    let key_type = first_word(rest);
    if !is_key_type(key_type) {
        return None;
    }
    let rest = rest[key_type.len()..].trim_start_matches([' ', '\t']);
    let key_body = first_word(rest);
    if key_body.is_empty() {
        return None;
    }
    let comment = rest[key_body.len()..].trim_matches([' ', '\t']);
    Some(Parsed {
        options,
        key_type,
        key_body,
        comment,
    })
}

fn first_word(t: &str) -> &str {
    t.split([' ', '\t']).next().unwrap_or("")
}

/// Where the options field ends: the first blank outside double quotes
/// (`\"` inside quotes is a literal quote). `None` for an unterminated quote.
fn options_end(t: &str) -> Option<usize> {
    let mut quoted = false;
    let mut escaped = false;
    for (i, c) in t.char_indices() {
        if escaped {
            escaped = false;
        } else if quoted && c == '\\' {
            escaped = true;
        } else if c == '"' {
            quoted = !quoted;
        } else if !quoted && (c == ' ' || c == '\t') {
            return Some(i);
        }
    }
    (!quoted).then_some(t.len())
}

impl<'a> Parsed<'a> {
    /// The options, split on commas outside quotes.
    fn options(&self) -> Vec<&'a str> {
        if self.options.is_empty() {
            return Vec::new();
        }
        let mut out = Vec::new();
        let (mut start, mut quoted, mut escaped) = (0, false, false);
        for (i, c) in self.options.char_indices() {
            if escaped {
                escaped = false;
            } else if quoted && c == '\\' {
                escaped = true;
            } else if c == '"' {
                quoted = !quoted;
            } else if !quoted && c == ',' {
                out.push(&self.options[start..i]);
                start = i + 1;
            }
        }
        out.push(&self.options[start..]);
        out
    }

    /// The `command="…"` value with `\"` unescaped, as sshd reads it.
    fn command(&self) -> Option<String> {
        let o = self.options().into_iter().find(|o| is_command_option(o))?;
        let inner = o[9..].strip_suffix('"')?;
        Some(inner.replace("\\\"", "\""))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    /// A legacy line exactly as Jodd's `install.rs` (commit d514d03d) writes
    /// it: `restrict,command="$HOME/.jodd/ai-run-v2 --key <id>" <type> <body>
    /// jodd-ai-<id>`, with `$HOME` expanded.
    fn legacy(home: &Path, id: &str) -> String {
        format!(
            "restrict,command=\"{}/.jodd/ai-run-v2 --key {id}\" ssh-ed25519 \
             AAAAC3NzaC1lZDI1NTE5AAAAIDummyDummyDummyDummyDummyDummyDummyDu jodd-ai-{id}",
            home.display()
        )
    }

    const MCP: &str = "/usr/local/bin/jodd-ai-mcp";
    const OTHER: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOtherOtherOther me@laptop";

    fn mcp_line(id: &str) -> String {
        format!(
            "restrict,command=\"{MCP} --key {id}\" ssh-ed25519 \
             AAAAC3NzaC1lZDI1NTE5AAAAIPhonePhonePhonePhone jodd-ai-{id}"
        )
    }

    fn uid() -> u32 {
        // SAFETY: geteuid has no preconditions and cannot fail.
        unsafe { libc::geteuid() }
    }

    struct Home {
        dir: tempfile::TempDir,
    }

    impl Home {
        /// A home with `~/.ssh` (0700) and no keys file.
        fn new() -> Home {
            let dir = tempfile::tempdir().unwrap();
            let ssh = dir.path().join(".ssh");
            fs::create_dir(&ssh).unwrap();
            fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();
            Home { dir }
        }
        fn path(&self) -> &Path {
            self.dir.path()
        }
        fn keys(&self) -> PathBuf {
            self.path().join(".ssh/authorized_keys")
        }
        fn write(&self, bytes: &[u8]) {
            fs::write(self.keys(), bytes).unwrap();
            fs::set_permissions(self.keys(), fs::Permissions::from_mode(0o600)).unwrap();
        }
        fn read(&self) -> Vec<u8> {
            fs::read(self.keys()).unwrap()
        }
        fn entries(&self) -> usize {
            fs::read_dir(self.path().join(".ssh")).unwrap().count()
        }
    }

    fn s(b: &[u8]) -> String {
        String::from_utf8_lossy(b).into_owned()
    }

    #[test]
    fn forget_removes_only_matching_line() {
        let h = Home::new();
        let a = legacy(h.path(), "11111111");
        let b = mcp_line("3f2a9b1c");
        // CRLF, a comment, a blank line and no final newline all survive.
        let before = format!(
            "# my keys\r\n{OTHER}\r\n\n{a}\n{b}\n{OTHER} jodd-ai-3f2a9b1c-not\nlast-line-no-newline"
        );
        h.write(before.as_bytes());
        assert!(forget_in(h.path(), uid(), "3f2a9b1c").unwrap());
        let want = format!(
            "# my keys\r\n{OTHER}\r\n\n{a}\n{OTHER} jodd-ai-3f2a9b1c-not\nlast-line-no-newline"
        );
        assert_eq!(s(&h.read()), want);
        assert_eq!(
            fs::metadata(h.keys()).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(h.entries(), 1, "no temp file left beside the keys file");

        // Nothing left to remove: false, and the file is untouched.
        let ino = fs::metadata(h.keys()).unwrap().ino();
        assert!(!forget_in(h.path(), uid(), "3f2a9b1c").unwrap());
        assert_eq!(s(&h.read()), want);
        assert_eq!(fs::metadata(h.keys()).unwrap().ino(), ino);

        // Every line for the id goes (wrapper v2's `grep -v`), CRLF included.
        let c = legacy(h.path(), "22222222");
        h.write(format!("{c}\r\n{OTHER}\n{c}").as_bytes());
        assert!(forget_in(h.path(), uid(), "22222222").unwrap());
        assert_eq!(s(&h.read()), format!("{OTHER}\n"));
    }

    #[test]
    fn forget_on_missing_file_is_ok_false() {
        let h = Home::new();
        assert!(!forget_in(h.path(), uid(), "3f2a9b1c").unwrap());
        assert!(!h.keys().exists(), "forget must not create the file");
        // No ~/.ssh at all either.
        let bare = tempfile::tempdir().unwrap();
        assert!(!forget_in(bare.path(), uid(), "3f2a9b1c").unwrap());
        assert!(!bare.path().join(".ssh").exists());
    }

    #[test]
    fn forget_refuses_a_malformed_id() {
        let h = Home::new();
        h.write(format!("{OTHER} jodd-ai-\n").as_bytes());
        for id in ["", "ABCD1234", "abcd123", "abcd12345", "abcd123g"] {
            assert!(forget_in(h.path(), uid(), id).is_err(), "{id:?}");
        }
        assert_eq!(s(&h.read()), format!("{OTHER} jodd-ai-\n"));
    }

    #[test]
    fn adopt_rewrites_only_legacy_jodd_lines() {
        let h = Home::new();
        let old = legacy(h.path(), "11111111");
        let mcp = mcp_line("3f2a9b1c");
        let before = format!("{OTHER}\n{old}\n{mcp}\r\n# {old}\n");
        h.write(before.as_bytes());
        assert_eq!(adopt_in(h.path(), uid(), MCP).unwrap(), 1);
        let want_new = format!(
            "restrict,command=\"{MCP} --key 11111111\" ssh-ed25519 \
             AAAAC3NzaC1lZDI1NTE5AAAAIDummyDummyDummyDummyDummyDummyDummyDu jodd-ai-11111111"
        );
        assert_eq!(
            s(&h.read()),
            format!("{OTHER}\n{want_new}\n{mcp}\r\n# {old}\n"),
            "only the legacy line changes; a commented-out one is not a key"
        );
        assert_eq!(
            fs::metadata(h.keys()).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(h.entries(), 1);
    }

    #[test]
    fn adopt_leaves_lines_that_are_not_jodds_shape() {
        let h = Home::new();
        let p = h.path().display().to_string();
        let lines = [
            // a legacy command but another comment
            format!("restrict,command=\"{p}/.jodd/ai-run-v2 --key 11111111\" ssh-ed25519 AAAAx me@laptop"),
            // the comment carries more than the jodd word
            format!("restrict,command=\"{p}/.jodd/ai-run-v2 --key 11111111\" ssh-ed25519 AAAAx extra jodd-ai-11111111"),
            // not a versioned wrapper
            format!("restrict,command=\"{p}/.jodd/ai-run-vx --key 11111111\" ssh-ed25519 AAAAx jodd-ai-11111111"),
            format!("restrict,command=\"{p}/.jodd/ai-run-v --key 11111111\" ssh-ed25519 AAAAx jodd-ai-11111111"),
            format!("restrict,command=\"{p}/.jodd/ai-run-v2x --key 11111111\" ssh-ed25519 AAAAx jodd-ai-11111111"),
            // no command at all
            "ssh-ed25519 AAAAx jodd-ai-22222222".to_string(),
            // an id that is not 8 lowercase hex
            format!("restrict,command=\"{p}/.jodd/ai-run-v2 --key 1111111A\" ssh-ed25519 AAAAx jodd-ai-1111111A"),
            // an unterminated quote is not a key line sshd would read
            format!("restrict,command=\"{p}/.jodd/ai-run-v2 --key 11111111 ssh-ed25519 AAAAx jodd-ai-11111111"),
        ];
        let before = lines.join("\n");
        h.write(before.as_bytes());
        assert_eq!(adopt_in(h.path(), uid(), MCP).unwrap(), 0);
        assert_eq!(s(&h.read()), before);
    }

    #[test]
    fn adopt_keeps_extra_options_and_the_line_ending() {
        let h = Home::new();
        let p = h.path().display().to_string();
        h.write(
            format!(
                "from=\"10.0.0.0/8,192.168.1.1\",command=\"{p}/.jodd/ai-run-v12 --key abcdef01\",no-pty  \
                 ssh-rsa AAAAB3rsa   jodd-ai-abcdef01\r\nlast"
            )
            .as_bytes(),
        );
        assert_eq!(adopt_in(h.path(), uid(), MCP).unwrap(), 1);
        assert_eq!(
            s(&h.read()),
            format!(
                "restrict,command=\"{MCP} --key abcdef01\",from=\"10.0.0.0/8,192.168.1.1\",no-pty \
                 ssh-rsa AAAAB3rsa jodd-ai-abcdef01\r\nlast"
            )
        );
    }

    #[test]
    fn adopt_is_idempotent() {
        let h = Home::new();
        let a = legacy(h.path(), "11111111");
        let b = legacy(h.path(), "22222222");
        h.write(format!("{a}\n{OTHER}\n{b}").as_bytes());
        assert_eq!(adopt_in(h.path(), uid(), MCP).unwrap(), 2);
        let once = h.read();
        let ino = fs::metadata(h.keys()).unwrap().ino();
        assert_eq!(adopt_in(h.path(), uid(), MCP).unwrap(), 0);
        assert_eq!(h.read(), once);
        // Nothing to adopt: the file is not even rewritten.
        assert_eq!(fs::metadata(h.keys()).unwrap().ino(), ino);
        assert!(
            s(&once).ends_with("jodd-ai-22222222"),
            "no newline was added"
        );
    }

    #[test]
    fn adopt_on_missing_file_is_zero() {
        let h = Home::new();
        assert_eq!(adopt_in(h.path(), uid(), MCP).unwrap(), 0);
        assert!(!h.keys().exists());
    }

    #[test]
    fn adopt_refuses_a_bad_mcp_path() {
        let h = Home::new();
        let before = legacy(h.path(), "11111111");
        h.write(before.as_bytes());
        for p in [
            "jodd-ai-mcp",
            "/usr/bin/x\"y",
            "/usr/bin/a b",
            "/a\\b",
            "/a\nb",
            "",
        ] {
            assert!(adopt_in(h.path(), uid(), p).is_err(), "{p:?}");
        }
        assert_eq!(s(&h.read()), before);
    }

    #[test]
    fn adopt_keys_refuses_symlinked_file() {
        // Review Focus 3: `~/.ssh/authorized_keys -> /etc/shadow`.
        let h = Home::new();
        let elsewhere = tempfile::tempdir().unwrap();
        let target = elsewhere.path().join("shadow");
        let before = legacy(h.path(), "11111111");
        fs::write(&target, &before).unwrap();
        std::os::unix::fs::symlink(&target, h.keys()).unwrap();

        let e = adopt_in(h.path(), uid(), MCP).unwrap_err();
        assert!(e.to_string().contains("symlink"), "{e}");
        assert!(forget_in(h.path(), uid(), "11111111").is_err());
        assert!(scan_in(h.path(), uid()).is_err());
        assert_eq!(fs::read_to_string(&target).unwrap(), before);
        assert!(fs::symlink_metadata(h.keys())
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(h.entries(), 1, "no temp file left");
    }

    #[test]
    fn a_symlinked_ssh_dir_is_refused() {
        let home = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let before = legacy(home.path(), "11111111");
        fs::write(elsewhere.path().join("authorized_keys"), &before).unwrap();
        std::os::unix::fs::symlink(elsewhere.path(), home.path().join(".ssh")).unwrap();

        let e = adopt_in(home.path(), uid(), MCP).unwrap_err();
        assert!(e.to_string().contains("symlink"), "{e}");
        assert!(forget_in(home.path(), uid(), "11111111").is_err());
        assert!(scan_in(home.path(), uid()).is_err());
        assert_eq!(
            fs::read_to_string(elsewhere.path().join("authorized_keys")).unwrap(),
            before
        );
        assert_eq!(fs::read_dir(elsewhere.path()).unwrap().count(), 1);
    }

    #[test]
    fn a_keys_file_owned_by_someone_else_is_refused() {
        // The edit runs as the account that owns the file; anything else
        // (root running it, a file root planted) is refused unchanged.
        let h = Home::new();
        let before = legacy(h.path(), "11111111");
        h.write(before.as_bytes());
        let other = uid().wrapping_add(1);
        assert!(adopt_in(h.path(), other, MCP).is_err());
        assert!(forget_in(h.path(), other, "11111111").is_err());
        assert!(scan_in(h.path(), other).is_err());
        assert_eq!(s(&h.read()), before);
    }

    #[test]
    fn a_keys_file_that_is_not_a_regular_file_is_refused() {
        let h = Home::new();
        fs::create_dir(h.keys()).unwrap();
        assert!(adopt_in(h.path(), uid(), MCP).is_err());
        assert!(forget_in(h.path(), uid(), "11111111").is_err());
        assert!(scan_in(h.path(), uid()).is_err());
    }

    #[test]
    fn scan_flags_lines_not_pointing_at_mcp() {
        let h = Home::new();
        let old = legacy(h.path(), "11111111");
        let mcp = mcp_line("3f2a9b1c");
        let hand = "command=\"/usr/bin/true\" ssh-ed25519 AAAAx jodd-ai-abcdef01";
        let bare = "ssh-ed25519 AAAAx jodd-ai-01234567";
        h.write(
            format!("{OTHER}\n{old}\n{mcp}\r\n{hand}\n{bare}\n# {old}\n{OTHER} jodd-ai-xyz\n")
                .as_bytes(),
        );
        let found = scan_in(h.path(), uid()).unwrap();
        let p = h.path().display();
        assert_eq!(
            found,
            vec![
                KeyLine {
                    id: "11111111".into(),
                    points_at_mcp: false,
                    command: Some(format!("{p}/.jodd/ai-run-v2 --key 11111111")),
                },
                KeyLine {
                    id: "3f2a9b1c".into(),
                    points_at_mcp: true,
                    command: Some(format!("{MCP} --key 3f2a9b1c")),
                },
                KeyLine {
                    id: "abcdef01".into(),
                    points_at_mcp: false,
                    command: Some("/usr/bin/true".into()),
                },
                KeyLine {
                    id: "01234567".into(),
                    points_at_mcp: false,
                    command: None,
                },
            ]
        );
        assert_eq!(
            found.iter().map(KeyLine::kind).collect::<Vec<_>>(),
            ["legacy", "mcp", "other", "other"]
        );
        // A scan never writes.
        assert_eq!(h.entries(), 1);
    }

    #[test]
    fn scan_on_missing_file_is_empty() {
        let h = Home::new();
        assert!(scan_in(h.path(), uid()).unwrap().is_empty());
    }

    #[test]
    fn home_must_be_absolute() {
        assert!(home_from(None).is_err());
        assert!(home_from(Some("relative/home".into())).is_err());
        assert!(home_from(Some("".into())).is_err());
        assert_eq!(
            home_from(Some("/home/jodd-ai".into())).unwrap(),
            PathBuf::from("/home/jodd-ai")
        );
    }
}
