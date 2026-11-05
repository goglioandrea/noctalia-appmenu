//! Template menus for Chromium-family browsers.
//!
//! Chrome, Chromium, Vivaldi, Brave, Edge and Opera have no menubar on
//! Linux: their "⋮" menu is a button whose items exist on AT-SPI only while
//! it is open, so the bridge has nothing to read. Instead, for these apps
//! only, the fallback menu gains File / Edit / View / History / Bookmarks /
//! Tools submenus whose items send Chromium's standard keyboard shortcuts.
//!
//! ## Why this is not upstream's rejected "lying menu"
//!
//! Upstream removed a wtype-driven Edit menu shown for *every* app (PR #44,
//! ADR-0031): terminals, vim-mode editors and custom keymaps bind those keys
//! differently, so the menu silently did the wrong thing. Here the menu is
//! shown only for apps identified as Chromium browsers, whose default
//! shortcuts are fixed and documented. A user who rebinds them (Vivaldi
//! allows it) gets the rebound behaviour — documented in the README.
//!
//! ## Security
//!
//! A menu item's path is `key:<id>` where `<id>` names an entry of the
//! closed [`SHORTCUTS`] table. The keys to press are never read from the
//! menu, `active.json` or the command line, so a tampered path can at most
//! pick another harmless browser shortcut.

use crate::atspi::{synthetic_separator, synthetic_submenu, MenuItem, SYNTHETIC_SERVICE};

/// Modifier keys, in the order they are pressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mod {
    /// Control.
    Ctrl,
    /// Shift.
    Shift,
    /// Alt.
    Alt,
}

impl Mod {
    /// `wtype -M` name.
    #[must_use]
    pub fn wtype(self) -> &'static str {
        match self {
            Self::Ctrl => "ctrl",
            Self::Shift => "shift",
            Self::Alt => "alt",
        }
    }

    /// Hyprland modifier name.
    #[must_use]
    pub fn hyprland(self) -> &'static str {
        match self {
            Self::Ctrl => "CTRL",
            Self::Shift => "SHIFT",
            Self::Alt => "ALT",
        }
    }
}

/// One allow-listed shortcut: modifiers + an xkb keysym name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shortcut {
    /// Stable id used in `key:<id>` menu paths.
    pub id: &'static str,
    /// Menu label.
    pub label: &'static str,
    /// Modifiers held while `key` is pressed.
    pub mods: &'static [Mod],
    /// xkb keysym name (`t`, `Tab`, `F5`, `equal`, …).
    pub key: &'static str,
}

const C: &[Mod] = &[Mod::Ctrl];
const CS: &[Mod] = &[Mod::Ctrl, Mod::Shift];
const A: &[Mod] = &[Mod::Alt];
const S: &[Mod] = &[Mod::Shift];
const NONE: &[Mod] = &[];

macro_rules! sc {
    ($id:literal, $label:literal, $mods:expr, $key:literal) => {
        Shortcut {
            id: $id,
            label: $label,
            mods: $mods,
            key: $key,
        }
    };
}

/// The closed table of shortcuts the browser menus may send.
pub const SHORTCUTS: &[Shortcut] = &[
    // File
    sc!("new-tab", "New Tab", C, "t"),
    sc!("new-window", "New Window", C, "n"),
    sc!(
        "new-private-window",
        "New Incognito/Private Window",
        CS,
        "n"
    ),
    sc!("reopen-tab", "Reopen Closed Tab", CS, "t"),
    sc!("open-file", "Open File…", C, "o"),
    sc!("save-page", "Save Page As…", C, "s"),
    sc!("print", "Print…", C, "p"),
    sc!("close-tab", "Close Tab", C, "w"),
    sc!("close-window", "Close Window", CS, "w"),
    // Edit
    sc!("undo", "Undo", C, "z"),
    sc!("redo", "Redo", CS, "z"),
    sc!("cut", "Cut", C, "x"),
    sc!("copy", "Copy", C, "c"),
    sc!("paste", "Paste", C, "v"),
    sc!("paste-plain", "Paste as Plain Text", CS, "v"),
    sc!("select-all", "Select All", C, "a"),
    sc!("find", "Find…", C, "f"),
    // View
    sc!("reload", "Reload", C, "r"),
    sc!("hard-reload", "Reload Ignoring Cache", CS, "r"),
    sc!("zoom-in", "Zoom In", C, "equal"),
    sc!("zoom-out", "Zoom Out", C, "minus"),
    sc!("zoom-reset", "Actual Size", C, "0"),
    sc!("fullscreen", "Full Screen", NONE, "F11"),
    sc!("devtools", "Developer Tools", CS, "i"),
    sc!("view-source", "View Page Source", C, "u"),
    // History / navigation
    sc!("back", "Back", A, "Left"),
    sc!("forward", "Forward", A, "Right"),
    sc!("home", "Home Page", A, "Home"),
    sc!("history", "Show Full History", C, "h"),
    // Tabs
    sc!("next-tab", "Next Tab", C, "Tab"),
    sc!("prev-tab", "Previous Tab", CS, "Tab"),
    // Bookmarks
    sc!("bookmark-page", "Bookmark This Page", C, "d"),
    sc!("bookmark-manager", "Bookmark Manager", CS, "o"),
    sc!("bookmarks-bar", "Show Bookmarks Bar", CS, "b"),
    // Tools
    sc!("downloads", "Downloads", C, "j"),
    sc!("task-manager", "Task Manager", S, "Escape"),
    sc!("clear-data", "Clear Browsing Data…", CS, "Delete"),
];

