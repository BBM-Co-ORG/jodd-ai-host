//! `jodd-ai-mcp admin …`: what the `jodd-ai-host` helper calls (spec §5.3).
//! The policy file has one owner, this binary; the helper never writes it.
//!
//! Exit 0 on success, 2 on a usage error, 1 otherwise. Every message names
//! the next command to run (a helper command: the admin runs the helper).
//!
//! - `list` (any user): one line per harness, `<name>  enabled|disabled|undecided  <path or ->`.
//! - `enable`, `disable`, `auto-enable` (root): edit the policy.
//! - `adopt-keys`, `scan-keys` (the AI account, never root): edit / read the
//!   account's own `authorized_keys` (root must never write inside its home).
//!
//! `main` refuses `admin` under a forced command, so the restricted key never
//! reaches any of this.

use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::Path;

use crate::keys;
use crate::policy::{self, HarnessId, HarnessPolicy, LoadOutcome, Policy};

const OK: i32 = 0;
const FAILED: i32 = 1;
const USAGE: i32 = 2;

/// Where the installers put this binary; `adopt-keys` points lines at it.
const SELF_PATH: &str = "/usr/local/bin/jodd-ai-mcp";
const DOCTOR: &str = "jodd-ai-host doctor";

const USAGE_TEXT: &str = "\
usage: jodd-ai-mcp admin <command>
  list
  enable <claude|codex> --path <absolute path> --path-env <PATH>        (root)
  auto-enable <claude|codex> --path <absolute path> --path-env <PATH>   (root)
  disable <claude|codex>                                                (root)
  adopt-keys | scan-keys                                       (as the AI account)
The jodd-ai-host helper runs these for you: sudo jodd-ai-host enable claude,
sudo jodd-ai-host adopt-keys, jodd-ai-host doctor.
";

/// The effective uid; in debug builds only, `JODD_AI_MCP_FAKE_UID` stands in
/// (ruling R12), so the root-only paths are testable without root.
fn uid() -> u32 {
    if let Some(v) = crate::test_override("JODD_AI_MCP_FAKE_UID").and_then(|v| v.parse().ok()) {
        return v;
    }
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

fn usage() -> i32 {
    eprint!("{USAGE_TEXT}");
    USAGE
}

pub fn run(args: &[&str], policy_path: &Path) -> i32 {
    match args {
        ["list"] => list(policy_path),
        [cmd @ ("enable" | "auto-enable"), name, flags @ ..] => {
            let (Some(h), Some((path, path_env))) = (HarnessId::from_name(name), flags_of(flags))
            else {
                return usage();
            };
            if !is_root(cmd, name) {
                return FAILED;
            }
            enable(policy_path, h, path, path_env, *cmd == "auto-enable")
        }
        ["disable", name] => {
            let Some(h) = HarnessId::from_name(name) else {
                return usage();
            };
            if !is_root("disable", name) {
                return FAILED;
            }
            disable(policy_path, h)
        }
        [cmd @ ("adopt-keys" | "scan-keys")] => {
            if uid() == 0 {
                // Root never touches the account's home; the helper runs these
                // as the account. scan-keys is read-only: `status` shows it.
                let next = if *cmd == "adopt-keys" {
                    "sudo jodd-ai-host adopt-keys"
                } else {
                    "jodd-ai-host status"
                };
                eprintln!(
                    "admin {cmd} works on the AI account's own ~/.ssh and must not run as root. \
                     Run it through the helper, which runs it as the account: {next}"
                );
                return FAILED;
            }
            if *cmd == "adopt-keys" {
                adopt_keys()
            } else {
                scan_keys()
            }
        }
        _ => usage(),
    }
}

/// `--path <p> --path-env <e>`, each exactly once, in either order.
fn flags_of<'a>(flags: &[&'a str]) -> Option<(&'a str, &'a str)> {
    let (mut path, mut path_env) = (None, None);
    let mut it = flags.iter();
    while let Some(flag) = it.next() {
        let slot = match *flag {
            "--path" => &mut path,
            "--path-env" => &mut path_env,
            _ => return None,
        };
        if slot.is_some() {
            return None;
        }
        *slot = Some(*it.next()?);
    }
    Some((path?, path_env?))
}

fn is_root(cmd: &str, name: &str) -> bool {
    if uid() == 0 {
        return true;
    }
    eprintln!("admin {cmd} edits the host policy and needs root: sudo jodd-ai-host {cmd} {name}");
    false
}

fn list(policy_path: &Path) -> i32 {
    let outcome = policy::load(policy_path);
    let policy = Policy::effective(&outcome);
    for h in HarnessId::all() {
        let (state, path) = match policy.harness.get(&h) {
            None => ("undecided", "-"),
            Some(hp) => (
                if hp.enabled { "enabled" } else { "disabled" },
                if hp.path.is_empty() {
                    "-"
                } else {
                    hp.path.as_str()
                },
            ),
        };
        println!("{}  {state}  {path}", h.as_str());
    }
    match outcome {
        LoadOutcome::Loaded(_) => OK,
        // Not decided yet is a state, not a fault.
        LoadOutcome::Missing => {
            eprintln!(
                "No policy at {}, so nothing is enabled. Turn a harness on with \
                 sudo jodd-ai-host enable <name>, or check the host with {DOCTOR}",
                policy_path.display()
            );
            OK
        }
        LoadOutcome::Invalid(why) => {
            eprintln!(
                "The policy at {} is invalid ({why}), so nothing is enabled. Run {DOCTOR}",
                policy_path.display()
            );
            FAILED
        }
    }
}

