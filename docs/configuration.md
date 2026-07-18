# Configuration

renga reads an optional TOML file at startup. This document is the **canonical reference** for the config keys, their defaults, and the precedence order between CLI flags, the config file, and built-in defaults.

For *behavior* documentation (what the IME overlay feels like, recommended overrides, troubleshooting) see [`ime.md`](./ime.md). This page only covers the config-key surface itself.

## Location

| OS | Path |
|---|---|
| Linux | `$XDG_CONFIG_HOME/renga/config.toml` (default `~/.config/renga/config.toml`) |
| macOS | `~/Library/Application Support/renga/config.toml` |
| Windows | `%APPDATA%\renga\config.toml` |

Missing or malformed files fall back to defaults with a stderr warning; renga never fails to start because of a config issue. Unknown sections and keys are ignored for forward-compat.

## Precedence

For every key that is also exposed as a CLI flag, the resolution order is:

**CLI flag > config file value > built-in default**

For `[ui] lang` the OS-locale detection step sits between the config file and the English fallback — see the table below.

## `[ime]` — IME composition overlay

```toml
[ime]
mode = "hotkey"               # "hotkey" | "off"
freeze_panes_on_overlay = true
overlay_catchup_ms = 3000
```

| Key | Type | Default | CLI flag | Notes |
|---|---|---|---|---|
| `mode` | `"hotkey" \| "off"` | `"hotkey"` | `--ime <hotkey\|off>` | `hotkey` binds `Ctrl+;` (with `Alt+;` / `Alt+I` as fallbacks for terminals that swallow `Ctrl+;`). `off` swallows `Ctrl+;` silently — useful when the host terminal already places IME candidates correctly. |
| `freeze_panes_on_overlay` | bool | `true` | `--ime-freeze-panes[=BOOL]` | While the overlay is open, freeze pane repaints so Claude's streaming tokens don't flicker past your IME candidates. Only takes effect while the overlay is open, so non-IME users never see a behavior change. Pass `=false` to force live repaints during composition. |
| `overlay_catchup_ms` | u64 ms | `3000` | `--ime-overlay-catchup-ms <MS>` | When the freeze is active, force a single repaint every N ms so body-content progress stays visible. `0` is a pure freeze (no periodic catch-up). Non-zero values are clamped to at least `100`. |

> A third `mode` value, `"always"`, used to auto-open the overlay on every Claude pane focus. It was removed because the auto-open never worked reliably in practice. Users who want the overlay ready on focus should press `Ctrl+;` once.

## `[ui]` — UI language and event-loop rate

```toml
[ui]
lang = "auto"   # "auto" | "ja" | "en"
fps = 30
file_tree = true
```

| Key | Type | Default | CLI flag | Notes |
|---|---|---|---|---|
| `lang` | `"auto" \| "ja" \| "en"` | `"auto"` | `--lang <auto\|ja\|en>` | Language for status bar hints and preview panel error messages. `auto` detects from the OS locale via `sys-locale` (wraps `nl_langinfo` on Unix and `GetUserDefaultLocaleName` on Windows). Locales starting with `ja` render in Japanese; everything else falls back to English. Values are case-insensitive in both CLI and TOML. |
| `fps` | u16 | `30` | `--fps <FPS>` | Main event-loop target rate. Drives the crossterm poll timeout used while the TUI is idle; higher values reduce input latency and make animations smoother at the cost of more wakeups. `0` is clamped to `1` at runtime so a bad config or CLI override never turns into a busy-spin. |
| `file_tree` | bool | `true` | `--file-tree` / `--no-file-tree` | Whether the file tree sidebar is visible when a workspace is created — the initial tab and every new tab. `false` starts tabs without the sidebar; `Alt+F` still toggles it per-tab at runtime either way. On the CLI, passing both flags lets the last one win. |

Precedence:

- `lang` — CLI > config > OS locale detection > English fallback.
- `fps` — CLI > config > default (with the `0`→`1` clamp applied last).
- `file_tree` — CLI > config > default (`true`).

## `[shell]` — pane shell

```toml
[shell]
program = "cmd"   # e.g. "cmd", "powershell", "fish", or a full path
```

| Key | Type | Default | CLI flag | Notes |
|---|---|---|---|---|
| `program` | string | *(unset — auto-detect)* | `--shell <PROGRAM>` | Shell launched in every new pane (the initial pane, splits, and new tabs). Bare names are resolved via PATH (`where` on Windows, `which` on Unix); full paths are used as-is. Unset keeps the historical auto-detection: Git Bash → bash in PATH → PowerShell on Windows, `$SHELL` → `/bin/sh` on Unix. A program that cannot be resolved falls back to auto-detection with a stderr warning — a typo never prevents renga from starting. |

> Note: renga's cwd tracking for new panes relies on an OSC 7 hook that is only injected for bash / zsh. With `cmd` or `powershell` as the pane shell, new panes and the file tree still open in the pane's *initial* directory, but they won't follow `cd` movements inside the shell.

## See also

- [`ime.md`](./ime.md) — IME overlay behavior, recommended overrides, troubleshooting.
- [`keymap.md`](./keymap.md) — Full keybindings, including the IME overlay's internal keymap.
- [`api-surface-v1.0.md`](./api-surface-v1.0.md) §4 — the wire-frozen subset of the config / layout / env surface.
