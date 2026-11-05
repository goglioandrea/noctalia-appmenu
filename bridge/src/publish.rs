//! Where the active-menu snapshot goes once it is built.
//!
//! Two sinks, replacing upstream's `qs ipc call` push (Quickshell does not
//! exist in Noctalia v5):
//!
//! 1. **`active.json`** in a private per-user directory
//!    (`$XDG_CACHE_HOME/noctalia-appmenu/`, mode `0700`, file mode `0600`),
//!    written atomically (temp file + `rename`) so a reader never sees a
//!    half-written payload. Menus can carry sensitive strings (window
//!    titles, recent-file names, bookmarks), hence the permissions.
//!
//! 2. **stdout stream** (`--stream` mode), consumed by the Noctalia plugin
//!    through `noctalia.runStream`. One JSON object per line:
//!
//!    ```json
//!    {"event":"active","seq":7,"data":{…the active.json payload…}}
//!    {"event":"active","seq":8,"file":"/home/u/.cache/noctalia-appmenu/active.json"}
//!    {"event":"heartbeat"}
//!    ```
//!
//!    Noctalia drops stream lines longer than 64 KiB, so payloads that do
//!    not fit are announced by path instead and the plugin reads the file.
//!    If writing to stdout fails (the shell went away and closed the pipe)
//!    the bridge exits — it is the plugin's child and has no reason to
//!    outlive it.

use anyhow::{bail, Context, Result};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

/// Largest payload sent inline on the stream. Noctalia's per-line cap is
/// 64 KiB including the envelope; leave headroom.
pub const MAX_INLINE_BYTES: usize = 60 * 1024;

/// Interval between heartbeat lines in stream mode. The plugin restarts
/// the bridge when it hears nothing for several intervals.
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);

static STREAM_ENABLED: AtomicBool = AtomicBool::new(false);
static SEQ: AtomicU64 = AtomicU64::new(0);
static STDOUT_LOCK: Mutex<()> = Mutex::new(());

/// Turn on the stdout stream for this process.
pub fn enable_stream() {
    STREAM_ENABLED.store(true, Ordering::SeqCst);
}

/// Whether `--stream` mode is on.
#[must_use]
pub fn stream_enabled() -> bool {
    STREAM_ENABLED.load(Ordering::SeqCst)
}

/// Resolve the private directory holding `active.json`.
///
/// Upstream fell back to the world-writable `/tmp` when `HOME` was unset,
/// which invites symlink and pre-creation attacks. We fall back to
/// `$XDG_RUNTIME_DIR` (per-user, `0700` by spec) and otherwise refuse.
///
/// # Errors
/// When none of `XDG_CACHE_HOME`, `HOME`, `XDG_RUNTIME_DIR` is usable.
pub fn state_dir() -> Result<PathBuf> {
    let absolute = |v: std::ffi::OsString| {
        let p = PathBuf::from(v);
        p.is_absolute().then_some(p)
    };
    let base = std::env::var_os("XDG_CACHE_HOME")
        .and_then(absolute)
        .or_else(|| {
            std::env::var_os("HOME")
                .and_then(absolute)
                .map(|h| h.join(".cache"))
        })
        .or_else(|| std::env::var_os("XDG_RUNTIME_DIR").and_then(absolute));
    match base {
        Some(b) => Ok(b.join("noctalia-appmenu")),
        None => bail!("none of XDG_CACHE_HOME, HOME, XDG_RUNTIME_DIR is set to an absolute path"),
    }
}

/// Create `dir` as a private (`0700`) directory, or tighten an existing
/// one. Refuses a symlink in its place.
///
/// # Errors
/// When the directory cannot be created, is a symlink, or is not a
/// directory.
pub fn ensure_private_dir(dir: &Path) -> Result<()> {
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    match std::fs::symlink_metadata(dir) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                bail!("{} is a symlink; refusing to use it", dir.display());
            }
            if !meta.is_dir() {
                bail!("{} exists and is not a directory", dir.display());
            }
            if meta.permissions().mode() & 0o077 != 0 {
                std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
                    .with_context(|| format!("restricting permissions on {}", dir.display()))?;
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(dir)
                .with_context(|| format!("creating {}", dir.display()))?;
        }
        Err(e) => return Err(e).with_context(|| format!("inspecting {}", dir.display())),
    }
    Ok(())
}

