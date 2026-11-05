//! noctalia-appmenu-bridge entry point.
//!
//! Wires the three subsystems together (post-ADR-0024 substrate):
//!
//! - focus: a `FocusSink` for the running compositor — `niri` (niri-IPC
//!   event stream) or `umbriel` (Umbriel IPC `windows` subscription),
//!   chosen by `wm::detect`.
//! - `active`: walk the focused app's AT-SPI menubar on each focus tick,
//!   emit `active.json` and, in `--stream` mode, announce it on stdout for
//!   the Noctalia v5 plugin (`publish`).
//! - `proxy`: own org.noctalia.AppMenu, expose a fixed-path active-app
//!   proxy with the focused window's metadata as D-Bus properties.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use noctalia_appmenu_bridge::focus::FocusSink;
use noctalia_appmenu_bridge::{active, atspi, config, hyprland, niri, proxy, publish, umbriel, wm};
use tracing::{info, warn};

#[derive(Parser, Debug)]
#[command(version, about = "noctalia-appmenu sidecar bridge", long_about = None)]
struct Cli {
    /// Run in foreground (no systemd-notify), log to stderr.
    #[arg(long)]
    foreground: bool,

    /// Print version JSON for telemetry / verify scripts.
    #[arg(long)]
    version_json: bool,

    /// Noctalia v5 plugin mode: announce every snapshot as a JSON line on
    /// stdout (plus a heartbeat), and exit when stdout closes. Logs go to
    /// stderr. This is how the plugin's service runs the bridge.
    #[arg(long)]
    stream: bool,

    /// Path to bridge config (TOML). Default: $XDG_CONFIG_HOME/noctalia-appmenu-bridge/config.toml
    #[arg(long)]
    config: Option<std::path::PathBuf>,

    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// AT-SPI click forward (v0.3 substrate). Calls
    /// `org.a11y.atspi.Action.DoAction(0)` on the accessible at the
    /// given (service, path) coordinates — the same pair the QML
    /// widget reads out of `active.json`'s menu tree.
    ///
    /// One-shot subcommand: spawn, call, exit. Bridge daemon stays
    /// up; this short-lived process does the click and goes away.
    /// On stale path (the app rebuilt its widget tree between
    /// snapshot and click) the subcommand exits with code 2 +
    /// stderr `MenuError::Stale {...}` — see spec 005 FR-007.
    AtspiClick {
        /// AT-SPI bus name (e.g. `:1.42` — unique connection).
        service: String,
        /// AT-SPI object path (e.g. `/org/a11y/atspi/accessible/12`).
        path: String,
        /// niri window id to focus *before* invoking `DoAction(0)`.
        /// Defaults to 0 (no pre-focus) for back-compat. When > 0, the
        /// subcommand sends `niri msg action focus-window --id <id>`
        /// via niri-ipc and sleeps `focus_settle_ms` before the click,
        /// so multi-window apps (Firefox in particular) route the
        /// action to the captured window rather than to whichever
        /// window has app-internal focus at click time. See issue #109.
        #[arg(long, default_value_t = 0)]
        winid: u64,
        /// Compositor-native id of the window to focus before the click
        /// (`focus_window` in `active.json`). Works on niri and Umbriel and
        /// takes precedence over `--winid`. Empty = no pre-focus.
        #[arg(long, default_value = "")]
        window: String,
        /// Milliseconds to sleep after the niri FocusWindow IPC reply
        /// and before issuing `Action.DoAction(0)`. The compositor
        /// needs at least one frame to swap wl_keyboard focus, and
        /// Firefox needs another tick to sync its internal
        /// "active browser" pointer. v1.0.20's 30 ms was empirically
        /// too short (Pedro confirms wrong-window routing persists);
        /// spec 015 FR-001 bumps the default to 150 ms and makes the
        /// constant configurable. 0 disables the wait (only valid
        /// when `winid == 0`).
        #[arg(long, default_value_t = 150)]
        focus_settle_ms: u64,
    },
    /// Expand-on-demand for lazily-realized menus (ADR-0034). Firefox/GTK
    /// expose a top-level menu's items over AT-SPI only once the menu is
    /// opened, so a passive walk sees zero children. This
    /// `DoAction(0)`-expands the accessible at `(service, path)`, walks
    /// the now-realized subtree, collapses it again, and prints the walked
    /// node's `children` as a JSON array to stdout. The QML widget spawns
    /// this for an empty top-level and renders the returned children in
    /// its popup. Same stale handling as `atspi-click` (exit 2 +
    /// `MenuError::Stale` on stderr).
    AtspiExpand {
        /// AT-SPI bus name of the menu node to expand.
        service: String,
        /// AT-SPI object path of the menu node to expand.
        path: String,
        /// niri window id to focus before expanding (same rationale as
        /// `atspi-click --winid`: the menu must realize in the right
        /// Firefox window). 0 = no pre-focus.
        #[arg(long, default_value_t = 0)]
        winid: u64,
        /// Compositor-native window id to focus first (see `atspi-click`).
        #[arg(long, default_value = "")]
        window: String,
        /// Settle ms after the focus IPC before expanding.
        #[arg(long, default_value_t = 150)]
        focus_settle_ms: u64,
    },
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    if cli.version_json {
        println!(
            r#"{{"name":"noctalia-appmenu-bridge","version":"{}","commit":"{}"}}"#,
            env!("CARGO_PKG_VERSION"),
            option_env!("VERGEN_GIT_SHA").unwrap_or("unknown")
        );
        return Ok(());
    }