/// Look up an allow-listed shortcut by id.
#[must_use]
pub fn shortcut(id: &str) -> Option<&'static Shortcut> {
    SHORTCUTS.iter().find(|s| s.id == id)
}

/// What kind of Chromium app an app id names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A browser window.
    Browser,
    /// An installed web app ("Install as app" / PWA): a single site in its
    /// own window, where tabs and bookmarks make no sense.
    WebApp,
}

const BROWSER_IDS: &[&str] = &[
    "google-chrome",
    "google-chrome-beta",
    "google-chrome-unstable",
    "chromium",
    "chromium-browser",
    "org.chromium.chromium",
    "com.google.chrome",
    "vivaldi",
    "vivaldi-stable",
    "vivaldi-snapshot",
    "com.vivaldi.vivaldi",
    "brave-browser",
    "brave-browser-beta",
    "brave-browser-nightly",
    "com.brave.browser",
    "microsoft-edge",
    "microsoft-edge-beta",
    "microsoft-edge-dev",
    "com.microsoft.edge",
    "opera",
    "thorium-browser",
];

/// Web-app app ids look like `<browser>-<32 letters a-p>-<profile>`, e.g.
/// `vivaldi-fmpnliohjhemenmnlpbfagaolkdacoja-Default`.
const WEB_APP_PREFIXES: &[&str] = &["chrome", "chromium", "vivaldi", "brave", "msedge", "opera"];

/// Classify `app_id`. Case-insensitive; `None` for anything that is not a
/// Chromium-family browser or web app.
#[must_use]
pub fn classify(app_id: &str) -> Option<Kind> {
    let id = app_id.trim().to_ascii_lowercase();
    if BROWSER_IDS.contains(&id.as_str()) {
        return Some(Kind::Browser);
    }
    let mut parts = id.splitn(3, '-');
    let (Some(prefix), Some(ext)) = (parts.next(), parts.next()) else {
        return None;
    };
    let is_extension_id = ext.len() == 32 && ext.chars().all(|c| ('a'..='p').contains(&c));
    (WEB_APP_PREFIXES.contains(&prefix) && is_extension_id).then_some(Kind::WebApp)
}

fn leaf(id: i32, sc_id: &str) -> MenuItem {
    let s = shortcut(sc_id).unwrap_or_else(|| panic!("unknown browser shortcut {sc_id}"));
    MenuItem {
        id,
        label: s.label.to_string(),
        item_type: "standard".to_string(),
        enabled: true,
        visible: true,
        service: SYNTHETIC_SERVICE.to_string(),
        path: format!("key:{}", s.id),
        ..Default::default()
    }
}

/// Build a submenu from shortcut ids; `"-"` is a separator.
fn submenu(id: i32, label: &str, ids: &[&str]) -> MenuItem {
    let children = ids
        .iter()
        .enumerate()
        .map(|(i, sc_id)| {
            let i = i32::try_from(i).unwrap_or(i32::MAX);
            if *sc_id == "-" {
                synthetic_separator(i)
            } else {
                leaf(i, sc_id)
            }
        })
        .collect();
    synthetic_submenu(id, label, children)
}