/// Atomically replace `path` with `body`, mode `0600`.
///
/// # Errors
/// On any filesystem failure.
pub fn write_private_atomic(path: &Path, body: &[u8]) -> Result<()> {
    let dir = path
        .parent()
        .context("active.json path has no parent directory")?;
    let tmp = dir.join(format!(
        ".{}.tmp",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("active.json")
    ));
    // A stale temp file from a crash is ours (private dir); clear it so
    // `create_new` can enforce a fresh inode with our mode.
    match std::fs::remove_file(&tmp) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("removing {}", tmp.display())),
    }
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .with_context(|| format!("creating {}", tmp.display()))?;
    f.write_all(body)
        .with_context(|| format!("writing {}", tmp.display()))?;
    drop(f);
    std::fs::rename(&tmp, path)
        .with_context(|| format!("renaming {} over {}", tmp.display(), path.display()))?;
    Ok(())
}

/// Build the stream line announcing `body` (the `active.json` payload).
/// Pure, for tests.
#[must_use]
pub fn active_line(seq: u64, body: &str, file: &Path) -> String {
    if body.len() <= MAX_INLINE_BYTES {
        // `body` is serde_json output, i.e. valid JSON: splice it in raw
        // rather than re-parsing it.
        format!("{{\"event\":\"active\",\"seq\":{seq},\"data\":{body}}}")
    } else {
        serde_json::json!({
            "event": "active",
            "seq": seq,
            "file": file.to_string_lossy(),
        })
        .to_string()
    }
}

/// Write one line to stdout; exit the process if stdout is gone.
fn emit_line(line: &str) {
    let _guard = STDOUT_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut out = std::io::stdout().lock();
    let ok = out
        .write_all(line.as_bytes())
        .and_then(|()| out.write_all(b"\n"))
        .and_then(|()| out.flush());
    if let Err(e) = ok {
        tracing::warn!(error = ?e, "stdout closed (shell gone?); exiting");
        std::process::exit(0);
    }
}

/// Announce a new snapshot on the stream (no-op outside `--stream`).
pub fn announce_active(body: &str, file: &Path) {
    if !stream_enabled() {
        return;
    }
    let seq = SEQ.fetch_add(1, Ordering::SeqCst) + 1;
    emit_line(&active_line(seq, body, file));
}

/// Heartbeat loop for `--stream` mode. Never returns.
pub async fn heartbeat() {
    let mut tick = tokio::time::interval(HEARTBEAT_INTERVAL);
    loop {
        tick.tick().await;
        emit_line("{\"event\":\"heartbeat\"}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_payloads_go_inline_large_ones_by_path() {
        let file = Path::new("/x/active.json");
        let small = active_line(3, "{\"v\":1}", file);
        let v: serde_json::Value = serde_json::from_str(&small).unwrap();
        assert_eq!(v["seq"], 3);
        assert_eq!(v["data"]["v"], 1);

        let big_body = format!("{{\"pad\":\"{}\"}}", "x".repeat(MAX_INLINE_BYTES));
        let big = active_line(4, &big_body, file);
        assert!(big.len() < 1024);
        let v: serde_json::Value = serde_json::from_str(&big).unwrap();
        assert_eq!(v["file"], "/x/active.json");
        assert!(v.get("data").is_none());
    }

    #[test]
    fn private_dir_and_atomic_write() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("noctalia-appmenu");
        ensure_private_dir(&dir).unwrap();
        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );

        let file = dir.join("active.json");
        write_private_atomic(&file, b"one").unwrap();
        write_private_atomic(&file, b"two").unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "two");
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(!dir.join(".active.json.tmp").exists());
    }

    #[test]
    fn loose_dir_permissions_are_tightened_and_symlinks_refused() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("loose");
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        ensure_private_dir(&dir).unwrap();
        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );

        let link = root.path().join("link");
        std::os::unix::fs::symlink(&dir, &link).unwrap();
        assert!(ensure_private_dir(&link).is_err());
    }
}
