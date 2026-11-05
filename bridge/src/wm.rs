//! Compositor abstraction for the bits of the bridge that *act* on the
//! compositor rather than observe it: detecting which compositor is
//! running, focusing a window by id before a menu click, and running the
//! synthetic "Window" menu actions.
//!
//! Two compositors are supported:
//!
//! - **niri** — IPC over `$NIRI_SOCKET` via the `niri-ipc` crate. Window
//!   ids are `u64`.
//! - **Umbriel** — JSON-lines IPC over `$UMBRIEL_SOCKET`, falling back to
//!   `$XDG_RUNTIME_DIR/umbriel-$WAYLAND_DISPLAY.sock` (the same rule the
//!   `umbriel` CLI follows). Window ids are opaque strings (the
//!   ext-foreign-toplevel identifier).
//!
//! The synthetic window menu is authored in niri's action vocabulary
//! (`niri:<action>` paths, unchanged from upstream so the menu model and
//! its tests stay stable). [`umbriel_action_for`] translates each one to
//! Umbriel's equivalent; actions with no equivalent are pruned from the
//! menu by [`retain_supported_actions`] so the user is never shown an item
//! that cannot work.
//!
//! ## Security
//!
//! Every action string that reaches a compositor passes through
//! [`is_allowed_niri_action`], a closed allow-list. The click path takes
//! `(service, path)` from the menu tree, and although the tree's
//! synthetic items are built by the bridge itself, the strings travel
//! through `active.json` and a CLI argument — so the dispatcher must not
//! trust them to name an arbitrary compositor action (niri's `spawn`, or
//! Umbriel's `config-replace`, would be arbitrary code execution).

use anyhow::{anyhow, bail, Context, Result};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// Which compositor the bridge is talking to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compositor {
    /// niri (`$NIRI_SOCKET`).
    Niri,
    /// Umbriel (`$UMBRIEL_SOCKET` or the XDG runtime fallback).
    Umbriel,
    /// Hyprland (`$HYPRLAND_INSTANCE_SIGNATURE`).
    Hyprland,
}

impl Compositor {
    /// Lower-case name used in logs, `active.json` and the
    /// `NOCTALIA_APPMENU_COMPOSITOR` override.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Niri => "niri",
            Self::Umbriel => "umbriel",
            Self::Hyprland => "hyprland",
        }
    }
}

/// Environment variable that forces the compositor choice (`niri`,
/// `umbriel` or `hyprland`). Useful when both sockets exist, e.g. a nested session.
pub const COMPOSITOR_OVERRIDE_ENV: &str = "NOCTALIA_APPMENU_COMPOSITOR";

/// The compositor chosen at daemon start. Read by the menu builders so
/// they only offer actions the running compositor supports. Unset in
/// unit tests, where builders behave as on niri (upstream behaviour).
static ACTIVE: OnceLock<Compositor> = OnceLock::new();

/// Record the compositor for the lifetime of the process. Later calls
/// are ignored.
pub fn set_active(c: Compositor) {
    let _ = ACTIVE.set(c);
}

/// The compositor recorded by [`set_active`], if any.
#[must_use]
pub fn active() -> Option<Compositor> {
    ACTIVE.get().copied()
}

/// Pure detection rule, split out for tests. `override_value` is the
/// `NOCTALIA_APPMENU_COMPOSITOR` value; the booleans say whether a usable
/// socket exists for each compositor.
#[must_use]
pub fn choose(
    override_value: Option<&str>,
    niri_available: bool,
    umbriel_available: bool,
    hyprland_available: bool,
) -> Option<Compositor> {
    match override_value
        .map(str::trim)
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("niri") => return Some(Compositor::Niri),
        Some("umbriel") => return Some(Compositor::Umbriel),
        Some("hyprland") => return Some(Compositor::Hyprland),
        _ => {}
    }
    if umbriel_available {
        Some(Compositor::Umbriel)
    } else if niri_available {
        Some(Compositor::Niri)
    } else if hyprland_available {
        Some(Compositor::Hyprland)
    } else {
        None
    }
}