    // CLI subcommands run in their own short-lived process — no
    // tracing setup, no daemon machinery, no D-Bus listener. Just
    // do the call and exit.
    if let Some(Cmd::AtspiClick {
        service,
        path,
        winid,
        window,
        focus_settle_ms,
    }) = cli.cmd.as_ref()
    {
        let target = click_target(window, *winid);
        return run_atspi_click(service, path, target.as_deref(), *focus_settle_ms).await;
    }

    if let Some(Cmd::AtspiExpand {
        service,
        path,
        winid,
        window,
        focus_settle_ms,
    }) = cli.cmd.as_ref()
    {
        let target = click_target(window, *winid);
        return run_atspi_expand(service, path, target.as_deref(), *focus_settle_ms).await;
    }

    init_tracing(cli.foreground);

    let compositor = wm::detect().context(
        "no supported compositor found: none of niri ($NIRI_SOCKET), Umbriel \
         ($UMBRIEL_SOCKET or $XDG_RUNTIME_DIR/umbriel-$WAYLAND_DISPLAY.sock) or Hyprland \
         ($HYPRLAND_INSTANCE_SIGNATURE) is reachable; \
         set NOCTALIA_APPMENU_COMPOSITOR=niri|umbriel|hyprland to force one",
    )?;
    wm::set_active(compositor);
    info!(compositor = compositor.as_str(), "compositor detected");

    if cli.stream {
        publish::enable_stream();
    }

    let cfg = config::Config::load(cli.config.as_deref())?;
    info!(?cfg, "starting noctalia-appmenu-bridge");

    // Flip `org.a11y.Status.IsEnabled = true` so Qt apps register
    // their accessible trees on the a11y bus. niri ships no AT
    // (Orca etc), so nobody else flips it; without this, the
    // registry stays empty and our walker finds nothing.
    if let Err(e) = atspi::enable_a11y().await {
        warn!(error = ?e, "atspi enable failed — qt apps may not expose menus");
    } else {
        info!("atspi a11y bus enabled");
    }

    // Connect to the user session bus — the bridge is a per-user daemon.
    let conn = zbus::Connection::session().await?;

    // Persistent AT-SPI connection holder (FR-006). Cloned into the
    // active loop (via proxy::run) and the IsEnabled watcher; both
    // share the same lazy-filled `zbus::Connection`. Invalidated by
    // `watch_a11y_status` when the a11y bus restarts.
    let atspi_client = atspi::AtspiClient::new();

    // Subsystems run as cancellation-safe tasks; the main task waits
    // for SIGTERM / SIGINT and signals all of them to drain. Post-
    // ADR-0024 the DBusMenu/Registrar substrate is retired; the bridge
    // walks AT-SPI directly inside the proxy task, so there is no
    // separate menu-map feed.
    let (focus_tx, focus_rx) = tokio::sync::watch::channel(None);
    let (active_tx, active_rx) = tokio::sync::watch::channel(active::ActiveSnapshot::empty());