/// The browser submenus for `kind`, in menubar order.
#[must_use]
pub fn menus(kind: Kind) -> Vec<MenuItem> {
    let edit = &[
        "undo",
        "redo",
        "-",
        "cut",
        "copy",
        "paste",
        "paste-plain",
        "select-all",
        "-",
        "find",
    ][..];
    match kind {
        Kind::Browser => vec![
            submenu(
                0,
                "File",
                &[
                    "new-tab",
                    "new-window",
                    "new-private-window",
                    "reopen-tab",
                    "-",
                    "open-file",
                    "save-page",
                    "print",
                    "-",
                    "close-tab",
                    "close-window",
                ],
            ),
            submenu(1, "Edit", edit),
            submenu(
                2,
                "View",
                &[
                    "reload",
                    "hard-reload",
                    "-",
                    "zoom-in",
                    "zoom-out",
                    "zoom-reset",
                    "-",
                    "fullscreen",
                    "-",
                    "devtools",
                    "view-source",
                ],
            ),
            submenu(
                3,
                "History",
                &[
                    "back", "forward", "home", "-", "next-tab", "prev-tab", "-", "history",
                ],
            ),
            submenu(
                4,
                "Bookmarks",
                &["bookmark-page", "bookmarks-bar", "bookmark-manager"],
            ),
            submenu(5, "Tools", &["downloads", "task-manager", "clear-data"]),
        ],
        Kind::WebApp => vec![
            submenu(0, "Edit", edit),
            submenu(
                1,
                "View",
                &[
                    "reload",
                    "-",
                    "zoom-in",
                    "zoom-out",
                    "zoom-reset",
                    "-",
                    "fullscreen",
                ],
            ),
            submenu(2, "Go", &["back", "forward"]),
        ],
    }
}

/// Insert the browser submenus into a fallback menu, after the application
/// submenu and before the Window submenu. No-op for non-browsers.
pub fn augment(app_id: &str, menu: &mut MenuItem) {
    let Some(kind) = classify(app_id) else {
        return;
    };
    let insert_at = usize::from(!menu.children.is_empty());
    for (offset, sub) in menus(kind).into_iter().enumerate() {
        menu.children.insert(insert_at + offset, sub);
    }
    for (i, child) in menu.children.iter_mut().enumerate() {
        child.id = i32::try_from(i).unwrap_or(i32::MAX);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_browsers_and_web_apps() {
        for b in [
            "google-chrome",
            "Vivaldi-stable",
            "brave-browser",
            "chromium",
            "microsoft-edge",
        ] {
            assert_eq!(classify(b), Some(Kind::Browser), "{b}");
        }
        assert_eq!(
            classify("vivaldi-fmpnliohjhemenmnlpbfagaolkdacoja-Default"),
            Some(Kind::WebApp)
        );
        assert_eq!(
            classify("chrome-abcdefghijklmnopabcdefghijklmnop-Profile_1"),
            Some(Kind::WebApp)
        );
        for other in [
            "org.kde.konsole",
            "firefox",
            "chrome-short-Default",
            "vivaldi-zzzz",
            "",
        ] {
            assert_eq!(classify(other), None, "{other}");
        }
    }

    #[test]
    fn every_menu_item_names_a_known_shortcut() {
        fn walk(item: &MenuItem) {
            if let Some(id) = item.path.strip_prefix("key:") {
                assert!(shortcut(id).is_some(), "unknown shortcut {id}");
            }
            item.children.iter().for_each(walk);
        }
        for kind in [Kind::Browser, Kind::WebApp] {
            for m in menus(kind) {
                walk(&m);
            }
        }
    }

    #[test]
    fn shortcut_ids_are_unique() {
        let mut ids: Vec<_> = SHORTCUTS.iter().map(|s| s.id).collect();
        ids.sort_unstable();
        let before = ids.len();
        ids.dedup();
        assert_eq!(before, ids.len());
    }

    #[test]
    fn augment_inserts_between_app_and_window_menus() {
        let mut menu = crate::atspi::synthetic_menu("google-chrome");
        let before: Vec<_> = menu.children.iter().map(|c| c.label.clone()).collect();
        augment("google-chrome", &mut menu);
        let labels: Vec<_> = menu.children.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(labels.first().copied(), Some(before[0].as_str()));
        assert_eq!(labels.last().copied(), Some("Window"));
        assert!(labels.contains(&"Bookmarks"));
        let ids: Vec<_> = menu.children.iter().map(|c| c.id).collect();
        assert_eq!(
            ids,
            (0..i32::try_from(ids.len()).unwrap()).collect::<Vec<_>>()
        );

        let mut kate = crate::atspi::synthetic_menu("org.kde.kate");
        let n = kate.children.len();
        augment("org.kde.kate", &mut kate);
        assert_eq!(kate.children.len(), n);
    }
}