/// Detect the running compositor from the environment.
#[must_use]
pub fn detect() -> Option<Compositor> {
    let override_value = std::env::var(COMPOSITOR_OVERRIDE_ENV).ok();
    let niri_available = std::env::var_os(niri_ipc::socket::SOCKET_PATH_ENV)
        .map(PathBuf::from)
        .is_some_and(|p| p.exists());
    let umbriel_available = umbriel_socket_path().is_some_and(|p| p.exists());
    let hyprland_available = crate::hyprland::command_socket().is_some();
    choose(
        override_value.as_deref(),
        niri_available,
        umbriel_available,
        hyprland_available,
    )
}

/// Umbriel's IPC socket: `$UMBRIEL_SOCKET`, else
/// `$XDG_RUNTIME_DIR/umbriel-$WAYLAND_DISPLAY.sock`. A shell started by
/// systemd can inherit `WAYLAND_DISPLAY` without `UMBRIEL_SOCKET`, which
/// is why the fallback matters.
#[must_use]
pub fn umbriel_socket_path() -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os("UMBRIEL_SOCKET").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(explicit));
    }
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty())?;
    let display = std::env::var("WAYLAND_DISPLAY")
        .ok()
        .filter(|v| !v.is_empty())?;
    // WAYLAND_DISPLAY may be an absolute socket path; Umbriel names its
    // IPC socket after the display *name*, so a path cannot be mapped.
    if display.contains('/') {
        return None;
    }
    Some(PathBuf::from(runtime).join(format!("umbriel-{display}.sock")))
}

/// Closed allow-list of niri actions the synthetic menu may trigger.
/// Anything else is rejected before it reaches a compositor.
const ALLOWED_NIRI_ACTIONS: &[&str] = &[
    "close-window",
    "fullscreen-window",
    "toggle-window-floating",
    "maximize-column",
    "center-column",
    "expand-column-to-available-width",
    "move-window-to-workspace-up",
    "move-window-to-workspace-down",
    "move-window-to-monitor-left",
    "move-window-to-monitor-right",
];

/// Whether `action` is on the synthetic-menu allow-list.
#[must_use]
pub fn is_allowed_niri_action(action: &str) -> bool {
    ALLOWED_NIRI_ACTIONS.contains(&action)
}

/// Umbriel equivalent of an allow-listed niri action, plus whether the
/// Umbriel action accepts a `window` argument (so it can target the
/// captured window directly instead of whatever has focus at click time).
/// `None` when Umbriel has no equivalent.
#[must_use]
pub fn umbriel_action_for(niri_action: &str) -> Option<(&'static str, bool)> {
    Some(match niri_action {
        "close-window" => ("window-close", true),
        "fullscreen-window" => ("window-toggle-fullscreen", false),
        "toggle-window-floating" => ("window-toggle-floating", true),
        "maximize-column" => ("window-toggle-maximize", false),
        "center-column" => ("column-center", false),
        "move-window-to-workspace-up" => ("window-move-to-workspace-previous", false),
        "move-window-to-workspace-down" => ("window-move-to-workspace-next", false),
        "move-window-to-monitor-left" => ("window-move-to-output-left", false),
        "move-window-to-monitor-right" => ("window-move-to-output-right", false),
        _ => return None,
    })
}

/// Whether the synthetic action `niri_action` can run on `compositor`.
#[must_use]
pub fn supports_action(compositor: Compositor, niri_action: &str) -> bool {
    if !is_allowed_niri_action(niri_action) {
        return false;
    }
    match compositor {
        Compositor::Niri => true,
        Compositor::Umbriel => umbriel_action_for(niri_action).is_some(),
        Compositor::Hyprland => crate::hyprland::commands_for(niri_action, None).is_some(),
    }
}

