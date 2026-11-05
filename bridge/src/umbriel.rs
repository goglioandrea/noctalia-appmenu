//! Umbriel focus tracker — the second [`FocusSink`] implementor.
//!
//! Umbriel's IPC is newline-delimited JSON on a Unix socket
//! (`$UMBRIEL_SOCKET`, see [`crate::wm::umbriel_socket_path`]). We send
//! `{"cmd":"subscribe","events":["windows"]}` and then read one line per
//! change. Each line is a **full snapshot** of the window list:
//!
//! ```json
//! {"event":"windows","data":[{"id":"…","app_id":"…","title":"…","pid":1234,"active":true,…}]}
//! ```
//!
//! A rejected subscription is reported as `{"err":"…"}` on the first line,
//! after which Umbriel closes the connection.
//!
//! ## Which window is "focused"
//!
//! Umbriel reports two flags per window: `focused` is *workspace-local*
//! remembered focus, and `active` is the one window holding seat keyboard
//! focus. `active` is the right signal — but when the AppMenu panel opens
//! it takes keyboard focus, so no window is `active` while the user is
//! browsing the menu. Treating that as a defocus would blank the menu the
//! user is reading. So [`decide`] keeps the previous window while it still
//! exists and only defocuses when it disappears.
//!
//! ## Window ids
//!
//! Umbriel ids are opaque strings; the rest of the bridge (the menu cache
//! key, `active.json`'s `focus_winid`) uses `u64`. [`IdMap`] hands out a
//! stable `u64` per string for the bridge's lifetime, and the string itself
//! travels in [`FocusEvent::window`] so the click path can focus it.

use crate::config::Config;
use crate::focus::{FocusEvent, FocusOp, FocusRunFuture, FocusSink};
use crate::niri::{next_backoff, BACKOFF_FLOOR, BACKOFF_MAX};
use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::watch;
use tracing::{debug, info, warn};

/// The subset of an Umbriel `windows` entry the bridge needs. Every field
/// defaults so a schema addition or omission never fails the parse.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct UmbrielWindow {
    /// ext-foreign-toplevel identifier (may be empty for odd surfaces).
    pub id: String,
    /// Wayland app-id.
    pub app_id: String,
    /// Window title.
    pub title: String,
    /// Owning process id; `-1` when unknown (an X11 client without
    /// `_NET_WM_PID`).
    pub pid: i64,
    /// Holds seat keyboard focus.
    pub active: bool,
}

/// One line of the subscription stream.
#[derive(Debug, Deserialize)]
struct StreamLine {
    #[serde(default)]
    event: Option<String>,
    #[serde(default)]
    data: Option<serde_json::Value>,
    #[serde(default)]
    err: Option<String>,
}

/// What a stream line means for the bridge.
#[derive(Debug, PartialEq, Eq)]
pub enum Parsed {
    /// A `windows` snapshot.
    Windows(Vec<UmbrielWindow>),
    /// Umbriel rejected the subscription.
    Rejected(String),
    /// Another event family, or a line we do not understand.
    Ignored,
}

/// Parse one stream line.
#[must_use]
pub fn parse_line(line: &str) -> Parsed {
    let Ok(parsed) = serde_json::from_str::<StreamLine>(line.trim()) else {
        return Parsed::Ignored;
    };
    if let Some(err) = parsed.err {
        return Parsed::Rejected(err);
    }
    if parsed.event.as_deref() != Some("windows") {
        return Parsed::Ignored;
    }
    match parsed
        .data
        .map(serde_json::from_value::<Vec<UmbrielWindow>>)
    {
        Some(Ok(windows)) => Parsed::Windows(windows),
        _ => Parsed::Ignored,
    }
}

/// Stable `u64` handles for Umbriel's string window ids.
#[derive(Debug, Default)]
pub struct IdMap {
    next: u64,
    ids: HashMap<String, u64>,
}

impl IdMap {
    /// The handle for `id`, allocating one on first sight. Handles start
    /// at 1 because `0` means "no window" downstream.
    pub fn handle(&mut self, id: &str) -> u64 {
        if let Some(h) = self.ids.get(id) {
            return *h;
        }
        self.next += 1;
        self.ids.insert(id.to_string(), self.next);
        self.next
    }

    /// Forget ids that are no longer in `windows`, so a long session does
    /// not grow the map without bound.
    pub fn retain_present(&mut self, windows: &[UmbrielWindow]) {
        self.ids.retain(|id, _| windows.iter().any(|w| &w.id == id));
    }
}

