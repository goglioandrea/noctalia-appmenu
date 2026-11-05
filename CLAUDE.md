# CLAUDE.md — noctalia-appmenu (Noctalia v5 fork)

Instructions for AI agents working on this repository. This is a fork of
`yolo-labz/noctalia-appmenu`; upstream's agent rules (NixOS deploys, self-merge to
yolo-labz, "niri only") do **not** apply here.

## What this project is

A global menu for **Noctalia v5** (native shell, Luau plugins) on **niri, Umbriel and Hyprland**,
developed and used on **Arch Linux**.

- `bridge/` (Rust): follows compositor focus (`niri.rs`, `umbriel.rs`, `hyprland.rs`
  behind the `FocusSink` trait), walks the focused app's menubar over AT-SPI (`atspi.rs`),
  publishes `active.json` + JSON lines on stdout in `--stream` mode (`publish.rs`).
  `wm.rs` owns compositor detection, window focus and the allow-listed window actions.
- `plugins/appmenu/` (Luau): `service.luau` runs the bridge via `noctalia.runStream`
  and is the only entry that talks to it; `bar.luau` and `panel.luau` render from
  `noctalia.state`. The channels are documented at the top of `common.luau`.
- `packaging/arch/PKGBUILD`, `scripts/dev-install.sh`.

Upstream's ADRs in `docs/adr/` still explain the bridge (why AT-SPI, why a sidecar,
the desktop fallback, the Firefox limitation). Specs under `specs/` are upstream
history.

## References

- Noctalia plugin API (v5.4.x): https://docs.noctalia.dev/noctalia/plugins/development/
  — `plugin_api` levels, runtime API, declarative UI. The type definitions in
  `noctalia-dev/official-plugins/noctalia.d.luau` are the most precise source.
- Umbriel IPC: https://docs.noctalia.dev/umbriel/ipc/ (window ids are strings;
  `subscribe` → full `windows` snapshots; `active` = keyboard focus).
- niri IPC: https://niri-wm.github.io/niri/IPC.html
- Hyprland IPC: `.socket.sock` (request → reply until EOF, `j/` = JSON) and
  `.socket2.sock` (`EVENT>>DATA` lines). From 0.57 `dispatch` takes Lua
  (`hl.dsp.…`); up to 0.56 the classic dispatcher syntax. `hyprland::dispatch`
  tries both. Validate addresses with `hyprland::parse_address`.

## Rules

1. **Security boundaries stay closed.** AT-SPI data is app-controlled: keep the
   child-count/label clamps and the same-connection rule in `atspi.rs`. Every action
   that reaches a compositor goes through `wm::is_allowed_niri_action`. The plugin
   passes bridge arguments as an argv array after `--`, never through a shell string.
   See `SECURITY.md`.
2. **stdout is protocol in `--stream` mode.** Logs go to stderr only; never
   `println!` in the daemon path.
3. **Noctalia stream lines are capped at 64 KiB.** Big payloads are announced by
   file path (`publish::MAX_INLINE_BYTES`).
4. **Test the bridge at the model layer** (pure functions, `insta` snapshots, fake
   Unix sockets as in `wm.rs`/`umbriel.rs` tests). zbus has no mock crate.
5. **Do not claim a fix works without running it.** Unit tests are not a shell
   reload; say what was and was not verified.
6. Feature branches + PRs; no force-push to `main`; `git commit -s`.

## Useful commands

```bash
cd bridge && cargo test --lib --bins && cargo clippy --all-targets
scripts/dev-install.sh --link && noctalia msg plugins enable goglioandrea/appmenu
cat "$XDG_RUNTIME_DIR/noctalia-appmenu-bridge.log"
umbriel subscribe windows        # raw focus events on Umbriel
niri msg --json event-stream     # raw focus events on niri
```