/// Remove synthetic leaves whose action the active compositor cannot run,
/// then drop separators left dangling at the edges or doubled up. No-op
/// when no compositor was recorded (tests, upstream niri behaviour).
pub fn retain_supported_actions(item: &mut crate::atspi::MenuItem) {
    let Some(compositor) = active() else {
        return;
    };
    retain_for(compositor, item);
}

/// [`retain_supported_actions`] with an explicit compositor (testable).
pub fn retain_for(compositor: Compositor, item: &mut crate::atspi::MenuItem) {
    item.children.retain(|child| {
        if child.service != crate::atspi::SYNTHETIC_SERVICE || !child.children.is_empty() {
            return true;
        }
        match child.path.strip_prefix("niri:") {
            Some("noop") | None => true,
            Some(action) => supports_action(compositor, action),
        }
    });
    for child in &mut item.children {
        retain_for(compositor, child);
    }
    // Tidy separators: no leading/trailing ones, no two in a row.
    let mut tidy: Vec<crate::atspi::MenuItem> = Vec::with_capacity(item.children.len());
    for child in item.children.drain(..) {
        let is_sep = child.item_type == "separator";
        if is_sep && tidy.last().map_or(true, |p| p.item_type == "separator") {
            continue;
        }
        tidy.push(child);
    }
    if tidy.last().is_some_and(|c| c.item_type == "separator") {
        tidy.pop();
    }
    item.children = tidy;
}

/// Timeout for a single compositor IPC round-trip.
const IPC_TIMEOUT: Duration = Duration::from_secs(2);

/// Upper bound on one Umbriel reply line. Replies to `action` are tiny
/// (`{"ok":null}`); the cap only stops a misbehaving peer from growing
/// the buffer without bound.
const MAX_REPLY_BYTES: usize = 64 * 1024;

/// Focus `window` (the compositor-native id) on `compositor`.
///
/// # Errors
/// When the compositor is unreachable, the id is malformed for niri, or
/// the compositor rejects the request.
pub async fn focus_window(compositor: Compositor, window: &str) -> Result<()> {
    match compositor {
        Compositor::Niri => {
            let id: u64 = window
                .parse()
                .with_context(|| format!("niri window id must be numeric, got {window:?}"))?;
            niri_action(niri_ipc::Action::FocusWindow { id }).await
        }
        Compositor::Umbriel => {
            validate_umbriel_window_id(window)?;
            umbriel_action(serde_json::json!({
                "cmd": "action",
                "action": "window-focus",
                "window": window,
            }))
            .await
        }
        Compositor::Hyprland => crate::hyprland::focus_window(window).await,
    }
}

/// Run the allow-listed synthetic action `niri_action`. `window` is the
/// captured window id, used by Umbriel actions that accept a target.
///
/// # Errors
/// When the action is not allow-listed, unsupported on `compositor`, or
/// the compositor rejects it.
pub async fn window_action(
    compositor: Compositor,
    niri_action: &str,
    window: Option<&str>,
) -> Result<()> {
    if !is_allowed_niri_action(niri_action) {
        bail!("synthetic action {niri_action:?} is not on the allow-list");
    }
    match compositor {
        Compositor::Niri => run_niri_cli_action(niri_action).await,
        Compositor::Umbriel => {
            let (action, takes_window) = umbriel_action_for(niri_action)
                .ok_or_else(|| anyhow!("{niri_action:?} has no Umbriel equivalent"))?;
            let mut req = serde_json::json!({ "cmd": "action", "action": action });
            if let (true, Some(w)) = (takes_window, window.filter(|w| !w.is_empty())) {
                validate_umbriel_window_id(w)?;
                req["window"] = serde_json::Value::String(w.to_string());
            }
            umbriel_action(req).await
        }
        Compositor::Hyprland => crate::hyprland::window_action(niri_action, window).await,
    }
}

