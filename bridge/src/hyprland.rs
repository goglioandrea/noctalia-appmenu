//! Hyprland support: focus tracking, window focus and window actions.
//!
//! Two Unix sockets under `$XDG_RUNTIME_DIR/hypr/$HYPRLAND_INSTANCE_SIGNATURE/`
//! (older releases used `/tmp/hypr/<sig>/`):
//!
//! - `.socket.sock` — one request per connection: write the command, read
//!   the reply until EOF. `j/<cmd>` asks for JSON.
//! - `.socket2.sock` — event stream, one `EVENT>>DATA` line per event.
//!
//! Focus comes from `activewindowv2>>ADDR` (hex address, no `0x`; empty when
//! focus moves to a layer surface such as our own panel), `closewindow>>ADDR`
//! and `windowtitlev2>>ADDR,TITLE`. The window's pid/class/title are looked up
//! with `j/clients`.
//!
//! ## Two dispatch syntaxes
//!
//! Hyprland 0.57 moved to a Lua-only config, and `dispatch` over IPC became
//! shorthand for `hl.dispatch(<arg>)`, e.g.
//! `dispatch hl.dsp.focus({ window = "address:0x…" })`. Up to 0.56 (hyprlang
//! configs) it took the classic form, `dispatch focuswindow address:0x…`.
//! [`dispatch`] sends the Lua form first and falls back to the classic one,
//! so both generations work without version sniffing.
//!
//! Window ids travel as Hyprland addresses (`0x…`); [`parse_address`]
//! validates every one before it is interpolated into a command.

use crate::config::Config;
use crate::focus::{FocusEvent, FocusOp, FocusRunFuture, FocusSink};
use crate::niri::{next_backoff, BACKOFF_FLOOR, BACKOFF_MAX};
use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::watch;
use tracing::{debug, info, warn};

/// Hyprland's per-instance signature variable.
pub const SIGNATURE_ENV: &str = "HYPRLAND_INSTANCE_SIGNATURE";

const IPC_TIMEOUT: Duration = Duration::from_secs(2);
/// `j/clients` grows with the window count; cap a misbehaving reply.
const MAX_REPLY_BYTES: u64 = 4 * 1024 * 1024;

/// The instance directory holding both sockets, if Hyprland is running.
#[must_use]
pub fn instance_dir() -> Option<PathBuf> {
    let sig = std::env::var(SIGNATURE_ENV)
        .ok()
        .filter(|s| !s.is_empty())?;
    // The signature becomes a path component: refuse anything that could
    // walk out of the hypr directory.
    if sig.contains('/') || sig == "." || sig == ".." {
        return None;
    }
    let mut candidates = Vec::new();
    if let Some(rt) = std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
        candidates.push(PathBuf::from(rt).join("hypr").join(&sig));
    }
    candidates.push(PathBuf::from("/tmp/hypr").join(&sig));
    candidates
        .into_iter()
        .find(|d| d.join(".socket.sock").exists())
}

/// Command socket path.
#[must_use]
pub fn command_socket() -> Option<PathBuf> {
    instance_dir().map(|d| d.join(".socket.sock"))
}

/// Event socket path.
#[must_use]
pub fn event_socket() -> Option<PathBuf> {
    instance_dir().map(|d| d.join(".socket2.sock"))
}

/// Normalise a Hyprland window address to `0x<lowercase hex>`, or `None`
/// when it is not a plain hex address. Accepts the event form (`55d0…`)
/// and the JSON form (`0x55d0…`).
#[must_use]
pub fn parse_address(raw: &str) -> Option<String> {
    let s = raw.trim();
    let hex = s.strip_prefix("0x").unwrap_or(s);
    if hex.is_empty() || hex.len() > 16 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some(format!("0x{}", hex.to_ascii_lowercase()))
}

/// The `u64` handle the rest of the bridge keys on.
#[must_use]
pub fn address_handle(address: &str) -> u64 {
    u64::from_str_radix(address.trim_start_matches("0x"), 16).unwrap_or(0)
}

/// Send one command on the command socket and return the reply.
///
/// # Errors
/// When the socket is unreachable or the exchange times out.
pub async fn request(command: &str) -> Result<String> {
    let path = command_socket().ok_or_else(|| {
        anyhow!("cannot locate the Hyprland socket (is HYPRLAND_INSTANCE_SIGNATURE set?)")
    })?;
    tokio::time::timeout(IPC_TIMEOUT, request_at(&path, command))
        .await
        .context("Hyprland IPC request timed out")?
}

