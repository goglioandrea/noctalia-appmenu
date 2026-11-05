# noctalia-appmenu (Noctalia v5 fork)

A macOS/KDE-style **global menu** for [Noctalia](https://github.com/noctalia-dev/noctalia) v5:
the focused application's menubar, shown in the bar. Works on **niri**,
**Umbriel** and **Hyprland**.

This is a fork of [yolo-labz/noctalia-appmenu](https://github.com/yolo-labz/noctalia-appmenu),
which targets the Quickshell-based Noctalia v4 and niri only. Noctalia v5 replaced
Quickshell/QML with a native shell and a Luau plugin API, so the QML widget could not
be carried over. What changed:

| | upstream | this fork |
|---|---|---|
| Shell | Noctalia v4 (Quickshell, QML plugin) | **Noctalia v5.4+** (Luau plugin, `plugin_api = 42`) |
| Compositors | niri | **niri, Umbriel and Hyprland**, detected automatically |
| Bridge start | systemd user service (Home-Manager) | **started by the plugin itself** |
| Updates to the UI | `qs ipc call` | JSON lines on the bridge's stdout (`--stream`) |
| Packaging | Nix flake | **Arch `PKGBUILD`** (Nix files kept, untested) |

## How it works

```
 compositor IPC ──► bridge ──► AT-SPI walk of the focused app's menubar
 (niri / Umbriel /    │
  Hyprland)           │
                      ├─► ~/.cache/noctalia-appmenu/active.json   (0600, atomic)
                      └─► stdout: {"event":"active",…}  ──►  plugin service
                                                               │
                                     bar widget ◄── noctalia.state ──► panel
                                         │ click                       │ click
                                         └────────► service ──► `bridge atspi-click`
```

- **Bridge** (`bridge/`, Rust): follows focus over the compositor's IPC, reads the
  focused app's menubar over AT-SPI (the accessibility bus), and announces every
  change. A click is a short-lived `atspi-click` subprocess that focuses the right
  window and activates the menu item.
- **Plugin** (`plugins/appmenu/`, Luau): a `[[service]]` that runs the bridge and
  restarts it if it exits, a bar widget with the app name and top-level menu titles,
  and a panel that shows a menu's items with drill-down into submenus.

## Install (Arch Linux)

```bash
git clone https://github.com/goglioandrea/noctalia-appmenu
cd noctalia-appmenu/packaging/arch
makepkg -si
```

Then, once, as your user:

```bash
noctalia msg plugins source add appmenu path /usr/share/noctalia-appmenu/plugins
noctalia msg plugins enable goglioandrea/appmenu
```

and add the **AppMenu** widget to your bar (Settings → Bar). There is no service to
enable: the plugin starts `noctalia-appmenu-bridge --stream` and stops it with the
shell.

### Without a package (development)

```bash
scripts/dev-install.sh          # bridge → ~/.local/bin, plugin → ~/.local/share/noctalia/plugins/appmenu
scripts/dev-install.sh --link   # symlink the plugin, so edits apply on plugin reload
noctalia msg plugins enable goglioandrea/appmenu
```

If the shell's `PATH` does not include `~/.local/bin`, set the plugin's **Bridge
command** setting (Settings → Plugins → AppMenu, advanced) to the absolute path.

## Which apps show a real menu

The bridge reads menus over **AT-SPI**. Apps that export their menubar there get
their real menu; the others get a **fallback menu** built from their `.desktop`
entry (name, desktop actions such as *New Window*) plus window controls.

| Toolkit / app | Result |
|---|---|
| Qt 5/6 (Kate, Okular, Dolphin, Anki, Krita…) | real menu |
| GTK 3 with the accessibility bridge (GIMP, LibreOffice…) | real menu |
| GTK 4 / libadwaita (popover menus) | fallback |
| Electron (VS Code, Slack…) | fallback, real menu with `--force-accessibility` |
| Chromium / Chrome | fallback |
| Firefox / Thunderbird | fallback (see below) |
| libcosmic / Iced | fallback |

**Qt apps** started before the bridge may need a restart: the bridge turns on the
accessibility bus when it starts, and Qt checks it at launch.

**Firefox** exposes its menubar over AT-SPI (`about:config` →
`accessibility.force_disabled = 0`), but its menus only fill in when visibly opened,
which would leave Firefox's own menubar pinned on screen. The bridge therefore serves
the fallback for Firefox. A clean fix needs the compositor to advertise
`org_kde_kwin_appmenu_manager`; niri, Umbriel and Hyprland do not today.

The window controls in the fallback map to each compositor's own actions. On
Umbriel and Hyprland, *Expand Column to Available Width* has no equivalent and is
hidden. On Hyprland both dispatch syntaxes are handled: the Lua one (0.57+) and the
classic one (0.56 and older).

## Configuration

Plugin settings (Settings → Plugins → AppMenu): **Bridge command**.
Widget settings (per bar widget): show the app name, maximum number of menus,
maximum title length, hide when empty.

The bridge itself reads `~/.config/noctalia-appmenu-bridge/config.toml` (optional;
for example `desktop_fallback = false` to show nothing for apps without a menu).

To force a compositor when both sockets exist (nested sessions):
`NOCTALIA_APPMENU_COMPOSITOR=niri`, `=umbriel` or `=hyprland` in the shell's environment.

## Troubleshooting

```bash
cat "$XDG_RUNTIME_DIR/noctalia-appmenu-bridge.log"          # bridge log (plugin-started)
RUST_LOG=noctalia_appmenu_bridge=debug noctalia-appmenu-bridge --foreground --stream
cat ~/.cache/noctalia-appmenu/active.json | jq '{compositor, app_id, source}'
umbriel focused-window --json        # Umbriel: is a window active, with a pid?
niri msg --json focused-window       # niri
busctl --user list | grep -E 'a11y|AppMenu'
```

- *Widget shows a warning triangle*: the bridge is not on the shell's `PATH`; set
  **Bridge command** to its absolute path.
- *"bridge stopped; restarting"*: read the log above. A second bridge (for example an
  old systemd unit from upstream) holding `org.noctalia.AppMenu` makes new ones exit;
  stop it with `systemctl --user disable --now noctalia-appmenu-bridge`.

## Security notes

See [SECURITY.md](SECURITY.md) for the threat model and the hardening in this fork.

## Project layout

```
bridge/            Rust bridge (compositor focus → AT-SPI → active.json / stdout stream)
  src/wm.rs          compositor detection, window focus, allow-listed window actions
  src/niri.rs        niri focus tracker
  src/umbriel.rs     Umbriel focus tracker
  src/hyprland.rs    Hyprland focus tracker and dispatch (Lua + classic syntax)
  src/publish.rs     private active.json + stdout stream
plugins/appmenu/   Noctalia v5 plugin (plugin.toml, service/bar/panel .luau, translations)
packaging/arch/    PKGBUILD
docs/adr/          upstream architecture decisions (still relevant for the bridge)
```

## Acknowledgements

- [yolo-labz/noctalia-appmenu](https://github.com/yolo-labz/noctalia-appmenu) by Pedro
  Balbino — the bridge, its AT-SPI walker and the design this fork builds on.
- [Noctalia](https://github.com/noctalia-dev/noctalia) and
  [Umbriel](https://github.com/noctalia-dev/umbriel).
- [niri](https://github.com/YaLTeR/niri) by Ivan Molodetskikh.
- [at-spi2-core](https://gitlab.gnome.org/GNOME/at-spi2-core).

## License

[Apache-2.0](LICENSE).
