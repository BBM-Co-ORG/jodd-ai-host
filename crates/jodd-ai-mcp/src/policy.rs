//! Host policy: which harnesses may run, and the hard limits. Fail closed: a missing or
//! unreadable policy enables nothing.

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use serde::{Deserialize, Serialize};

pub const DEFAULT_POLICY_PATH: &str = "/etc/jodd-ai-host/policy.toml";

const DEFAULT_MAX_REQUEST_BYTES: u64 = 8_388_608;
const DEFAULT_MAX_CONCURRENT_RUNS: u32 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HarnessId {
    Claude,
    Codex,
}

impl HarnessId {
    pub fn all() -> [HarnessId; 2] {
        [HarnessId::Claude, HarnessId::Codex]
    }

    /// The harness named `name` (`claude`, `codex`), if any.
    pub fn from_name(name: &str) -> Option<HarnessId> {
        HarnessId::all().into_iter().find(|h| h.as_str() == name)
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            HarnessId::Claude => "claude",
            HarnessId::Codex => "codex",
        }
    }

    #[allow(dead_code)] // the admin surface (Task 7) writes it into new policies
    pub fn default_timeout(&self) -> u64 {
        match self {
            HarnessId::Claude => 120,
            HarnessId::Codex => 180,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessPolicy {
    pub enabled: bool,
    pub path: String,
    pub path_env: String,
    pub timeout_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    #[serde(default = "default_max_request_bytes")]
    pub max_request_bytes: u64,
    #[serde(default = "default_max_concurrent_runs")]
    pub max_concurrent_runs: u32,
    /// An absent entry means "undecided", which is different from `enabled = false`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub harness: BTreeMap<HarnessId, HarnessPolicy>,
    /// Reserved; unused in v1.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub keys: BTreeMap<String, toml::Value>,
}

fn default_max_request_bytes() -> u64 {
    DEFAULT_MAX_REQUEST_BYTES
}

fn default_max_concurrent_runs() -> u32 {
    DEFAULT_MAX_CONCURRENT_RUNS
}

#[derive(Debug, Clone, PartialEq)]
pub enum LoadOutcome {
    Loaded(Policy),
    Missing,
    /// Unreadable or unparseable; carries a human reason.
    Invalid(String),
}

/// Read the policy at `path`. Never panics and never guesses: anything but a clean parse is
/// `Missing` or `Invalid`.
pub fn load(path: &Path) -> LoadOutcome {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return LoadOutcome::Missing,
        Err(e) => return LoadOutcome::Invalid(format!("cannot read {}: {e}", path.display())),
    };
    match toml::from_str::<Policy>(&text) {
        Ok(p) => {
            // A zero timeout is ambiguous (instant kill? no limit?), so refuse it.
            if let Some((h, _)) = p.harness.iter().find(|(_, hp)| hp.timeout_secs == 0) {
                return LoadOutcome::Invalid(format!(
                    "harness.{}.timeout_secs must be greater than 0",
                    h.as_str()
                ));
            }
            LoadOutcome::Loaded(p)
        }
        Err(e) => LoadOutcome::Invalid(e.to_string()),
    }
}

/// Follow a chain of symlinks at the final path component (the target may not exist yet).
#[allow(dead_code)] // used by save_atomic
fn resolve_write_target(path: &Path) -> io::Result<std::path::PathBuf> {
    let mut cur = path.to_path_buf();
    for _ in 0..40 {
        match std::fs::symlink_metadata(&cur) {
            Ok(m) if m.file_type().is_symlink() => {
                let link = std::fs::read_link(&cur)?;
                cur = match cur.parent() {
                    Some(parent) => parent.join(link),
                    None => link,
                };
            }
            _ => return Ok(cur),
        }
    }
    Err(io::Error::other("too many levels of symbolic links"))
}

impl Policy {
    /// Defaults and no harnesses: what a missing or invalid policy means.
    pub fn empty() -> Policy {
        Policy {
            max_request_bytes: DEFAULT_MAX_REQUEST_BYTES,
            max_concurrent_runs: DEFAULT_MAX_CONCURRENT_RUNS,
            harness: BTreeMap::new(),
            keys: BTreeMap::new(),
        }
    }

    pub fn effective(o: &LoadOutcome) -> Policy {
        match o {
            LoadOutcome::Loaded(p) => p.clone(),
            LoadOutcome::Missing | LoadOutcome::Invalid(_) => Policy::empty(),
        }
    }

    /// `Some` only when the harness is enabled AND its path is absolute.
    pub fn enabled(&self, h: HarnessId) -> Option<&HarnessPolicy> {
        self.harness
            .get(&h)
            .filter(|hp| hp.enabled && Path::new(&hp.path).is_absolute())
    }

    /// Write via a same-directory temp file, fsync, mode 0644, rename over `path`.
    #[allow(dead_code)] // the admin surface (Task 7) saves the policy
    pub fn save_atomic(&self, path: &Path) -> io::Result<()> {
        let text = toml::to_string_pretty(self)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        // Write THROUGH a symlinked policy path (Docker: /etc -> state volume); the link stays.
        let path = resolve_write_target(path)?;
        let path = path.as_path();
        let dir = match path.parent() {
            Some(d) if !d.as_os_str().is_empty() => d,
            _ => Path::new("."),
        };
        let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
        tmp.write_all(text.as_bytes())?;
        tmp.as_file().sync_all()?;
        tmp.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o644))?;
        tmp.persist(path).map_err(|e| e.error)?;
        // Best effort: make the rename itself durable.
        if let Ok(d) = std::fs::File::open(dir) {
            let _ = d.sync_all();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    fn write(dir: &tempfile::TempDir, text: &str) -> std::path::PathBuf {
        let p = dir.path().join("policy.toml");
        fs::write(&p, text).unwrap();
        p
    }

    #[test]
    fn missing_file_is_missing_and_effective_is_empty() {
        let d = tempfile::tempdir().unwrap();
        let o = load(&d.path().join("nope.toml"));
        assert!(matches!(o, LoadOutcome::Missing));
        let p = Policy::effective(&o);
        assert!(p.harness.is_empty());
        assert_eq!(p.max_request_bytes, 8_388_608);
        assert_eq!(p.max_concurrent_runs, 2);
    }

    #[test]
    fn invalid_toml_is_empty_policy() {
        let d = tempfile::tempdir().unwrap();
        let o = load(&write(&d, "max_request_bytes = 'x'"));
        assert!(matches!(o, LoadOutcome::Invalid(ref r) if !r.is_empty()));
        assert!(Policy::effective(&o).harness.is_empty());
    }

    #[test]
    fn unknown_top_level_key_is_invalid() {
        let d = tempfile::tempdir().unwrap();
        let o = load(&write(&d, "max_request_byte = 5\n"));
        assert!(matches!(o, LoadOutcome::Invalid(_)));
    }

    #[test]
    fn unknown_harness_or_field_is_invalid() {
        let d = tempfile::tempdir().unwrap();
        let o = load(&write(
            &d,
            "[harness.pi]\nenabled=true\npath=\"/x\"\npath_env=\"/bin\"\ntimeout_secs=1\n",
        ));
        assert!(matches!(o, LoadOutcome::Invalid(_)));
        let o = load(&write(
            &d,
            "[harness.claude]\nenabled=true\npath=\"/x\"\npath_env=\"/bin\"\ntimeout_secs=1\nextra=1\n",
        ));
        assert!(matches!(o, LoadOutcome::Invalid(_)));
    }

    #[test]
    fn defaults_fill_absent_limits() {
        let d = tempfile::tempdir().unwrap();
        let o = load(&write(
            &d,
            "[harness.codex]\nenabled = true\npath = \"/x\"\npath_env = \"/bin\"\ntimeout_secs = 180\n",
        ));
        let LoadOutcome::Loaded(p) = o else {
            panic!("not loaded")
        };
        assert_eq!(p.max_request_bytes, 8_388_608);
        assert_eq!(p.max_concurrent_runs, 2);
        assert!(p.enabled(HarnessId::Codex).is_some());
    }

    #[test]
    fn disabled_or_relative_path_is_not_enabled() {
        let mut p = Policy::empty();
        let hp = |enabled, path: &str| HarnessPolicy {
            enabled,
            path: path.into(),
            path_env: "/bin".into(),
            timeout_secs: 120,
        };
        p.harness
            .insert(HarnessId::Claude, hp(false, "/usr/bin/claude"));
        p.harness.insert(HarnessId::Codex, hp(true, "codex"));
        assert!(p.enabled(HarnessId::Claude).is_none());
        assert!(p.enabled(HarnessId::Codex).is_none());
        p.harness
            .insert(HarnessId::Codex, hp(true, "/usr/bin/codex"));
        assert!(p.enabled(HarnessId::Codex).is_some());
    }

    #[test]
    fn absent_entry_is_undecided() {
        let d = tempfile::tempdir().unwrap();
        let o = load(&write(
            &d,
            "[harness.codex]\nenabled = false\npath = \"/x\"\npath_env = \"/bin\"\ntimeout_secs = 180\n",
        ));
        let LoadOutcome::Loaded(p) = o else {
            panic!("not loaded")
        };
        assert!(!p.harness.contains_key(&HarnessId::Claude));
        assert!(p.harness.contains_key(&HarnessId::Codex));
        assert!(p.enabled(HarnessId::Codex).is_none());
    }

    #[test]
    fn harness_helpers() {
        assert_eq!(HarnessId::all(), [HarnessId::Claude, HarnessId::Codex]);
        assert_eq!(HarnessId::Claude.as_str(), "claude");
        assert_eq!(HarnessId::Codex.as_str(), "codex");
        assert_eq!(HarnessId::Claude.default_timeout(), 120);
        assert_eq!(HarnessId::Codex.default_timeout(), 180);
    }

    #[test]
    fn save_then_load_round_trips() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("policy.toml");
        let mut p = Policy::empty();
        p.max_concurrent_runs = 3;
        p.harness.insert(
            HarnessId::Claude,
            HarnessPolicy {
                enabled: true,
                path: "/usr/local/bin/claude".into(),
                path_env: "/usr/local/bin:/usr/bin".into(),
                timeout_secs: 90,
            },
        );
        p.harness.insert(
            HarnessId::Codex,
            HarnessPolicy {
                enabled: false,
                path: "/usr/local/bin/codex".into(),
                path_env: "/usr/bin".into(),
                timeout_secs: 180,
            },
        );
        p.save_atomic(&path).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o644
        );
        let LoadOutcome::Loaded(q) = load(&path) else {
            panic!("not loaded")
        };
        assert_eq!(p, q);
        // no stray temp files left beside the target
        assert_eq!(fs::read_dir(d.path()).unwrap().count(), 1);
    }

    #[test]
    fn save_overwrites_existing_file() {
        let d = tempfile::tempdir().unwrap();
        let path = write(&d, "max_concurrent_runs = 9\n");
        Policy::empty().save_atomic(&path).unwrap();
        let LoadOutcome::Loaded(q) = load(&path) else {
            panic!("not loaded")
        };
        assert_eq!(q.max_concurrent_runs, 2);
    }

    #[test]
    fn zero_timeout_is_invalid() {
        let d = tempfile::tempdir().unwrap();
        let o = load(&write(
            &d,
            "[harness.claude]\nenabled = true\npath = \"/x\"\npath_env = \"/bin\"\ntimeout_secs = 0\n",
        ));
        assert!(matches!(o, LoadOutcome::Invalid(ref r) if r.contains("timeout_secs")));
        assert!(Policy::effective(&o).harness.is_empty());
    }

    #[test]
    fn zero_limits_are_accepted() {
        let d = tempfile::tempdir().unwrap();
        let o = load(&write(
            &d,
            "max_request_bytes = 0\nmax_concurrent_runs = 0\n",
        ));
        assert!(matches!(o, LoadOutcome::Loaded(_)));
    }

    #[test]
    fn save_through_file_symlink_keeps_link() {
        let d = tempfile::tempdir().unwrap();
        let vol = d.path().join("vol");
        let etc = d.path().join("etc");
        fs::create_dir(&vol).unwrap();
        fs::create_dir(&etc).unwrap();
        let target = vol.join("policy.toml");
        let link = etc.join("policy.toml");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        // dangling at first: the target is created through the link
        let mut p = Policy::empty();
        p.max_concurrent_runs = 5;
        p.save_atomic(&link).unwrap();
        assert!(fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(!fs::symlink_metadata(&target)
            .unwrap()
            .file_type()
            .is_symlink());
        // and again over an existing target
        p.max_concurrent_runs = 6;
        p.save_atomic(&link).unwrap();
        assert!(fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        let LoadOutcome::Loaded(q) = load(&target) else {
            panic!("not loaded")
        };
        assert_eq!(q.max_concurrent_runs, 6);
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o644
        );
        assert_eq!(fs::read_dir(&vol).unwrap().count(), 1);
        assert_eq!(fs::read_dir(&etc).unwrap().count(), 1);
    }
}