/// Umbriel window ids are ext-foreign-toplevel identifiers: short opaque
/// tokens. Reject anything that is empty, huge, or carries control
/// characters before it goes on the wire.
fn validate_umbriel_window_id(window: &str) -> Result<()> {
    if window.is_empty() || window.len() > 128 || window.chars().any(char::is_control) {
        bail!("malformed Umbriel window id {window:?}");
    }
    Ok(())
}

async fn niri_action(action: niri_ipc::Action) -> Result<()> {
    use niri_ipc::{socket::Socket, Request};
    // niri-ipc's socket is blocking; keep it off the async workers.
    tokio::task::spawn_blocking(move || -> Result<()> {
        let mut socket = Socket::connect().context("connecting to niri IPC socket")?;
        let reply = socket
            .send(Request::Action(action))
            .context("sending action to niri")?;
        reply.map_err(|e| anyhow!("niri rejected action: {e}"))?;
        Ok(())
    })
    .await
    .context("niri IPC task panicked")?
}

/// `niri msg action <action>` — upstream's dispatch path for the
/// synthetic menu, kept because the action vocabulary is niri's CLI one.
/// The action is allow-listed by the caller and passed as a single argv
/// element (no shell).
async fn run_niri_cli_action(action: &str) -> Result<()> {
    let status = tokio::time::timeout(
        IPC_TIMEOUT,
        tokio::process::Command::new("niri")
            .args(["msg", "action", action])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status(),
    )
    .await
    .with_context(|| format!("niri msg action {action} timed out"))?
    .with_context(|| format!("spawning niri msg action {action}"))?;
    if !status.success() {
        bail!("niri msg action {action} exited {status}");
    }
    Ok(())
}

/// Send one request to Umbriel and check the `{"ok":…}` / `{"err":…}`
/// reply.
async fn umbriel_action(request: serde_json::Value) -> Result<()> {
    let path = umbriel_socket_path().ok_or_else(|| {
        anyhow!("cannot locate the Umbriel IPC socket (set UMBRIEL_SOCKET or WAYLAND_DISPLAY)")
    })?;
    tokio::time::timeout(IPC_TIMEOUT, umbriel_request_at(&path, &request))
        .await
        .context("Umbriel IPC request timed out")?
}

/// [`umbriel_action`] against an explicit socket path (testable).
pub async fn umbriel_request_at(path: &std::path::Path, request: &serde_json::Value) -> Result<()> {
    let stream = UnixStream::connect(path)
        .await
        .with_context(|| format!("connecting to Umbriel socket at {}", path.display()))?;
    let (rd, mut wr) = stream.into_split();
    let line = request.to_string() + "\n";
    wr.write_all(line.as_bytes())
        .await
        .context("writing Umbriel request")?;
    let mut rd = BufReader::new(rd).take(MAX_REPLY_BYTES as u64);
    let mut reply = String::new();
    rd.read_line(&mut reply)
        .await
        .context("reading Umbriel reply")?;
    parse_umbriel_reply(&reply)
}

/// Interpret one Umbriel reply line.
///
/// # Errors
/// When the line is not JSON, is an `{"err": …}` reply, or is neither.
pub fn parse_umbriel_reply(line: &str) -> Result<()> {
    let value: serde_json::Value = serde_json::from_str(line.trim())
        .with_context(|| format!("Umbriel reply is not JSON: {:?}", truncate(line, 200)))?;
    if value.get("ok").is_some() {
        return Ok(());
    }
    if let Some(err) = value.get("err").and_then(serde_json::Value::as_str) {
        bail!("Umbriel rejected the request: {}", truncate(err, 200));
    }
    bail!("unexpected Umbriel reply: {}", truncate(line, 200))
}

