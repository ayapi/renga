# Keymap

Full keybinding reference. The README only carries a "first 5–8 keys" cheat sheet; everything else lives here.

> **macOS users:** the default macOS terminal swallows `Option+<key>` before renga sees it, so the `Alt+<key>` shortcuts — which is nearly the whole global keymap — won't fire out of the box. See [macOS: Option as Meta](#macos-option-as-meta) below for the one-line fix per terminal.

## Pane mode (default)

Every global renga chord lives in the `Alt` namespace. Bare `Ctrl+<key>` combinations (`Ctrl+D`, `Ctrl+W`, `Ctrl+Q`, …) are deliberately **not** bound — they pass through to the program running inside the pane (shell readline, vim, fzf, …) untouched. The only exceptions are `Ctrl+C` (renga-side copy, but only while a text selection exists), `Ctrl+;` (IME overlay), and the `Ctrl+Shift+M` copy-mode alias.

| Key | Action |
|-----|--------|
| `Alt+D` | Split vertically |
| `Alt+E` | Split horizontally |
| `Alt+W` | Close pane / tab |
| `Alt+T` | New tab |
| `Alt+1..9` | Jump to tab N |
| `Alt+Left/Right` | Previous / next tab |
| `Alt+R` | Rename tab (session only) |
| `Alt+S` | Toggle status bar |
| `Alt+Shift+C` | Dump the focused pane's always-on capture ring to replay-compatible `.bin` / `.jsonl` files beneath the platform data directory (`%LOCALAPPDATA%\renga\pane-captures` on Windows). The status bar briefly reports completion or failure. |
| `Alt+P` | Insert the peer-enabled Claude Code launch command into the focused pane (see [`peer-messaging.md`](./peer-messaging.md)). Silently no-ops when the focused pane is in alt-screen mode (vim, less, lazygit, a running Claude / Codex TUI) or its title contains "claude" — by design, so the command bytes aren't injected as keystrokes into a running TUI. Switch focus to a shell-prompt pane and press again. |
| `Alt+F` | Toggle file tree |
| `Alt+O` | Swap preview/terminal layout |
| `Alt+Up/Down` | Cycle focus (sidebar, preview, panes) |
| `Alt+PageUp/PageDown` | Scroll the focused pane half a page through scrollback history (no copy mode needed) |
| `Alt+Home` / `Alt+End` | Jump to the top of scrollback / back to the live view |
| `Ctrl+;` / `Alt+;` / `Alt+I` | Open IME composition overlay (centered multi-line — see [`ime.md`](./ime.md)). `Alt+;` and `Alt+I` are fallbacks for terminals that swallow `Ctrl+;` (WSL under Windows Terminal, VS Code terminal on Linux, some tmux configs). |
| `Alt+M` / `Ctrl+Shift+M` | Enter keyboard copy mode on the focused pane (see [Copy mode](#copy-mode-after-altm)) |
| `Alt+Q` | Quit |

Git Bash binds the legacy `ESC C` sequence through `do-lowercase-version` to
`capitalize-word`. Some terminals encode `Alt+Shift+C` as that same sequence,
so renga consumes this rarely used alias for capture. Caps Lock plus `Alt+c`
can encode identically and may therefore trigger a dump. Automation can use the
`dump_pane_capture` MCP tool with an explicit `target` or `all: true` instead.

## Copy mode (after `Alt+M`)

Keyboard-only text selection and copy, modeled on Windows Terminal's mark mode. A `COPY` hint appears on the pane's bottom border and a block cursor appears at the pane's caret. While the mode is active no key (or paste) reaches the PTY.

> `Ctrl+Shift+M` matches Windows Terminal's own mark-mode binding, but most host terminals (including Windows Terminal itself) intercept it before renga sees it — `Alt+M` is the binding that always works.

| Key | Action |
|-----|--------|
| `←` `↑` `↓` `→` | Move the cursor. `↑` at the top edge scrolls into history. |
| `Home` / `End` | Jump to the first / last column |
| `PageUp` / `PageDown` | Scroll the view a page through history |
| `Shift` + any movement above | Extend the selection (anchored at the cursor before the first shifted move) |
| Movement without `Shift` | Collapse the selection |
| `Enter` / `Ctrl+C` | Copy the selection to the clipboard and exit |
| `Esc` | Exit without copying |
| Any mouse click / wheel | Cancels the mode (mouse selection takes over) |

## macOS: Option as Meta

By default macOS terminals bind `Option+<key>` to Unicode input (`å`, `∫`, `π`, …), so renga's `Alt`-based shortcuts — splits, tabs, quit, essentially the entire global keymap — never reach the app. Flip Option to act as a Meta key — it's a one-line change in every modern terminal. If you're on plain **Terminal.app**, consider switching to one of the terminals below first; they all handle IME, ligatures, and the image preview panel better than Terminal.app anyway.

| Terminal | Setting |
|---|---|
| **WezTerm** (`~/.wezterm.lua`) | `config.send_composed_key_when_left_alt_is_pressed = false` <br> `config.send_composed_key_when_right_alt_is_pressed = false` |
| **iTerm2** | Settings → Profiles → Keys → set **Left Option key** and **Right Option key** to **Esc+** |
| **Alacritty** (`~/.config/alacritty/alacritty.toml`) | `[window]` <br> `option_as_alt = "Both"` (or `"OnlyLeft"` / `"OnlyRight"`) |
| **Ghostty** (`~/.config/ghostty/config`) | `macos-option-as-alt = true` |
| **Kitty** (`~/.config/kitty/kitty.conf`) | `macos_option_as_alt yes` |
| **Terminal.app** | Settings → Profiles → Keyboard → tick **Use Option as Meta key** |

**Known gaps**

- Some macOS IMEs (Kotoeri's "Romaji" toggle, kana layouts, …) bind Option themselves. If flipping Option breaks IME for you, try the `OnlyLeft` / `OnlyRight` variants so one Option stays native to the OS.
- `Alt+1..9` can collide with macOS Mission Control / Spaces shortcuts on some setups. If the OS swallows the number keys, `Alt+Left/Right` still cycles tabs.

## File tree mode (after `Alt+F`)

| Key | Action |
|-----|--------|
| `j` / `k` | Move selection |
| `Enter` | Open file / expand directory (inline) |
| `h` | Move the tree root up one level |
| `l` | Descend into the selected directory (no-op on files) |
| `c` | Split left/right and queue the Claude+peer launch line in the selection's directory (file → parent, empty → tree root). Not executed until you press Enter, same as `Alt+P`. |
| `v` | Same as `c` but splits top/bottom. |
| `.` | Toggle hidden files |
| `Esc` | Return to pane |

## Preview mode (after focusing preview)

| Key | Action |
|-----|--------|
| `j` / `k` | Scroll vertically |
| `h` / `l` | Scroll horizontally |
| `Alt+W` | Close preview |
| `Esc` | Return to pane |

## Mouse

| Action | Effect |
|--------|--------|
| Click pane | Focus pane |
| Click tab | Switch tab |
| Double-click tab | Rename tab |
| Click `+` | New tab |
| Double-click pane outer edge | Split toward the clicked side. Top / Left places the new pane on the clicked side; Bottom / Right places it on the trailing side. Corner cells are ignored. Refused when the resulting pane would be smaller than `min_pane_width` / `min_pane_height` or when the workspace is already at the pane cap. |
| Double-click shared border | Split the pane on the leading side of the divider, dropping the new pane right on the border (between the two siblings). A vertical divider splits the left pane to the right; a horizontal divider splits the top pane downward. Junction cells where two dividers cross are ignored. Same `min_pane_width` / `min_pane_height` / pane-cap refusals as above. |
| Drag border | Resize panels |
| Scroll wheel | Scroll file tree / preview / terminal history. In panes running a TUI that subscribed to mouse reporting (Claude Code `/tui fullscreen`, vim, lazygit, less, …) the wheel is forwarded to the app instead. |
| Click / drag inside a pane | Normally selects text for copy. When the pane is running a mouse-reporting TUI, the click is forwarded to the app so buttons, carets, etc. work. Hold `Shift` to force renga-side text selection (same escape hatch as tmux / alacritty). |

Both wheel and click forwarding can be disabled globally with `RENGA_DISABLE_MOUSE_FORWARD=1` — useful for nested renga or terminals whose mouse-protocol encoding confuses the inner app.

## IME overlay keymap

The keys *inside* the IME composition box (commit, navigate, close-and-restore-draft, etc.) are documented in [`ime.md`](./ime.md#overlay-keymap). Only the open / fallback chords (`Ctrl+;` / `Alt+;` / `Alt+I`) appear in the Pane-mode table above.