/// Pure focus decision for one snapshot.
///
/// `previous` is the compositor id of the window we last emitted (if any).
#[must_use]
pub fn decide(windows: &[UmbrielWindow], previous: Option<&str>, ids: &mut IdMap) -> FocusOp {
    if let Some(w) = windows.iter().find(|w| w.active) {
        return match u32::try_from(w.pid) {
            Ok(pid) if pid > 0 && !w.id.is_empty() => FocusOp::Emit(FocusEvent {
                winid: ids.handle(&w.id),
                pid,
                app_id: w.app_id.clone(),
                title: w.title.clone(),
                window: w.id.clone(),
            }),
            // Active window whose owner we cannot identify: we cannot
            // match its accessible tree, and keeping the previous app's
            // menu would show the wrong menu. Clear it.
            _ => FocusOp::Defocus,
        };
    }
    // Nothing active: a layer surface (our own panel, the launcher …) took
    // the keyboard. Keep the previous window while it still exists.
    match previous {
        Some(prev) if windows.iter().any(|w| w.id == prev) => FocusOp::NoChange,
        Some(_) => FocusOp::Defocus,
        None => FocusOp::NoChange,
    }
}

/// The Umbriel [`FocusSink`].
#[derive(Debug, Default, Clone, Copy)]
pub struct UmbrielFocusSink;

impl UmbrielFocusSink {
    /// Construct the sink. Cheap; all state is per `run`.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl FocusSink for UmbrielFocusSink {
    fn run(self, tx: watch::Sender<Option<FocusEvent>>, cfg: Config) -> FocusRunFuture {
        Box::pin(run(tx, cfg))
    }
}

/// Reconnect loop, same backoff contract as the niri sink.
///
/// # Errors
/// Never in practice: session errors are logged and retried; returns
/// `Ok(())` once `tx` is closed.
pub async fn run(tx: watch::Sender<Option<FocusEvent>>, _cfg: Config) -> Result<()> {
    let mut backoff = BACKOFF_FLOOR;
    loop {
        if tx.is_closed() {
            return Ok(());
        }
        let started = std::time::Instant::now();
        let result = match crate::wm::umbriel_socket_path() {
            Some(path) => run_once_at(&path, &tx).await,
            None => Err(anyhow!(
                "cannot locate the Umbriel IPC socket (set UMBRIEL_SOCKET or WAYLAND_DISPLAY)"
            )),
        };
        match result {
            Ok(()) => return Ok(()),
            Err(e) => {
                let session = started.elapsed();
                backoff = next_backoff(backoff, session);
                warn!(
                    error = ?e,
                    backoff_ms = backoff.as_millis(),
                    session_secs = session.as_secs(),
                    "umbriel IPC session ended; reconnecting after backoff"
                );
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(BACKOFF_MAX);
            }
        }
    }
}

/// One subscription session against `socket_path`.
///
/// # Errors
/// When the socket cannot be reached, the subscription is rejected, or
/// the stream closes.
pub async fn run_once_at(
    socket_path: &std::path::Path,
    tx: &watch::Sender<Option<FocusEvent>>,
) -> Result<()> {
    let stream = UnixStream::connect(socket_path)
        .await
        .with_context(|| format!("connecting to Umbriel socket at {}", socket_path.display()))?;
    let (rd, mut wr) = stream.into_split();
    wr.write_all(b"{\"cmd\":\"subscribe\",\"events\":[\"windows\"]}\n")
        .await
        .context("writing Umbriel subscribe request")?;
    info!("subscribed to umbriel window events");
    // Keep `wr` alive for the session: Umbriel may treat a half-closed
    // connection as finished.
    let mut rd = BufReader::new(rd);
    let r = drive(&mut rd, tx).await;
    drop(wr);
    r
}