    // The `FocusSink` trait is the abstraction door: one implementor per
    // compositor, nothing downstream depends on which one runs.
    let focus_task = match compositor {
        wm::Compositor::Niri => tokio::spawn(niri::NiriFocusSink::new().run(focus_tx, cfg.clone())),
        wm::Compositor::Umbriel => {
            tokio::spawn(umbriel::UmbrielFocusSink::new().run(focus_tx, cfg.clone()))
        }
        wm::Compositor::Hyprland => {
            tokio::spawn(hyprland::HyprlandFocusSink::new().run(focus_tx, cfg.clone()))
        }
    };
    let active_task = tokio::spawn(active::run(focus_rx, active_tx, cfg.clone()));
    let proxy_task = tokio::spawn(proxy::run(
        conn.clone(),
        atspi_client.clone(),
        active_rx,
        cfg.clone(),
    ));
    // FR-005: re-flip IsEnabled + invalidate the AT-SPI cache when
    // at-spi2-core restarts. Best-effort task — failures are logged
    // and the loop retries, never fatal.
    let a11y_watcher_task = tokio::spawn(atspi::watch_a11y_status(atspi_client));

    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;

    // niri / active / proxy failures abort the bridge.
    // SIGTERM/SIGINT are graceful shutdowns (exit 0).
    // The a11y_watcher_task is best-effort; we drop its handle into a
    // discard binding so it stays scheduled but its exit does not
    // tear the bridge down.
    let _a11y_watcher_handle = a11y_watcher_task;
    // Stream mode: periodic heartbeat so the plugin can tell a live bridge
    // from a wedged one. Also exits the process once stdout is closed.
    let _heartbeat_handle = cli.stream.then(|| tokio::spawn(publish::heartbeat()));
    tokio::select! {
        _ = sigterm.recv() => {
            warn!("SIGTERM — shutting down");
            Ok(())
        }
        _ = sigint.recv() => {
            warn!("SIGINT — shutting down");
            Ok(())
        }
        r = focus_task => {
            warn!(?r, "focus task exited unexpectedly");
            anyhow::bail!("focus task exited: {r:?}")
        }
        r = active_task => {
            warn!(?r, "active task exited unexpectedly");
            anyhow::bail!("active task exited: {r:?}")
        }
        r = proxy_task => {
            warn!(?r, "proxy task exited unexpectedly");
            anyhow::bail!("proxy task exited: {r:?}")
        }
    }
}

/// CLI `atspi-click` subcommand: forward the click to the AT-SPI
/// accessible at `(service, path)`. Delegates to [`atspi::do_action`].
///
/// FR-007 stale handling: when `do_action` surfaces
/// [`atspi::MenuError::Stale`] we signal the long-running bridge to
/// re-walk the focused app (`org.noctalia.AppMenu.Active.RefreshActive`)
/// and exit with status 2 + a `MenuError::Stale {...}` line on stderr.
/// The QML widget interprets exit-2 as "click missed, snapshot will
/// repaint shortly" and re-renders against the refreshed `active.json`.
async fn run_atspi_click(
    service: &str,
    path: &str,
    window: Option<&str>,
    focus_settle_ms: u64,
) -> Result<()> {
    // Issue #109: pre-focus the captured window so multi-window apps
    // route `Action.DoAction(0)` to the correct window. Firefox is the
    // canonical case — `DoAction` on its menu accessibles delegates to
    // whichever Firefox window has internal focus, so without this step
    // *New Tab* on window A's menu can open the tab on window B.
    //
    // Best-effort: a failed pre-focus is logged but does NOT block the
    // click (the window often still has focus).
    let compositor = wm::detect();
    let focus_result = prefocus(compositor, window, focus_settle_ms).await;
    // Synthetic window actions that accept a target (Umbriel) use the
    // captured window directly.
    atspi::set_click_window(window.unwrap_or_default());

    eprintln!(
        "[appmenu] atspi-click service={service} path={path} window={} compositor={} settle_ms={focus_settle_ms} focus={focus_result}",
        window.unwrap_or("-"),
        compositor.map_or("none", wm::Compositor::as_str),
    );

    match atspi::do_action(service, path).await {
        Ok(()) => Ok(()),
        Err(err) => {
            if let Some(stale) = err.downcast_ref::<atspi::MenuError>() {
                if let Err(e) = signal_refresh_active().await {
                    // Best-effort: even if the daemon is unreachable
                    // the exit-2 still tells the widget to give up
                    // on this click; the next focus event will
                    // naturally refresh the menu tree.
                    eprintln!(
                        "warning: RefreshActive signal failed: {e:#}; bridge will refresh on next focus event"
                    );
                }
                eprintln!("{stale}");
                std::process::exit(2);
            }
            Err(err)
        }
    }
}