/// [`request`] against an explicit socket path (testable).
///
/// # Errors
/// On connection, write or read failure.
pub async fn request_at(path: &Path, command: &str) -> Result<String> {
    let mut stream = UnixStream::connect(path)
        .await
        .with_context(|| format!("connecting to Hyprland socket at {}", path.display()))?;
    stream
        .write_all(command.as_bytes())
        .await
        .context("writing Hyprland request")?;
    let mut reply = String::new();
    stream
        .take(MAX_REPLY_BYTES)
        .read_to_string(&mut reply)
        .await
        .context("reading Hyprland reply")?;
    Ok(reply)
}

/// Whether a `dispatch` reply means success.
#[must_use]
pub fn dispatch_ok(reply: &str) -> bool {
    reply.trim_start().starts_with("ok")
}

/// Dispatch `lua` (Hyprland ≥ 0.57) or, if that is rejected, `classic`
/// (≤ 0.56).
///
/// # Errors
/// When both forms are rejected or the socket is unreachable.
pub async fn dispatch(lua: &str, classic: &str) -> Result<()> {
    let first = request(&format!("dispatch {lua}")).await?;
    if dispatch_ok(&first) {
        return Ok(());
    }
    let second = request(&format!("dispatch {classic}")).await?;
    if dispatch_ok(&second) {
        return Ok(());
    }
    bail!(
        "Hyprland rejected the dispatch: lua form: {:?}; classic form: {:?}",
        truncate(first.trim(), 200),
        truncate(second.trim(), 200)
    )
}

/// Focus the window at `window` (a Hyprland address).
///
/// # Errors
/// When the address is malformed or Hyprland rejects the request.
pub async fn focus_window(window: &str) -> Result<()> {
    let addr = parse_address(window)
        .ok_or_else(|| anyhow!("malformed Hyprland window address {window:?}"))?;
    dispatch(
        &format!("hl.dsp.focus({{ window = \"address:{addr}\" }})"),
        &format!("focuswindow address:{addr}"),
    )
    .await
}

/// Lua and classic commands for an allow-listed niri action, targeting
/// `addr` where the dispatcher accepts a window. `None` when Hyprland has
/// no equivalent. Pure, for tests.
#[must_use]
pub fn commands_for(niri_action: &str, addr: Option<&str>) -> Option<(String, String)> {
    let win_lua = addr.map_or(String::new(), |a| format!(" window = \"address:{a}\" "));
    let win_classic = addr.map_or(String::new(), |a| format!(" address:{a}"));
    Some(match niri_action {
        "close-window" => (
            format!("hl.dsp.window.close({{{win_lua}}})"),
            if addr.is_some() {
                format!("closewindow{win_classic}")
            } else {
                "killactive".to_string()
            },
        ),
        "fullscreen-window" => (
            "hl.dsp.window.fullscreen({ mode = \"fullscreen\", action = \"toggle\" })".to_string(),
            "fullscreen 0".to_string(),
        ),
        "toggle-window-floating" => (
            format!("hl.dsp.window.float({{{win_lua}}})"),
            format!("togglefloating{win_classic}"),
        ),
        "maximize-column" => (
            "hl.dsp.window.fullscreen({ mode = \"maximized\", action = \"toggle\" })".to_string(),
            "fullscreen 1".to_string(),
        ),
        "center-column" => (
            format!("hl.dsp.window.center({{{win_lua}}})"),
            "centerwindow".to_string(),
        ),
        "move-window-to-workspace-up" => (
            "hl.dsp.window.move({ workspace = \"r-1\" })".to_string(),
            "movetoworkspace r-1".to_string(),
        ),
        "move-window-to-workspace-down" => (
            "hl.dsp.window.move({ workspace = \"r+1\" })".to_string(),
            "movetoworkspace r+1".to_string(),
        ),
        "move-window-to-monitor-left" => (
            "hl.dsp.window.move({ monitor = \"l\" })".to_string(),
            "movewindow mon:l".to_string(),
        ),
        "move-window-to-monitor-right" => (
            "hl.dsp.window.move({ monitor = \"r\" })".to_string(),
            "movewindow mon:r".to_string(),
        ),
        _ => return None,
    })
}

