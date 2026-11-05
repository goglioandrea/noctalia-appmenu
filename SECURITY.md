# Security

## Reporting

Report vulnerabilities privately through GitHub's advisory flow:
https://github.com/goglioandrea/noctalia-appmenu/security/advisories/new

Issues that also affect upstream should be reported to
[yolo-labz/noctalia-appmenu](https://github.com/yolo-labz/noctalia-appmenu/security/advisories/new) too.

## Threat model

Everything runs as the logged-in user. The bridge needs no privileges and opens no
network sockets. The inputs it does not control are:

1. **Applications on the accessibility bus.** Any app can publish an arbitrary
   AT-SPI tree: labels, child counts, references to other accessibles.
2. **The compositor IPC** (niri / Umbriel). Trusted: it already controls every
   window.
3. **`active.json` and the CLI arguments** of `atspi-click` / `atspi-expand`,
   which carry values that originated from (1).
4. **`.desktop` files** in the XDG data dirs (used by the fallback menu). Same trust
   as the user's own files.

The goals: a hostile app must not be able to make a menu click run arbitrary
commands, act on a different app, or crash/stall the bridge; menu contents (which
can include window titles, recent files, bookmarks) must not be readable by other
local users.

## Audit (November 2026) — findings and fixes

| # | Finding | Severity | Fix |
|---|---|---|---|
| 1 | The synthetic window menu dispatched `niri:<action>` by passing `<action>` to `niri msg action` unchecked. The value travels through `active.json` and a CLI argument, so anything able to influence it could request any niri action, including `spawn`. | Medium | Closed allow-list of window actions (`wm::is_allowed_niri_action`), applied before any compositor call; Umbriel actions come from a fixed translation table. |
| 2 | An app-reported `ChildCount` (an `i32`) was used as a `Vec` capacity and loop bound. A negative value became a huge `usize` (an abort, since release builds use `panic = "abort"`); a huge positive one meant millions of D-Bus round-trips. | Medium (DoS) | `clamp_child_count`: 0 ..= 512 children per node. |
| 3 | Menu walks followed child references to *any* bus connection, so an app could list another app's accessibles as its own menu items and have an innocent-looking click activate something elsewhere. | Medium | The walk only follows children on the parent's own connection. |
| 4 | Labels were unbounded, so an app could inflate `active.json` and the shell's memory. | Low | Labels truncated to 256 characters. |
| 5 | `active.json` was written with the default umask (usually world-readable `0644`) and, without `HOME`, under the shared `/tmp` (symlink / pre-creation attacks). Writes were also non-atomic, so readers could see torn JSON. | Low | Private `0700` directory, `0600` file, refuses a symlinked directory, atomic write via temp file + `rename`; falls back to `$XDG_RUNTIME_DIR`, never `/tmp`. |
| 6 | The upstream push path spawned `qs` (resolved through `PATH`) synchronously on every change. | Info | Removed; updates go to stdout. |
| 7 | `.claude/settings.json` pre-approved broad shell commands (`gh:*`, `nixos-rebuild:*`, `systemctl --user:*`, …) for any AI agent opened in the repo. | Info | Trimmed to read-only and build/test commands. |

The plugin side adds defence in depth: the service validates every service/path/window
before it goes on the bridge's command line, passes them as an argv array (no shell)
after `--`, and reads only the bridge's own `…/noctalia-appmenu/active.json`.

### Inherent to the design (not fixable here)

- The bridge sets `org.a11y.Status.IsEnabled = true` so apps export their trees. That
  is how every screen reader works, but it means any process of yours can read the
  UI of every accessible app. Do not run untrusted software in the same session if
  that matters to you.
- Any process in your session can call the bridge's `RefreshActive` D-Bus method
  (a re-walk). It cannot do anything else through it.

### Not checked in this audit

- Third-party crate advisories: run `cargo audit` / `cargo deny check` in `bridge/`
  (the upstream `deny.toml` is kept).
- The GitHub workflows still target upstream's self-hosted runner and will not run on
  this fork until they are adapted.