/// CLI `atspi-expand` subcommand (ADR-0034): expand a lazily-realized
/// menu node, walk its now-realized subtree, collapse it, and print the
/// realized children as a JSON array to stdout for the QML widget to
/// render. Pre-focuses the captured niri window first (same reason as
/// [`run_atspi_click`]). On a stale path: signal the daemon to refresh
/// and exit 2 (the widget then falls back), mirroring the click path.
async fn run_atspi_expand(
    service: &str,
    path: &str,
    window: Option<&str>,
    focus_settle_ms: u64,
) -> Result<()> {
    let compositor = wm::detect();
    let focus_result = prefocus(compositor, window, focus_settle_ms).await;

    eprintln!(
        "[appmenu] atspi-expand service={service} path={path} window={} compositor={} settle_ms={focus_settle_ms} focus={focus_result}",
        window.unwrap_or("-"),
        compositor.map_or("none", wm::Compositor::as_str),
    );

    match atspi::expand_and_fetch(service, path).await {
        Ok(tree) => {
            // Emit the realized children (not the wrapper node) so the
            // widget can drop them straight into the popup model.
            println!("{}", serde_json::to_string(&tree.children)?);
            // ADR-0034: the realized children PERSIST in the app's AT-SPI
            // tree after collapse. Signal the daemon to re-walk so they
            // land in active.json — then subsequent clicks of this menu
            // open directly from the snapshot (children > 0) with NO
            // re-expand, so the Firefox-menu flash is first-click-per-menu
            // only, not every click. Best-effort: a failure just means the
            // next click re-expands (re-flashes), no correctness impact.
            if let Err(e) = signal_refresh_active().await {
                eprintln!(
                    "warning: post-expand RefreshActive failed: {e:#}; next click of this menu will re-expand"
                );
            }
            Ok(())
        }
        Err(err) => {
            if let Some(stale) = err.downcast_ref::<atspi::MenuError>() {
                if let Err(e) = signal_refresh_active().await {
                    eprintln!(
                        "warning: RefreshActive signal failed: {e:#}; bridge will refresh on next focus event"
                    );
                }
                eprintln!("{stale}");
                std::process::exit(2);
            }
            Err(err)
        }
    }
}

/// The window to pre-focus for a click: `--window` when given, else the
/// legacy numeric `--winid` (niri only), else none.
fn click_target(window: &str, winid: u64) -> Option<String> {
    let window = window.trim();
    if !window.is_empty() {
        Some(window.to_string())
    } else if winid > 0 {
        Some(winid.to_string())
    } else {
        None
    }
}

/// Focus `window` on `compositor` and wait `settle_ms` for the
/// compositor (one frame to move keyboard focus) and the app (one tick to
/// sync its "active window") to catch up. Returns a short status for the
/// log line: `skipped`, `ok` or `err`.
async fn prefocus(
    compositor: Option<wm::Compositor>,
    window: Option<&str>,
    settle_ms: u64,
) -> &'static str {
    let (Some(compositor), Some(window)) = (compositor, window) else {
        return "skipped";
    };
    match wm::focus_window(compositor, window).await {
        Ok(()) => {
            tokio::time::sleep(std::time::Duration::from_millis(settle_ms)).await;
            "ok"
        }
        Err(e) => {
            eprintln!(
                "warning: focusing window {window} on {} failed: {e:#}; continuing",
                compositor.as_str()
            );
            "err"
        }
    }
}

/// Send a `RefreshActive` D-Bus method call to the running bridge so
/// it re-walks the focused app's AT-SPI tree immediately. Used by the
/// short-lived `atspi-click` subprocess when it detects a stale path.
async fn signal_refresh_active() -> Result<()> {
    let conn = zbus::Connection::session()
        .await
        .context("connecting to session bus for RefreshActive")?;
    conn.call_method(
        Some("org.noctalia.AppMenu"),
        "/org/noctalia/AppMenu/Active",
        Some("org.noctalia.AppMenu.Active"),
        "RefreshActive",
        &(),
    )
    .await
    .context("calling org.noctalia.AppMenu.Active.RefreshActive")?;
    Ok(())
}

fn init_tracing(foreground: bool) {
    use tracing_subscriber::{fmt, prelude::*, EnvFilter};

    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("noctalia_appmenu_bridge=info,zbus=warn"));

    // Always stderr: in `--stream` mode stdout carries the protocol.
    let layer = if foreground {
        fmt::layer()
            .with_writer(std::io::stderr)
            .with_ansi(true)
            .boxed()
    } else {
        fmt::layer()
            .json()
            .with_writer(std::io::stderr)
            .with_ansi(false)
            .boxed()
    };

    tracing_subscriber::registry()
        .with(env_filter)
        .with(layer)
        .init();
}