/// Run an allow-listed synthetic action on Hyprland.
///
/// # Errors
/// When the action has no Hyprland equivalent, the address is malformed,
/// or Hyprland rejects it.
pub async fn window_action(niri_action: &str, window: Option<&str>) -> Result<()> {
    let addr = match window.filter(|w| !w.is_empty()) {
        Some(w) => Some(
            parse_address(w).ok_or_else(|| anyhow!("malformed Hyprland window address {w:?}"))?,
        ),
        None => None,
    };
    let (lua, classic) = commands_for(niri_action, addr.as_deref())
        .ok_or_else(|| anyhow!("{niri_action:?} has no Hyprland equivalent"))?;
    dispatch(&lua, &classic).await
}

/// Lua and classic `send_shortcut` commands for `sc`. Pure, for tests.
#[must_use]
pub fn shortcut_commands(sc: &crate::browser::Shortcut, addr: Option<&str>) -> (String, String) {
    let mods = sc
        .mods
        .iter()
        .map(|m| m.hyprland())
        .collect::<Vec<_>>()
        .join(" ");
    let win_lua = addr.map_or(String::new(), |a| format!(", window = \"address:{a}\""));
    let win_classic = addr.map_or(String::new(), |a| format!(", address:{a}"));
    (
        format!(
            "hl.dsp.send_shortcut({{ mods = \"{mods}\", key = \"{}\"{win_lua} }})",
            sc.key
        ),
        format!("sendshortcut {mods}, {}{win_classic}", sc.key),
    )
}

/// Send an allow-listed browser shortcut to `window`.
///
/// # Errors
/// When the address is malformed or Hyprland rejects the dispatch.
pub async fn send_shortcut(sc: &crate::browser::Shortcut, window: Option<&str>) -> Result<()> {
    let addr = match window.filter(|w| !w.is_empty()) {
        Some(w) => Some(
            parse_address(w).ok_or_else(|| anyhow!("malformed Hyprland window address {w:?}"))?,
        ),
        None => None,
    };
    let (lua, classic) = shortcut_commands(sc, addr.as_deref());
    dispatch(&lua, &classic).await
}

/// The subset of a `j/clients` entry the bridge needs.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Client {
    /// `0x…` address.
    pub address: String,
    /// Owning process id (`-1` when unknown).
    pub pid: i64,
    /// Wayland app-id / X11 class.
    pub class: String,
    /// Window title.
    pub title: String,
}

/// One parsed event-socket line, as far as focus is concerned.
#[derive(Debug, PartialEq, Eq)]
pub enum Event {
    /// Focus moved to this window (normalised address).
    Active(String),
    /// Focus moved off every window (a layer surface, nothing).
    ActiveNone,
    /// This window closed.
    Closed(String),
    /// This window's title changed.
    Title(String, String),
    /// Anything else.
    Other,
}

/// Parse one `EVENT>>DATA` line.
#[must_use]
pub fn parse_event(line: &str) -> Event {
    let line = line.trim_end_matches(['\r', '\n']);
    let Some((event, data)) = line.split_once(">>") else {
        return Event::Other;
    };
    match event {
        "activewindowv2" => match parse_address(data) {
            Some(a) => Event::Active(a),
            None if data.trim().is_empty() => Event::ActiveNone,
            None => Event::Other,
        },
        "closewindow" => parse_address(data).map_or(Event::Other, Event::Closed),
        "windowtitlev2" => match data.split_once(',') {
            Some((a, title)) => {
                parse_address(a).map_or(Event::Other, |a| Event::Title(a, title.to_string()))
            }
            None => Event::Other,
        },
        _ => Event::Other,
    }
}

/// Build the focus decision for `client`. Pure.
#[must_use]
pub fn focus_op_for(client: &Client) -> FocusOp {
    let Some(address) = parse_address(&client.address) else {
        return FocusOp::Defocus;
    };
    match u32::try_from(client.pid) {
        Ok(pid) if pid > 0 => FocusOp::Emit(FocusEvent {
            winid: address_handle(&address),
            pid,
            app_id: client.class.clone(),
            title: client.title.clone(),
            window: address,
        }),
        // Unknown owner: we cannot match its accessible tree, and keeping
        // the previous app's menu would show the wrong one.
        _ => FocusOp::Defocus,
    }
}

async fn lookup_client(address: &str) -> Result<Option<Client>> {
    let reply = request("j/clients").await?;
    let clients: Vec<Client> =
        serde_json::from_str(&reply).context("parsing Hyprland j/clients reply")?;
    Ok(clients
        .into_iter()
        .find(|c| parse_address(&c.address).as_deref() == Some(address)))
}