/// The policy to edit: a missing file starts empty; an invalid one is never
/// overwritten (it holds the administrator's decisions, typo and all).
fn load_for_edit(policy_path: &Path) -> Option<Policy> {
    match policy::load(policy_path) {
        LoadOutcome::Loaded(p) => Some(p),
        LoadOutcome::Missing => Some(Policy::empty()),
        LoadOutcome::Invalid(why) => {
            eprintln!(
                "The policy at {} is invalid ({why}); nothing was changed. Fix it by hand, \
                 then run {DOCTOR}",
                policy_path.display()
            );
            None
        }
    }
}

/// Write the policy, creating its directory (0755) if missing.
fn save(policy: &Policy, policy_path: &Path) -> i32 {
    let result = (|| {
        if let Some(dir) = policy_path.parent().filter(|d| !d.as_os_str().is_empty()) {
            if !dir.exists() {
                std::fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o755)
                    .create(dir)?;
                // The umask may have narrowed it; the server reads the policy as the account.
                std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755))?;
            }
        }
        policy.save_atomic(policy_path)
    })();
    match result {
        Ok(()) => OK,
        Err(e) => {
            eprintln!(
                "Could not write the policy at {}: {e}. Run {DOCTOR}",
                policy_path.display()
            );
            FAILED
        }
    }
}

/// Absolute, exists, a regular file (after symlinks), executable.
fn check_cli(name: &str, path: &str) -> Result<(), String> {
    let again = format!("sudo jodd-ai-host enable {name} (it finds the CLI for you), or {DOCTOR}");
    if !Path::new(path).is_absolute() {
        return Err(format!(
            "--path must be an absolute path, not {path:?}. Run {again}"
        ));
    }
    let m = std::fs::metadata(path).map_err(|e| {
        format!(
            "{path} cannot be used ({e}). Install the CLI with sudo jodd-ai-host update-clis, \
             then run {again}"
        )
    })?;
    if !m.is_file() {
        return Err(format!("{path} is not a regular file. Run {again}"));
    }
    if m.permissions().mode() & 0o111 == 0 {
        return Err(format!("{path} is not executable. Run {again}"));
    }
    Ok(())
}

fn enable(
    policy_path: &Path,
    h: HarnessId,
    path: &str,
    path_env: &str,
    only_if_absent: bool,
) -> i32 {
    let name = h.as_str();
    let Some(mut policy) = load_for_edit(policy_path) else {
        return FAILED;
    };
    // auto-enable (spec §5.3): an entry, enabled OR disabled, is a decision.
    if only_if_absent {
        if let Some(hp) = policy.harness.get(&h) {
            let (now, other) = if hp.enabled {
                ("enabled", "disable")
            } else {
                ("disabled", "enable")
            };
            println!(
                "{name} is already {now} in the policy; no change. To change it: \
                 sudo jodd-ai-host {other} {name}"
            );
            return OK;
        }
    }
    if let Err(why) = check_cli(name, path) {
        eprintln!("{why}");
        return FAILED;
    }
    if path_env.is_empty() {
        eprintln!("--path-env must not be empty. Run sudo jodd-ai-host enable {name}, or {DOCTOR}");
        return FAILED;
    }
    let timeout_secs = policy
        .harness
        .get(&h)
        .map(|hp| hp.timeout_secs)
        .unwrap_or_else(|| h.default_timeout());
    policy.harness.insert(
        h,
        HarnessPolicy {
            enabled: true,
            path: path.to_string(),
            path_env: path_env.to_string(),
            timeout_secs,
        },
    );
    let code = save(&policy, policy_path);
    if code == OK {
        println!("{name} is enabled ({path}). Check it with: jodd-ai-host list");
    }
    code
}

fn disable(policy_path: &Path, h: HarnessId) -> i32 {
    let name = h.as_str();
    let Some(mut policy) = load_for_edit(policy_path) else {
        return FAILED;
    };
    // An absent entry is created, so the decision sticks: a later
    // auto-enable (installer, image update) leaves it alone.
    policy
        .harness
        .entry(h)
        .and_modify(|hp| hp.enabled = false)
        .or_insert_with(|| HarnessPolicy {
            enabled: false,
            path: String::new(),
            path_env: String::new(),
            timeout_secs: h.default_timeout(),
        });
    let code = save(&policy, policy_path);
    if code == OK {
        println!("{name} is disabled. Turn it back on with: sudo jodd-ai-host enable {name}");
    }
    code
}

/// `/usr/local/bin/jodd-ai-mcp`; in debug builds `JODD_AI_MCP_SELF_PATH` (R12).
fn self_path() -> String {
    crate::test_override("JODD_AI_MCP_SELF_PATH").unwrap_or_else(|| SELF_PATH.to_string())
}

fn adopt_keys() -> i32 {
    match keys::adopt(&self_path()) {
        Ok(n) => {
            println!(
                "Re-pointed {n} legacy key line(s) at jodd-ai-mcp. Check them with: \
                 jodd-ai-host status"
            );
            OK
        }
        Err(e) => {
            eprintln!("Could not adopt the keys: {e}. Run {DOCTOR}");
            FAILED
        }
    }
}

fn scan_keys() -> i32 {
    match keys::scan() {
        Ok(found) => {
            for k in found {
                println!(
                    "{}  {}  {}",
                    k.id,
                    k.kind(),
                    k.command.as_deref().unwrap_or("-")
                );
            }
            OK
        }
        Err(e) => {
            eprintln!("Could not read the keys: {e}. Run {DOCTOR}");
            FAILED
        }
    }
}
