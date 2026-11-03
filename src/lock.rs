//! Locks credhop shares with Claude Code and Codex, so it never reads or
//! replaces live credentials while a tool refreshes or writes them.

use std::fs::{self, File, TryLockError};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::fsutil::open_lock_file;

/// How long to wait for a tool to release a lock.
const TIMEOUT: Duration = Duration::from_secs(10);

/// Locks held on the live credentials of one tool until dropped.
#[derive(Default)]
pub struct Locks {
    dirs: Vec<DirLock>,
    files: Vec<File>,
}

impl Locks {
    /// Waits for, then holds, the lock directory `path`; see `DirLock`.
    pub fn add_dir(&mut self, path: &Path, stale: Duration) -> Result<()> {
        self.dirs.push(DirLock::acquire(path, stale, TIMEOUT)?);
        Ok(())
    }

    /// Waits for, then holds, an exclusive lock on the file `path`, creating it if missing.
    pub fn add_file(&mut self, path: &Path) -> Result<()> {
        self.files.push(lock_file(path, TIMEOUT)?);
        Ok(())
    }
}

/// A lock in the format of the `proper-lockfile` package, which Claude Code
/// uses: the directory exists while the lock is held.
struct DirLock {
    path: PathBuf,
}

impl DirLock {
    /// Creates the directory `path`, waiting up to `timeout` while it exists.
    /// Like proper-lockfile, takes over a directory last modified more than
    /// `stale` ago, which a process that died left behind.
    fn acquire(path: &Path, stale: Duration, timeout: Duration) -> Result<Self> {
        let deadline = Instant::now() + timeout;
        loop {
            match fs::create_dir(path) {
                Ok(()) => {
                    return Ok(Self {
                        path: path.to_owned(),
                    });
                }
                Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                    if is_stale(path, stale) && fs::remove_dir(path).is_ok() {
                        continue;
                    }
                }
                Err(e) => {
                    return Err(e).with_context(|| format!("failed to create {}", path.display()));
                }
            }
            wait_until(deadline, path)?;
        }
    }
}

impl Drop for DirLock {
    fn drop(&mut self) {
        let _ = fs::remove_dir(&self.path);
    }
}

fn is_stale(path: &Path, stale: Duration) -> bool {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .is_ok_and(|modified| modified.elapsed().is_ok_and(|age| age > stale))
}

/// Locks the file `path` exclusively, waiting up to `timeout` while another process holds it.
fn lock_file(path: &Path, timeout: Duration) -> Result<File> {
    let deadline = Instant::now() + timeout;
    let file = open_lock_file(path)?;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(TryLockError::WouldBlock) => wait_until(deadline, path)?,
            Err(TryLockError::Error(e)) => {
                return Err(e).with_context(|| format!("failed to lock {}", path.display()));
            }
        }
    }
}

/// Sleeps before the next attempt to take the lock `path`, or fails once `deadline` has passed.
fn wait_until(deadline: Instant, path: &Path) -> Result<()> {
    if Instant::now() >= deadline {
        bail!(
            "{} is still locked; another process is using the credentials, try again",
            path.display()
        );
    }
    thread::sleep(Duration::from_millis(50));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    const LONG: Duration = Duration::from_secs(60);
    const SHORT: Duration = Duration::from_millis(200);

    #[test]
    fn dir_lock_creates_the_directory_and_removes_it_when_dropped() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("x.lock");

        let lock = DirLock::acquire(&path, LONG, SHORT).unwrap();
        assert!(path.is_dir());

        drop(lock);
        assert!(!path.exists());
    }

    #[test]
    fn dir_lock_times_out_while_another_process_holds_it() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("x.lock");
        fs::create_dir(&path).unwrap();

        let started = Instant::now();
        let err = DirLock::acquire(&path, LONG, SHORT).err().unwrap();

        assert!(started.elapsed() >= SHORT);
        assert_eq!(
            err.to_string(),
            format!(
                "{} is still locked; another process is using the credentials, try again",
                path.display()
            )
        );
        assert!(path.is_dir(), "the other process's lock is kept");
    }

    #[test]
    fn dir_lock_waits_for_the_holder_to_release_it() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("x.lock");
        fs::create_dir(&path).unwrap();
        let holder = {
            let path = path.clone();
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(100));
                fs::remove_dir(path).unwrap();
            })
        };

        let lock = DirLock::acquire(&path, LONG, Duration::from_secs(5)).unwrap();

        holder.join().unwrap();
        assert!(path.is_dir());
        drop(lock);
    }

    #[test]
    fn dir_lock_takes_over_a_stale_lock() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("x.lock");
        fs::create_dir(&path).unwrap();
        thread::sleep(Duration::from_millis(50));

        let lock = DirLock::acquire(&path, Duration::from_millis(10), SHORT).unwrap();

        assert!(path.is_dir());
        drop(lock);
        assert!(!path.exists());
    }

    #[test]
    fn dir_lock_reports_a_missing_parent_directory() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("missing/x.lock");

        let err = DirLock::acquire(&path, LONG, SHORT).err().unwrap();

        assert_eq!(
            err.to_string(),
            format!("failed to create {}", path.display())
        );
    }

    #[test]
    fn file_lock_is_exclusive_until_dropped() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("auth.lock");

        let held = lock_file(&path, SHORT).unwrap();
        let other = File::open(&path).unwrap();
        assert!(matches!(other.try_lock(), Err(TryLockError::WouldBlock)));

        drop(held);
        other.try_lock().unwrap();
    }

    #[test]
    fn file_lock_times_out_while_another_process_holds_it() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("auth.lock");
        let other = File::create(&path).unwrap();
        other.lock().unwrap();

        let err = lock_file(&path, SHORT).err().unwrap();

        assert_eq!(
            err.to_string(),
            format!(
                "{} is still locked; another process is using the credentials, try again",
                path.display()
            )
        );
    }
}