async fn active_client() -> Result<Option<Client>> {
    let reply = request("j/activewindow").await?;
    // `{}` when nothing is focused.
    let client: Client = serde_json::from_str(&reply).unwrap_or_default();
    Ok(parse_address(&client.address).map(|_| client))
}

fn publish(tx: &watch::Sender<Option<FocusEvent>>, op: FocusOp, current: &mut Option<String>) {
    match op {
        FocusOp::Emit(evt) => {
            *current = Some(evt.window.clone());
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
            *current = None;
            tx.send_if_modified(|cur| cur.take().is_some());
        }
        FocusOp::NoChange | FocusOp::UnknownWindow(_) => {}
    }
}

/// The Hyprland [`FocusSink`].
#[derive(Debug, Default, Clone, Copy)]
pub struct HyprlandFocusSink;

impl HyprlandFocusSink {
    /// Construct the sink.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl FocusSink for HyprlandFocusSink {
    fn run(self, tx: watch::Sender<Option<FocusEvent>>, cfg: Config) -> FocusRunFuture {
        Box::pin(run(tx, cfg))
    }
}

/// Reconnect loop (same backoff contract as the niri sink).
///
/// # Errors
/// Never in practice; returns `Ok(())` once `tx` is closed.
pub async fn run(tx: watch::Sender<Option<FocusEvent>>, _cfg: Config) -> Result<()> {
    let mut backoff = BACKOFF_FLOOR;
    loop {
        if tx.is_closed() {
            return Ok(());
        }
        let started = std::time::Instant::now();
        let result = match event_socket() {
            Some(path) => run_once_at(&path, &tx).await,
            None => Err(anyhow!(
                "cannot locate the Hyprland event socket (is HYPRLAND_INSTANCE_SIGNATURE set?)"
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
                    "hyprland IPC session ended; reconnecting after backoff"
                );
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(BACKOFF_MAX);
            }
        }
    }
}

async fn run_once_at(path: &Path, tx: &watch::Sender<Option<FocusEvent>>) -> Result<()> {
    let stream = UnixStream::connect(path)
        .await
        .with_context(|| format!("connecting to Hyprland event socket at {}", path.display()))?;
    info!("subscribed to hyprland events");
    let mut current: Option<String> = None;
    // Seed with whatever is focused now; the stream only reports changes.
    match active_client().await {
        Ok(Some(c)) => publish(tx, focus_op_for(&c), &mut current),
        Ok(None) => {}
        Err(e) => debug!(error = ?e, "hyprland j/activewindow failed at startup"),
    }
    let mut rd = BufReader::new(stream);
    drive(&mut rd, tx, &mut current).await
}

