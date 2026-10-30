//! Concurrency slots. Each SSH call is its own process, so the limit
//! (`max_concurrent_runs`) is held across processes: one file per slot,
//! `0..n`, in a lock directory, and a run holds an exclusive `flock` on one of
//! them. The kernel drops the lock when the guard drops or the process dies,
//! so a crashed run never leaks its slot.
//!
//! `flock` locks belong to the open file description, so two acquires in ONE
//! process (the MCP server's concurrent calls) also exclude each other.
//!
//! Where the directory lives (ruling R2): `/run/jodd-ai-host/slots` when it
//! exists or can be created and is writable, else a per-user dir
//! (`$XDG_RUNTIME_DIR/jodd-ai-host/slots`, else `$HOME/.cache/jodd-ai-host/slots`).
//! Never a shared `/tmp` path: another local user could pre-create it and hold
//! every slot. Tests point it elsewhere with `JODD_AI_MCP_SLOTS_DIR` (debug
//! builds only, ruling R12).

use std::fs::{DirBuilder, File, OpenOptions};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use nix::fcntl::{Flock, FlockArg, OFlag};
use nix::unistd::{access, AccessFlags};

const RUN_DIR: &str = "/run/jodd-ai-host/slots";

pub struct Slots {
    /// `None` when no usable lock directory exists: every acquire then fails
    /// (closed), rather than running unbounded.
    dir: Option<PathBuf>,
    n: u32,
}

/// Holds one slot until dropped.
#[derive(Debug)]
pub struct SlotGuard {
    _lock: Flock<File>,
}

impl Slots {
    /// `n` slots in the default lock directory. `n == 0` admits nothing.
    pub fn new(n: u32) -> Self {
        Slots {
            dir: default_dir(),
            n,
        }
    }

    /// The first free slot, without waiting; `None` when all `n` are held.
    pub fn try_acquire(&self) -> Option<SlotGuard> {
        let dir = self.dir.as_ref()?;
        (0..self.n).find_map(|i| {
            let file = open_slot(&dir.join(i.to_string())).ok()?;
            Flock::lock(file, FlockArg::LockExclusiveNonblock)
                .ok()
                .map(|lock| SlotGuard { _lock: lock })
        })
    }
}

/// Open (creating) one slot file, never through a symlink: a planted link
/// must not make us create or lock a file elsewhere. std opens with
/// `O_CLOEXEC`, so the CLI child never inherits (and so never outlives us
/// holding) the lock.
fn open_slot(p: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(OFlag::O_NOFOLLOW.bits())
        .open(p)
}

fn default_dir() -> Option<PathBuf> {
    if let Some(d) = crate::test_override("JODD_AI_MCP_SLOTS_DIR") {
        return usable(PathBuf::from(d));
    }
    let per_user = |var: &str, rel: &str| {
        std::env::var_os(var)
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .map(|p| p.join(rel))
    };
    [
        Some(PathBuf::from(RUN_DIR)),
        per_user("XDG_RUNTIME_DIR", "jodd-ai-host/slots"),
        per_user("HOME", ".cache/jodd-ai-host/slots"),
    ]
    .into_iter()
    .flatten()
    .find_map(usable)
}

/// `d`, created (mode 0700) if absent, when we can make files in it.
fn usable(d: PathBuf) -> Option<PathBuf> {
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&d)
        .ok()?;
    access(&d, AccessFlags::W_OK | AccessFlags::X_OK).ok()?;
    Some(d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slots(dir: &Path, n: u32) -> Slots {
        Slots {
            dir: Some(dir.to_path_buf()),
            n,
        }
    }

    #[test]
    fn n_slots_then_none_then_one_back_after_a_drop() {
        let d = tempfile::tempdir().unwrap();
        let s = slots(d.path(), 2);
        let a = s.try_acquire().expect("first");
        let b = s.try_acquire().expect("second");
        assert!(s.try_acquire().is_none(), "a third must be refused");
        drop(a);
        let c = s.try_acquire().expect("a dropped guard frees its slot");
        assert!(s.try_acquire().is_none());
        drop((b, c));
    }

    #[test]
    fn two_slots_objects_on_one_dir_share_the_limit() {
        // As two processes would: separate opens of the same files.
        let d = tempfile::tempdir().unwrap();
        let (s1, s2) = (slots(d.path(), 1), slots(d.path(), 1));
        let g = s1.try_acquire().unwrap();
        assert!(s2.try_acquire().is_none());
        drop(g);
        assert!(s2.try_acquire().is_some());
    }

    #[test]
    fn zero_slots_or_no_dir_admits_nothing() {
        let d = tempfile::tempdir().unwrap();
        assert!(slots(d.path(), 0).try_acquire().is_none());
        assert!(Slots { dir: None, n: 4 }.try_acquire().is_none());
    }

    #[test]
    fn a_symlinked_slot_file_is_not_followed() {
        let d = tempfile::tempdir().unwrap();
        let target = d.path().join("elsewhere");
        std::os::unix::fs::symlink(&target, d.path().join("0")).unwrap();
        let s = slots(d.path(), 1);
        assert!(s.try_acquire().is_none(), "slot 0 is a symlink: unusable");
        assert!(!target.exists(), "nothing created through the link");
    }

    #[test]
    fn usable_creates_the_dir_private() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let p = usable(d.path().join("a/b")).unwrap();
        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }
}
