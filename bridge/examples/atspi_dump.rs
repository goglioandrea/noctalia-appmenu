//! Diagnostic: why does an app get the fallback menu?
//!
//! Prints, without changing anything:
//!   1. the accessibility switches on the session bus (`org.a11y.Status`);
//!   2. the app's Qt/GTK accessibility environment variables;
//!   3. every app registered on the AT-SPI bus, with its PID;
//!   4. the accessible tree of the app with the given PID (roles + names),
//!      so you can see whether a "menu bar" exists and where.
//!
//! Usage:
//!   cargo run --example atspi_dump -- <pid> [max_depth]
//!
//! Note: unlike the daemon and `atspi_probe`, this does NOT turn
//! accessibility on, so it shows the state the app actually sees.

use anyhow::{Context, Result};
use noctalia_appmenu_bridge::atspi;
use zbus::zvariant::{OwnedObjectPath, OwnedValue};
use zbus::Connection;

const ACCESSIBLE: &str = "org.a11y.atspi.Accessible";
const MAX_NODES: usize = 600;

async fn prop(
    conn: &Connection,
    dest: &str,
    path: &str,
    iface: &str,
    name: &str,
) -> Result<OwnedValue> {
    let reply = conn
        .call_method(
            Some(dest),
            path,
            Some("org.freedesktop.DBus.Properties"),
            "Get",
            &(iface, name),
        )
        .await?;
    Ok(reply.body().deserialize::<OwnedValue>()?)
}

async fn name_of(conn: &Connection, dest: &str, path: &str) -> String {
    match prop(conn, dest, path, ACCESSIBLE, "Name").await {
        Ok(v) => String::try_from(v).unwrap_or_default(),
        Err(_) => "<no Name>".into(),
    }
}

async fn child_count(conn: &Connection, dest: &str, path: &str) -> i32 {
    match prop(conn, dest, path, ACCESSIBLE, "ChildCount").await {
        Ok(v) => i32::try_from(v).unwrap_or(0),
        Err(_) => 0,
    }
}

async fn role_name(conn: &Connection, dest: &str, path: &str) -> String {
    match conn
        .call_method(Some(dest), path, Some(ACCESSIBLE), "GetRoleName", &())
        .await
    {
        Ok(reply) => reply.body().deserialize::<String>().unwrap_or_default(),
        Err(e) => format!("<GetRoleName failed: {e}>"),
    }
}

async fn child_at(
    conn: &Connection,
    dest: &str,
    path: &str,
    i: i32,
) -> Option<(String, OwnedObjectPath)> {
    let reply = conn
        .call_method(Some(dest), path, Some(ACCESSIBLE), "GetChildAtIndex", &(i,))
        .await
        .ok()?;
    reply.body().deserialize::<(String, OwnedObjectPath)>().ok()
}

fn env_report(pid: u32) {
    let path = format!("/proc/{pid}/environ");
    match std::fs::read(&path) {
        Ok(raw) => {
            let wanted = [
                "QT_LINUX_ACCESSIBILITY_ALWAYS_ON",
                "QT_ACCESSIBILITY",
                "QT_QPA_PLATFORM",
                "QT_QPA_PLATFORMTHEME",
                "NO_AT_BRIDGE",
                "GTK_MODULES",
            ];
            let mut found = false;
            for var in raw.split(|b| *b == 0) {
                let s = String::from_utf8_lossy(var);
                if wanted.iter().any(|w| s.starts_with(&format!("{w}="))) {
                    println!("  {s}");
                    found = true;
                }
            }
            if !found {
                println!("  (none of {wanted:?} set)");
            }
        }
        Err(e) => println!("  cannot read {path}: {e}"),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let pid: u32 = std::env::args()
        .nth(1)
        .context("usage: atspi_dump <pid> [max_depth]")?
        .parse()
        .context("pid must be a number")?;
    let max_depth: u32 = std::env::args()
        .nth(2)
        .and_then(|d| d.parse().ok())
        .unwrap_or(6);

    println!("== 1. accessibility switches (session bus org.a11y.Status)");
    let session = Connection::session().await?;
    for name in ["IsEnabled", "ScreenReaderEnabled"] {
        match prop(
            &session,
            "org.a11y.Bus",
            "/org/a11y/bus",
            "org.a11y.Status",
            name,
        )
        .await
        {
            Ok(v) => println!(
                "  {name} = {}",
                bool::try_from(v).map_or("?".into(), |b| b.to_string())
            ),
            Err(e) => println!("  {name}: unavailable ({e})"),
        }
    }

    println!("\n== 2. environment of pid {pid}");
    env_report(pid);

    println!("\n== 3. apps registered on the AT-SPI bus");
    let a11y = atspi::connect_a11y()
        .await
        .context("connecting to the a11y bus")?;
    let dbus = zbus::fdo::DBusProxy::new(&a11y).await?;
    let registry = "org.a11y.atspi.Registry";
    let root = "/org/a11y/atspi/accessible/root";
    let apps = child_count(&a11y, registry, root).await;
    let mut target: Option<(String, OwnedObjectPath)> = None;
    for i in 0..apps.clamp(0, 512) {
        let Some((svc, path)) = child_at(&a11y, registry, root, i).await else {
            continue;
        };
        let app_pid = match zbus::names::BusName::try_from(svc.as_str()) {
            Ok(bus) => dbus.get_connection_unix_process_id(bus).await.ok(),
            Err(_) => None,
        };
        let name = name_of(&a11y, &svc, path.as_str()).await;
        let mark = if app_pid == Some(pid) {
            "  <== target"
        } else {
            ""
        };
        println!(
            "  {svc:<10} pid={:<8} {name:?}{mark}",
            app_pid.map_or("?".to_string(), |p| p.to_string())
        );
        if app_pid == Some(pid) {
            target = Some((svc, path));
        }
    }
    if apps == 0 {
        println!("  (registry is empty: no app exposes accessibility right now)");
    }

    let Some((svc, path)) = target else {
        println!(
            "\n== 4. pid {pid} is NOT on the AT-SPI bus.\n  \
             The app did not enable accessibility. Restart it after the bridge is running, or\n  \
             set QT_LINUX_ACCESSIBILITY_ALWAYS_ON=1 in your session environment."
        );
        return Ok(());
    };

    println!("\n== 4. accessible tree of pid {pid} (depth <= {max_depth}, roles + names)");
    let mut stack: Vec<(String, OwnedObjectPath, u32)> = vec![(svc, path, 0)];
    let mut printed = 0usize;
    let mut menubars = 0usize;
    while let Some((s, p, depth)) = stack.pop() {
        if printed >= MAX_NODES {
            println!("  … stopped after {MAX_NODES} nodes");
            break;
        }
        let role = role_name(&a11y, &s, p.as_str()).await;
        let name = name_of(&a11y, &s, p.as_str()).await;
        let count = child_count(&a11y, &s, p.as_str()).await;
        if role == "menu bar" {
            menubars += 1;
        }
        println!(
            "  {}{role} {name:?} [{count} children]{}",
            "  ".repeat(depth as usize),
            if role == "menu bar" {
                "   <== MENU BAR"
            } else {
                ""
            }
        );
        printed += 1;
        if depth < max_depth {
            // Push in reverse so children print in order.
            for i in (0..count.clamp(0, 200)).rev() {
                if let Some((cs, cp)) = child_at(&a11y, &s, p.as_str(), i).await {
                    stack.push((cs, cp, depth + 1));
                }
            }
        }
    }
    println!("\n  menu bars found: {menubars}");
    Ok(())
}