fn truncate(s: &str, max: usize) -> &str {
    match s.char_indices().nth(max) {
        Some((idx, _)) => &s[..idx],
        None => s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::atspi::{niri_leaf, synthetic_separator, synthetic_submenu};

    #[test]
    fn override_wins_over_sockets() {
        assert_eq!(
            choose(Some("niri"), false, true, false),
            Some(Compositor::Niri)
        );
        assert_eq!(
            choose(Some(" UMBRIEL "), true, false, false),
            Some(Compositor::Umbriel)
        );
        assert_eq!(
            choose(Some("hyprland"), true, false, false),
            Some(Compositor::Hyprland)
        );
    }

    #[test]
    fn detects_from_available_sockets() {
        assert_eq!(choose(None, true, false, false), Some(Compositor::Niri));
        assert_eq!(choose(None, false, true, false), Some(Compositor::Umbriel));
        assert_eq!(choose(None, false, false, true), Some(Compositor::Hyprland));
        assert_eq!(choose(Some("sway"), false, false, false), None);
        assert_eq!(choose(None, false, false, false), None);
    }

    #[test]
    fn allow_list_blocks_arbitrary_actions() {
        assert!(is_allowed_niri_action("close-window"));
        for bad in [
            "spawn",
            "spawn -- sh -c id",
            "quit",
            "",
            "close-window ",
            "--help",
        ] {
            assert!(!is_allowed_niri_action(bad), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn every_allowed_action_is_known_to_both_tables() {
        for a in ALLOWED_NIRI_ACTIONS {
            assert!(supports_action(Compositor::Niri, a));
        }
        // The one niri-only action is pruned on Umbriel.
        assert!(!supports_action(
            Compositor::Umbriel,
            "expand-column-to-available-width"
        ));
        assert_eq!(
            umbriel_action_for("close-window"),
            Some(("window-close", true))
        );
    }

    #[test]
    fn retain_prunes_unsupported_and_tidies_separators() {
        let mut menu = synthetic_submenu(
            0,
            "Window",
            vec![
                niri_leaf(0, "Close", "close-window"),
                synthetic_separator(1),
                niri_leaf(2, "Expand", "expand-column-to-available-width"),
                synthetic_separator(3),
                niri_leaf(4, "Center", "center-column"),
                synthetic_separator(5),
            ],
        );
        retain_for(Compositor::Umbriel, &mut menu);
        let labels: Vec<_> = menu.children.iter().map(|c| c.item_type.as_str()).collect();
        assert_eq!(labels, ["standard", "separator", "standard"]);

        let mut on_niri = synthetic_submenu(
            0,
            "Window",
            vec![niri_leaf(0, "Expand", "expand-column-to-available-width")],
        );
        retain_for(Compositor::Niri, &mut on_niri);
        assert_eq!(on_niri.children.len(), 1);
    }

    #[test]
    fn umbriel_reply_parsing() {
        assert!(parse_umbriel_reply("{\"ok\":null}\n").is_ok());
        let err = parse_umbriel_reply("{\"err\":\"no such window\"}").unwrap_err();
        assert!(err.to_string().contains("no such window"));
        assert!(parse_umbriel_reply("garbage").is_err());
        assert!(parse_umbriel_reply("{\"other\":1}").is_err());
    }

    #[test]
    fn umbriel_window_ids_are_validated() {
        assert!(validate_umbriel_window_id("a1b2c3").is_ok());
        assert!(validate_umbriel_window_id("").is_err());
        assert!(validate_umbriel_window_id("x\ny").is_err());
        assert!(validate_umbriel_window_id(&"x".repeat(200)).is_err());
    }

    #[tokio::test]
    async fn umbriel_request_round_trip_against_fake_socket() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("umbriel-test.sock");
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (rd, mut wr) = stream.into_split();
            let mut line = String::new();
            BufReader::new(rd).read_line(&mut line).await.unwrap();
            wr.write_all(b"{\"ok\":null}\n").await.unwrap();
            line
        });
        let req = serde_json::json!({"cmd":"action","action":"window-focus","window":"abc"});
        umbriel_request_at(&sock, &req).await.unwrap();
        let sent: serde_json::Value = serde_json::from_str(&server.await.unwrap()).unwrap();
        assert_eq!(sent, req);
    }
}