/// Read stream lines until EOF, publishing focus changes to `tx`.
///
/// # Errors
/// On read failure, rejection, or EOF (so the caller reconnects).
pub async fn drive<R>(rd: &mut R, tx: &watch::Sender<Option<FocusEvent>>) -> Result<()>
where
    R: AsyncBufRead + Unpin,
{
    let mut ids = IdMap::default();
    let mut previous: Option<String> = None;
    let mut line = String::new();
    loop {
        if tx.is_closed() {
            return Ok(());
        }
        line.clear();
        let n = rd
            .read_line(&mut line)
            .await
            .context("reading Umbriel event stream")?;
        if n == 0 {
            return Err(anyhow!("Umbriel closed the event stream"));
        }
        match parse_line(&line) {
            Parsed::Rejected(err) => return Err(anyhow!("Umbriel rejected subscription: {err}")),
            Parsed::Ignored => {}
            Parsed::Windows(windows) => {
                match decide(&windows, previous.as_deref(), &mut ids) {
                    FocusOp::Emit(evt) => {
                        previous = Some(evt.window.clone());
                        // Snapshots arrive for every window-state change
                        // (geometry, other windows' titles …); only wake
                        // the pipeline when *our* focus event changed.
                        tx.send_if_modified(|cur| {
                            if cur.as_ref() == Some(&evt) {
                                false
                            } else {
                                *cur = Some(evt);
                                true
                            }
                        });
                    }
                    FocusOp::Defocus => {
                        previous = None;
                        tx.send_if_modified(|cur| cur.take().is_some());
                    }
                    FocusOp::NoChange | FocusOp::UnknownWindow(_) => {}
                }
                ids.retain_present(&windows);
                debug!(windows = windows.len(), "umbriel windows snapshot");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn win(id: &str, pid: i64, active: bool) -> UmbrielWindow {
        UmbrielWindow {
            id: id.into(),
            app_id: format!("app.{id}"),
            title: format!("title {id}"),
            pid,
            active,
        }
    }

    #[test]
    fn parses_windows_snapshot_and_ignores_extra_fields() {
        let line = r#"{"event":"windows","data":[{"id":"w1","app_id":"org.kde.kate","title":"Kate","pid":42,"active":true,"floating":false,"workspace":"1"}]}"#;
        let Parsed::Windows(w) = parse_line(line) else {
            panic!("expected windows");
        };
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].pid, 42);
        assert!(w[0].active);
    }

    #[test]
    fn parses_rejection_and_ignores_other_families() {
        assert_eq!(
            parse_line(r#"{"err":"unknown subscription event: windows"}"#),
            Parsed::Rejected("unknown subscription event: windows".into())
        );
        assert_eq!(
            parse_line(r#"{"event":"workspaces","data":[]}"#),
            Parsed::Ignored
        );
        assert_eq!(parse_line("not json"), Parsed::Ignored);
    }

    #[test]
    fn active_window_is_emitted_with_stable_handle() {
        let mut ids = IdMap::default();
        let ws = [win("a", 10, false), win("b", 20, true)];
        let FocusOp::Emit(e) = decide(&ws, None, &mut ids) else {
            panic!("expected emit");
        };
        assert_eq!((e.pid, e.window.as_str(), e.winid), (20, "b", 1));
        let FocusOp::Emit(again) = decide(&ws, Some("b"), &mut ids) else {
            panic!("expected emit");
        };
        assert_eq!(again.winid, 1, "same string id → same handle");
    }

    #[test]
    fn panel_taking_keyboard_keeps_previous_window() {
        let mut ids = IdMap::default();
        let ws = [win("a", 10, false), win("b", 20, false)];
        assert_eq!(decide(&ws, Some("b"), &mut ids), FocusOp::NoChange);
        // …until that window closes.
        assert_eq!(
            decide(&[win("a", 10, false)], Some("b"), &mut ids),
            FocusOp::Defocus
        );
    }

    #[test]
    fn unknown_pid_defocuses() {
        let mut ids = IdMap::default();
        assert_eq!(
            decide(&[win("x", -1, true)], None, &mut ids),
            FocusOp::Defocus
        );
    }

    #[test]
    fn id_map_forgets_closed_windows() {
        let mut ids = IdMap::default();
        ids.handle("a");
        ids.handle("b");
        ids.retain_present(&[win("b", 1, false)]);
        assert_eq!(ids.ids.len(), 1);
    }

    #[tokio::test]
    async fn drive_publishes_only_real_focus_changes() {
        let stream = concat!(
            r#"{"event":"windows","data":[{"id":"a","app_id":"A","title":"t","pid":5,"active":true}]}"#,
            "\n",
            // geometry-only change: same focus event
            r#"{"event":"windows","data":[{"id":"a","app_id":"A","title":"t","pid":5,"active":true}]}"#,
            "\n",
        );
        let (tx, mut rx) = watch::channel(None);
        let mut rd = BufReader::new(stream.as_bytes());
        let err = drive(&mut rd, &tx).await.unwrap_err();
        assert!(err.to_string().contains("closed"));
        assert!(rx.has_changed().unwrap());
        let got = rx.borrow_and_update().clone().unwrap();
        assert_eq!(got.window, "a");
        assert!(!rx.has_changed().unwrap());
    }
}