async fn drive<R>(
    rd: &mut R,
    tx: &watch::Sender<Option<FocusEvent>>,
    current: &mut Option<String>,
) -> Result<()>
where
    R: AsyncBufRead + Unpin,
{
    let mut line = String::new();
    loop {
        if tx.is_closed() {
            return Ok(());
        }
        line.clear();
        let n = rd
            .read_line(&mut line)
            .await
            .context("reading Hyprland event stream")?;
        if n == 0 {
            return Err(anyhow!("Hyprland closed the event stream"));
        }
        match parse_event(&line) {
            Event::Active(addr) => match lookup_client(&addr).await {
                Ok(Some(client)) => publish(tx, focus_op_for(&client), current),
                Ok(None) => debug!(%addr, "focused hyprland window not in j/clients yet"),
                Err(e) => debug!(error = ?e, "hyprland j/clients lookup failed"),
            },
            // A layer surface (the AppMenu panel, a launcher) took focus:
            // keep the previous window, like the Umbriel sink does.
            Event::ActiveNone => {}
            Event::Closed(addr) if current.as_deref() == Some(addr.as_str()) => {
                publish(tx, FocusOp::Defocus, current);
            }
            Event::Title(addr, title) if current.as_deref() == Some(addr.as_str()) => {
                let updated = tx.borrow().clone().map(|mut evt| {
                    evt.title = title;
                    evt
                });
                if let Some(evt) = updated {
                    publish(tx, FocusOp::Emit(evt), current);
                }
            }
            _ => {}
        }
    }
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

    #[test]
    fn addresses_are_normalised_and_validated() {
        assert_eq!(
            parse_address("55d0c6e8a8b0").as_deref(),
            Some("0x55d0c6e8a8b0")
        );
        assert_eq!(parse_address("0x55D0C6").as_deref(), Some("0x55d0c6"));
        for bad in [
            "",
            "0x",
            "zz",
            "0x12 ; dispatch exec rm",
            "address:0x1",
            "0x123456789abcdef01",
        ] {
            assert_eq!(parse_address(bad), None, "{bad:?}");
        }
        assert_eq!(address_handle("0xff"), 255);
    }

    #[test]
    fn events_parse() {
        assert_eq!(
            parse_event("activewindowv2>>55d0c6\n"),
            Event::Active("0x55d0c6".into())
        );
        assert_eq!(parse_event("activewindowv2>>\n"), Event::ActiveNone);
        assert_eq!(
            parse_event("closewindow>>abc"),
            Event::Closed("0xabc".into())
        );
        assert_eq!(
            parse_event("windowtitlev2>>abc,~ : fish, with comma"),
            Event::Title("0xabc".into(), "~ : fish, with comma".into())
        );
        assert_eq!(parse_event("workspace>>2"), Event::Other);
        assert_eq!(parse_event("garbage"), Event::Other);
    }

    #[test]
    fn clients_become_focus_events() {
        let c = Client {
            address: "0x1f".into(),
            pid: 4242,
            class: "org.kde.konsole".into(),
            title: "Konsole".into(),
        };
        let FocusOp::Emit(e) = focus_op_for(&c) else {
            panic!("expected emit");
        };
        assert_eq!((e.pid, e.window.as_str(), e.winid), (4242, "0x1f", 31));
        assert_eq!(
            focus_op_for(&Client {
                pid: -1,
                ..c.clone()
            }),
            FocusOp::Defocus
        );
    }

    #[test]
    fn every_allowed_action_but_expand_maps() {
        for a in [
            "close-window",
            "fullscreen-window",
            "toggle-window-floating",
            "maximize-column",
            "center-column",
            "move-window-to-workspace-up",
            "move-window-to-workspace-down",
            "move-window-to-monitor-left",
            "move-window-to-monitor-right",
        ] {
            assert!(commands_for(a, Some("0x1")).is_some(), "{a}");
        }
        assert!(commands_for("expand-column-to-available-width", None).is_none());
        let (lua, classic) = commands_for("close-window", Some("0xab")).unwrap();
        assert_eq!(lua, "hl.dsp.window.close({ window = \"address:0xab\" })");
        assert_eq!(classic, "closewindow address:0xab");
        assert_eq!(commands_for("close-window", None).unwrap().1, "killactive");
    }

    #[test]
    fn shortcut_commands_both_syntaxes() {
        let sc = crate::browser::shortcut("new-private-window").unwrap();
        let (lua, classic) = shortcut_commands(sc, Some("0xab"));
        assert_eq!(
            lua,
            "hl.dsp.send_shortcut({ mods = \"CTRL SHIFT\", key = \"n\", window = \"address:0xab\" })"
        );
        assert_eq!(classic, "sendshortcut CTRL SHIFT, n, address:0xab");
    }

    #[test]
    fn dispatch_reply_detection() {
        assert!(dispatch_ok("ok"));
        assert!(dispatch_ok("ok\n"));
        assert!(!dispatch_ok("Invalid dispatcher"));
        assert!(!dispatch_ok(
            "[string \"...\"]:1: syntax error\n\n → Note: dispatch in lua …"
        ));
    }

    #[tokio::test]
    async fn request_round_trip_against_fake_socket() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join(".socket.sock");
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 256];
            let n = stream.read(&mut buf).await.unwrap();
            stream.write_all(b"ok").await.unwrap();
            String::from_utf8_lossy(&buf[..n]).to_string()
        });
        let reply = request_at(&sock, "dispatch focuswindow address:0x1")
            .await
            .unwrap();
        assert_eq!(reply, "ok");
        assert_eq!(server.await.unwrap(), "dispatch focuswindow address:0x1");
    }

    #[tokio::test]
    async fn drive_follows_focus_title_and_close() {
        // Only events that need no j/clients lookup are fed here.
        let (tx, mut rx) = watch::channel(None);
        let mut current = None;
        publish(
            &tx,
            focus_op_for(&Client {
                address: "0xa".into(),
                pid: 7,
                class: "kate".into(),
                title: "old".into(),
            }),
            &mut current,
        );
        rx.borrow_and_update();
        let stream = "windowtitlev2>>a,new\nactivewindowv2>>\nclosewindow>>a\n";
        let mut rd = BufReader::new(stream.as_bytes());
        let _ = drive(&mut rd, &tx, &mut current).await;
        assert!(rx.borrow().is_none(), "closed window defocuses");
        assert!(current.is_none());
    }
}
