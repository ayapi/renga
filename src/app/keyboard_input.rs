use super::*;

impl App {
    // ─── Key handling ─────────────────────────────────────

    pub fn handle_key_event(&mut self, key: KeyEvent) -> Result<bool> {
        // First-launch macOS tip: dismiss on any key, but fall through
        // so the key still performs its normal action. The banner is a
        // transient hint, not a modal — the user shouldn't have to
        // press a key twice (once to dismiss, once to do what they
        // wanted). Persists the marker file here so the banner never
        // reappears on the next launch, including when the next key
        // is Alt+Q — otherwise a quit-while-banner-up would leave
        // the marker unwritten and the tip would return next launch.
        if self.macos_tip_visible {
            self.dismiss_macos_tip();
        }

        // Emergency escape hatch: Alt+Q must always quit renga, even
        // while the IME composition overlay is holding input. Checked
        // before overlay routing so the user can never get trapped in
        // a wedged composition mode. Alt (not Ctrl) so the key never
        // shadows in-pane bindings — Ctrl+Q is XON / vim visual-block
        // territory and now forwards to the PTY untouched.
        if key.modifiers == KeyModifiers::ALT
            && matches!(key.code, KeyCode::Char('q') | KeyCode::Char('Q'))
        {
            self.should_quit = true;
            return Ok(true);
        }

        // IME composition overlay — route every relevant key into the
        // buffer until the user commits or cancels. Takes precedence
        // over rename and every other handler so composition never
        // leaks into the layout / PTY unintentionally.
        if self.overlay.is_some() {
            return crate::input::overlay::handle_overlay_key(self, key);
        }

        if self.codex_peer_notification_is_visible() {
            if matches!(key.code, KeyCode::Esc)
                || (key.modifiers == KeyModifiers::CONTROL
                    && matches!(key.code, KeyCode::Char('c')))
            {
                self.dismiss_codex_peer_notification();
                return Ok(true);
            }
            if crate::input::overlay::is_overlay_commit_key(key) {
                return self
                    .accept_codex_peer_notification()
                    .map_err(|e| anyhow::anyhow!(e.to_string()));
            }
            match key.code {
                KeyCode::Up
                | KeyCode::Down
                | KeyCode::Left
                | KeyCode::Right
                | KeyCode::Home
                | KeyCode::End
                | KeyCode::PageUp
                | KeyCode::PageDown => {
                    // Navigation does not dismiss the notice. Continue so later
                    // renga handlers can consume their own chords and modes;
                    // unclaimed keys follow the normal PTY path.
                }
                _ => {
                    // The user started typing or acting, so get the notice out
                    // of the way and return its message to the next-focus queue.
                    self.requeue_codex_peer_notification();
                }
            }
        }

        // Rename mode — swallow all input until Enter/Esc.
        if self.rename_input.is_some() {
            return Ok(self.handle_rename_key(key));
        }

        // Keyboard copy mode — while active, every key is handled (or
        // swallowed) here so nothing leaks to the PTY.
        if self.copy_mode.is_some() {
            return self.handle_copy_mode_key(key);
        }

        // Alt+Shift+C — dump the focused pane's always-on capture ring. Some
        // legacy terminals encode this as Alt plus an uppercase C without an
        // explicit Shift bit; accept that form without consuming Alt+c.
        let capture_dump_key = matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
            && (key.modifiers == (KeyModifiers::ALT | KeyModifiers::SHIFT)
                || (key.modifiers == KeyModifiers::ALT && matches!(key.code, KeyCode::Char('C'))));
        if capture_dump_key && self.ws().focus_target == FocusTarget::Pane {
            let pane_id = self.ws().focused_pane_id;
            let capture = self
                .ws()
                .panes
                .get(&pane_id)
                .and_then(|pane| pane.capture.clone());
            let Some(capture) = capture else {
                self.pane_capture_status = Some("disabled by ring_bytes = 0".into());
                self.status_bar_visible = true;
                self.dirty = true;
                return Ok(true);
            };
            self.pane_capture_status = Some(format!("dumping pane {pane_id}..."));
            self.status_bar_visible = true;
            self.dirty = true;
            let event_tx = self.event_tx.clone();
            if let Err(error) =
                crate::pane_capture::dump_captures_async(vec![capture], move |result| {
                    let status = result
                        .as_ref()
                        .map(crate::pane_capture::format_dump_report)
                        .unwrap_or_else(|error| format!("dump failed: {error}"));
                    let _ = event_tx.send(AppEvent::PaneCaptureDumped(status));
                })
            {
                self.pane_capture_status = Some(format!("dump failed: {error}"));
            }
            return Ok(true);
        }

        // Open the IME composition overlay. Primary hotkey is
        // `Ctrl+;`, with `Alt+;` and `Alt+I` as fallbacks for
        // terminals that refuse to pass `Ctrl+;` through to
        // stdin. ASCII has no encoding for Ctrl+punctuation and
        // many terminals (Windows Terminal with WSL, VS Code
        // terminal on Linux, plain TTYs, some tmux configs) drop
        // the Ctrl modifier and deliver a bare `;` to the
        // application. The Alt-based fallbacks arrive as an
        // ESC-prefixed sequence that every tier-1 terminal
        // forwards reliably, so the overlay is always reachable.
        //
        // Originally gated to `is_claude_running()` panes, but
        // that proved flaky — Claude briefly retitles the pane
        // while running tools, so the detection would flicker
        // and the hotkey would "mysteriously stop working" mid-
        // session. The overlay opens unconditionally on any
        // focused pane; users who don't need IME just don't
        // press the hotkey.
        let is_semi = matches!(key.code, KeyCode::Char(';'));
        let is_alt_i = key.modifiers == KeyModifiers::ALT
            && matches!(key.code, KeyCode::Char('i') | KeyCode::Char('I'));
        let is_open_hotkey = ((key.modifiers == KeyModifiers::CONTROL
            || key.modifiers == KeyModifiers::ALT)
            && is_semi)
            || is_alt_i;
        if is_open_hotkey {
            match self.ime_mode {
                crate::config::ImeMode::Off => {
                    // User opted out of the overlay. Don't leak a bare
                    // ';' to the PTY either: terminals encode Ctrl+;
                    // inconsistently, and falling through to
                    // `key_event_to_bytes` strips the Ctrl modifier and
                    // injects a stray semicolon into the shell. Silent
                    // swallow matches the "off" intent — the hotkey
                    // simply does nothing.
                    return Ok(true);
                }
                crate::config::ImeMode::Hotkey => {
                    // Fall through to open the overlay deliberately.
                }
            }
            let focused_id = self.ws().focused_pane_id;
            let pane_focused = matches!(self.ws().focus_target, FocusTarget::Pane)
                && self.ws().panes.contains_key(&focused_id);
            if pane_focused {
                if let Some(saved) = self.take_overlay_draft(focused_id) {
                    self.overlay = Some(saved);
                    self.mark_layout_change();
                    return Ok(true);
                }

                // Visible-input bootstrap is Claude-specific. Codex
                // panes use a different composer layout, and trying to
                // "steal" their draft into the IME overlay corrupts the
                // handoff instead of preserving it.
                let snapshot = (!self
                    .pane_expects_codex_peer_delivery(self.active_tab, focused_id))
                .then(|| {
                    self.ws()
                        .panes
                        .get(&focused_id)
                        .and_then(crate::input::overlay::snapshot_visible_input)
                })
                .flatten();

                if snapshot.as_ref().is_some_and(|snapshot| {
                    crate::input::overlay::visible_input_contains_claude_paste_placeholder(
                        &snapshot.buffer,
                    )
                }) {
                    self.overlay = Some(OverlayState::new(focused_id));
                    self.mark_layout_change();
                    return Ok(true);
                }

                let mut overlay = OverlayState::new(focused_id);
                if let Some(snapshot) = snapshot.as_ref() {
                    overlay.buffer = snapshot.buffer.clone();
                    overlay.cursor = snapshot.cursor.min(overlay.buffer.chars().count());
                }
                self.overlay = Some(overlay);

                if let Some(snapshot) = snapshot.as_ref() {
                    let clear = crate::input::overlay::clear_visible_input_bytes(snapshot);
                    if !clear.is_empty() {
                        if let Some(pane) = self.ws_mut().panes.get_mut(&focused_id) {
                            let _ = pane.write_input(&clear);
                        }
                    }
                }
                self.mark_layout_change();
                return Ok(true);
            }
            // Fall through when focus is on the file tree / preview;
            // Ctrl+; in those contexts has no meaning and shouldn't
            // open an overlay attached to a hidden target.
        }

        // Alt+Q — quit
        if key.modifiers == KeyModifiers::ALT
            && matches!(key.code, KeyCode::Char('q') | KeyCode::Char('Q'))
        {
            self.should_quit = true;
            return Ok(true);
        }

        // Alt+R — rename active tab (session only)
        if key.modifiers == KeyModifiers::ALT
            && matches!(key.code, KeyCode::Char('r') | KeyCode::Char('R'))
        {
            self.rename_input = Some(String::new());
            if !self.status_bar_visible {
                self.mark_layout_change();
            }
            return Ok(true);
        }

        // Alt+M / Ctrl+Shift+M — enter keyboard copy mode (WT
        // mark-mode style). Ctrl+Shift+M matches Windows Terminal's
        // own markMode default, but many hosts (including WT itself,
        // which binds it to its native mark mode) intercept it before
        // it reaches renga — and legacy terminals encode it as a bare
        // CR. Alt+M arrives as an ESC-prefixed sequence every tier-1
        // terminal forwards reliably, so it is the binding that
        // always works.
        if matches!(key.code, KeyCode::Char('m') | KeyCode::Char('M'))
            && (key.modifiers == KeyModifiers::ALT
                || key.modifiers == KeyModifiers::CONTROL | KeyModifiers::SHIFT)
        {
            self.enter_copy_mode();
            return Ok(true);
        }

        // Ctrl+C — if text is selected, copy to clipboard instead of sending SIGINT
        if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('c') {
            if let Some(ref sel) = self.selection.clone() {
                let (sr, sc, er, ec) = sel.normalized();
                if sr != er || sc != ec {
                    let text = match sel.target {
                        SelectionTarget::Pane(pane_id) => self
                            .ws()
                            .panes
                            .get(&pane_id)
                            .map(|p| extract_selected_text(p, sr, sc, er, ec))
                            .unwrap_or_default(),
                        SelectionTarget::Preview => {
                            extract_preview_selected_text(&self.ws().preview, sr, sc, er, ec)
                        }
                    };
                    if !text.is_empty() {
                        self.copy_to_clipboard(&text);
                    }
                    self.selection = None;
                    return Ok(true);
                }
            }
            // No selection — fall through to forward Ctrl+C to PTY
        }

        // Alt+T — new tab (groups with Alt-based tab nav). The old
        // Ctrl+T alias was dropped with the Ctrl→Alt migration so the
        // byte reaches the PTY (readline transpose-chars, fzf, etc.).
        if key.modifiers == KeyModifiers::ALT
            && matches!(key.code, KeyCode::Char('t') | KeyCode::Char('T'))
        {
            let new_id = self.new_tab()?;
            self.emit_pane_started(new_id);
            return Ok(true);
        }

        // Alt+Right — next tab
        if key.modifiers == KeyModifiers::ALT && key.code == KeyCode::Right {
            if !self.workspaces.is_empty() {
                self.active_tab = (self.active_tab + 1) % self.workspaces.len();
                self.suspend_overlay();
            }
            return Ok(true);
        }

        // Alt+Left — previous tab
        if key.modifiers == KeyModifiers::ALT && key.code == KeyCode::Left {
            if !self.workspaces.is_empty() {
                self.active_tab = if self.active_tab == 0 {
                    self.workspaces.len() - 1
                } else {
                    self.active_tab - 1
                };
                self.suspend_overlay();
            }
            return Ok(true);
        }

        // Alt+S — toggle status bar
        if key.modifiers == KeyModifiers::ALT
            && matches!(key.code, KeyCode::Char('s') | KeyCode::Char('S'))
        {
            self.status_bar_visible = !self.status_bar_visible;
            self.mark_layout_change();
            return Ok(true);
        }

        // Alt+P — insert the peer-enabled claude launch command into
        // the focused pane (trailing space, no Enter). The user reviews,
        // optionally edits, then presses Enter to actually run — a
        // conscious action, which is why we deliberately don't gate
        // this on "is renga-peers installed": pressing Alt+P already
        // means the user wants peer mode, and a missing MCP entry will
        // surface itself when Claude starts.
        //
        // Refuse when the pane is in alternate-screen mode (a TUI —
        // Claude Code itself, vim, less, lazygit — has captured the
        // terminal). Writing the command bytes there would land as
        // keystrokes inside that TUI instead of at a shell prompt,
        // which could accidentally send a prompt to a running Claude.
        if key.modifiers == KeyModifiers::ALT
            && matches!(key.code, KeyCode::Char('p') | KeyCode::Char('P'))
        {
            let ws = self.ws_mut();
            let focused_id = ws.focused_pane_id;
            if let Some(pane) = ws.panes.get_mut(&focused_id) {
                if pane.shell_accepts_command_injection() {
                    let cmd = format!("{CLAUDE_PEER_LAUNCH_CMD} ");
                    let _ = pane.write_input(cmd.as_bytes());
                    self.dirty = true;
                }
                // else: silently no-op; the pane is in an alt-screen
                // TUI. Users can switch to a shell pane and retry.
            }
            return Ok(true);
        }

        // Alt+1 .. Alt+9 — jump to tab N
        if key.modifiers == KeyModifiers::ALT {
            if let KeyCode::Char(c) = key.code {
                if let Some(digit) = c.to_digit(10) {
                    if digit >= 1 && (digit as usize) <= self.workspaces.len() {
                        self.active_tab = (digit as usize) - 1;
                        self.suspend_overlay();
                        return Ok(true);
                    }
                }
            }
        }

        // Alt+Down — next pane (Alt+Left/Right is taken by tab nav,
        // so focus cycling lives on the vertical arrows)
        if key.modifiers == KeyModifiers::ALT && key.code == KeyCode::Down {
            self.focus_next_pane();
            return Ok(true);
        }

        // Alt+Up — previous pane
        if key.modifiers == KeyModifiers::ALT && key.code == KeyCode::Up {
            self.focus_prev_pane();
            return Ok(true);
        }

        // Alt+PageUp / Alt+PageDown — scroll the focused pane's view a
        // half page through scrollback history without entering copy
        // mode; Alt+Home / Alt+End jump to the top of history / back to
        // the live view. Only while focus is on a terminal pane — the
        // file tree and preview have their own scrolling. Always
        // consumed there, even when there is nothing to scroll: falling
        // through would strip the Alt modifier in `key_event_to_bytes`
        // and leak a bare PageUp/Home escape into the PTY app.
        if key.modifiers == KeyModifiers::ALT
            && matches!(
                key.code,
                KeyCode::PageUp | KeyCode::PageDown | KeyCode::Home | KeyCode::End
            )
            && self.ws().focus_target == FocusTarget::Pane
        {
            let focused_id = self.ws().focused_pane_id;
            // Step = half the pane content height (like less's d/u),
            // so consecutive views overlap and lines are easy to track
            // across presses. Copy mode's PageUp/PageDown stays
            // full-page. Falls back to one line when the pane hasn't
            // been rendered yet (no rect on the first frame).
            let page = self
                .pane_content_rect(focused_id)
                .map_or(1, |r| (r.height as usize / 2).max(1));
            if let Some(pane) = self.ws().panes.get(&focused_id) {
                match key.code {
                    KeyCode::PageUp => pane.scroll_up(page),
                    KeyCode::PageDown => pane.scroll_down(page),
                    KeyCode::Home => pane.scroll_to_top(),
                    _ => pane.scroll_reset(),
                }
                self.dirty = true;
            }
            return Ok(true);
        }

        // Preview mode
        if self.ws().focus_target == FocusTarget::Preview {
            return self.handle_preview_key(key);
        }

        // File tree mode
        if self.ws().focus_target == FocusTarget::FileTree {
            if key.modifiers == KeyModifiers::ALT
                && matches!(key.code, KeyCode::Char('f') | KeyCode::Char('F'))
            {
                self.toggle_file_tree();
                return Ok(true);
            }
            return self.handle_file_tree_key(key);
        }

        // Alt+F — toggle file tree
        if key.modifiers == KeyModifiers::ALT
            && matches!(key.code, KeyCode::Char('f') | KeyCode::Char('F'))
        {
            self.toggle_file_tree();
            return Ok(true);
        }

        // Alt+O — swap preview and terminal positions (Alt+P is taken
        // by the claude launch chord, so the swap sits next door on O)
        if key.modifiers == KeyModifiers::ALT
            && matches!(key.code, KeyCode::Char('o') | KeyCode::Char('O'))
        {
            self.layout_swapped = !self.layout_swapped;
            return Ok(true);
        }

        let multi_pane = self.ws().layout.pane_count() > 1;
        let multi_tab = self.workspaces.len() > 1;

        match (key.modifiers, key.code) {
            (KeyModifiers::ALT, KeyCode::Char('d') | KeyCode::Char('D')) => {
                if let Some(new_id) = self.split_focused_pane(SplitDirection::Vertical, None)? {
                    self.emit_pane_started(new_id);
                }
                Ok(true)
            }
            (KeyModifiers::ALT, KeyCode::Char('e') | KeyCode::Char('E')) => {
                if let Some(new_id) = self.split_focused_pane(SplitDirection::Horizontal, None)? {
                    self.emit_pane_started(new_id);
                }
                Ok(true)
            }
            (KeyModifiers::ALT, KeyCode::Char('w') | KeyCode::Char('W')) => {
                if self.ws().focus_target == FocusTarget::Preview {
                    // Close preview and return to pane
                    self.ws_mut().preview.close();
                    self.ws_mut().focus_target = FocusTarget::Pane;
                    Ok(true)
                } else if multi_pane {
                    self.close_focused_pane();
                    Ok(true)
                } else if multi_tab {
                    self.close_tab(self.active_tab);
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
            _ => Ok(false),
        }
    }

    // ─── PTY forwarding ───────────────────────────────────

    /// Route a terminal-level paste payload (bracketed-paste from the
    /// host terminal — typically Ctrl+V on WSL2 / Windows Terminal /
    /// WezTerm / iTerm2) to the right destination. When the IME
    /// composition overlay is open, the paste belongs to the overlay
    /// buffer; otherwise it forwards to the focused pane's PTY via
    /// `forward_paste_to_pty`. Centralizing the routing here keeps
    /// `main.rs` from having to reach into overlay internals.
    pub fn handle_paste(&mut self, text: &str) -> Result<bool> {
        if let Some(overlay) = self.overlay.as_mut() {
            overlay.insert_str(text);
            self.dirty = true;
            return Ok(true);
        }
        // Copy mode swallows pastes: no input may reach the PTY while
        // the mode is active, and `forward_paste_to_pty`'s
        // `scroll_reset` would yank a scrolled-back view to the live
        // screen underneath the screen-relative selection.
        if self.copy_mode.is_some() {
            return Ok(true);
        }
        self.forward_paste_to_pty(text)?;
        Ok(false)
    }

    /// Forward pasted text to PTY, wrapping in bracketed paste only if
    /// the PTY application has enabled the mode (e.g. Claude Code, modern
    /// readline). Sending bracketed paste to a shell that hasn't opted in
    /// causes the escape sequences to appear as literal text (issue #2).
    pub fn forward_paste_to_pty(&mut self, text: &str) -> Result<()> {
        let focused_id = self.ws().focused_pane_id;
        if let Some(pane) = self.ws_mut().panes.get_mut(&focused_id) {
            pane.scroll_reset();
            pane.clear_codex_transcript_overlay_hint();
            if pane.is_bracketed_paste_enabled() {
                let mut data = Vec::with_capacity(text.len() + 12);
                data.extend_from_slice(b"\x1b[200~");
                data.extend_from_slice(text.as_bytes());
                data.extend_from_slice(b"\x1b[201~");
                pane.write_input(&data)?;
            } else {
                pane.write_input(text.as_bytes())?;
            }
        }
        Ok(())
    }

    #[allow(dead_code)]
    pub fn forward_key_to_pty(&mut self, key: KeyEvent) -> Result<()> {
        let focused_id = self.ws().focused_pane_id;
        if let Some(pane) = self.ws_mut().panes.get_mut(&focused_id) {
            pane.scroll_reset();
            pane.clear_codex_transcript_overlay_hint();
            if let Some(bytes) = key_event_to_bytes(&key) {
                pane.write_input(&bytes)?;
            }
        }
        Ok(())
    }

    // ─── Keyboard copy mode ───────────────────────────────

    /// Enter keyboard copy mode on the focused pane. No-op when focus
    /// isn't on a terminal pane, the pane has exited, or its rect
    /// hasn't been rendered yet (first frame).
    pub(crate) fn enter_copy_mode(&mut self) {
        if self.ws().focus_target != FocusTarget::Pane {
            return;
        }
        let pane_id = self.ws().focused_pane_id;
        let Some(inner) = self.pane_content_rect(pane_id) else {
            return;
        };
        // Start at the pane's live cursor cell (like WT mark mode),
        // clamped into the content area.
        let (crow, ccol) = {
            let Some(pane) = self.ws().panes.get(&pane_id) else {
                return;
            };
            // Exited panes stay in the pane map but skip content
            // rendering, so the mode's cursor and selection would be
            // invisible — refuse to enter.
            if pane.exited {
                return;
            }
            let parser = pane.parser.lock().unwrap_or_else(|e| e.into_inner());
            parser.screen().cursor_position()
        };
        self.copy_mode = Some(CopyModeState {
            pane_id,
            cursor_row: (crow as u32).min(inner.height.saturating_sub(1) as u32),
            cursor_col: (ccol as u32).min(inner.width.saturating_sub(1) as u32),
            anchor: None,
        });
        self.selection = None;
        self.dirty = true;
    }

    /// Leave copy mode, dropping any in-progress selection.
    pub(crate) fn exit_copy_mode(&mut self) {
        if self.copy_mode.take().is_some() {
            self.selection = None;
            self.dirty = true;
        }
    }

    /// Content rect (inside the border) of a pane from the last
    /// rendered layout, or `None` when unknown or too small to hold
    /// content.
    fn pane_content_rect(&self, pane_id: usize) -> Option<Rect> {
        let (_, rect) = self
            .ws()
            .last_pane_rects
            .iter()
            .copied()
            .find(|&(id, _)| id == pane_id)?;
        if rect.width < 4 || rect.height < 3 {
            return None;
        }
        Some(super::pane_content_rect(rect))
    }

    /// Modal key handler while copy mode is active. Every key is
    /// consumed: movement keys move the cursor / extend the selection,
    /// Enter / Ctrl+C copy and exit, Esc exits, anything else is
    /// swallowed so stray input can't reach the PTY.
    fn handle_copy_mode_key(&mut self, key: KeyEvent) -> Result<bool> {
        let Some(mut cm) = self.copy_mode.clone() else {
            return Ok(false);
        };
        // Pane closed / exited, tab switched away, or layout
        // degenerated under us — bail out rather than operate on a
        // stale target (exited panes skip content rendering, which
        // would leave the cursor and selection invisible).
        if self
            .ws()
            .panes
            .get(&cm.pane_id)
            .is_none_or(|pane| pane.exited)
        {
            self.exit_copy_mode();
            return Ok(true);
        }
        let Some(inner) = self.pane_content_rect(cm.pane_id) else {
            self.exit_copy_mode();
            return Ok(true);
        };

        if key.code == KeyCode::Esc {
            self.exit_copy_mode();
            return Ok(true);
        }

        let is_copy_key = key.code == KeyCode::Enter
            || (key.modifiers.contains(KeyModifiers::CONTROL)
                && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C')));
        if is_copy_key {
            if let Some(sel) = self.selection.clone() {
                let (sr, sc, er, ec) = sel.normalized();
                if sr != er || sc != ec {
                    let text = self
                        .ws()
                        .panes
                        .get(&cm.pane_id)
                        .map(|p| extract_selected_text(p, sr, sc, er, ec))
                        .unwrap_or_default();
                    if !text.is_empty() {
                        self.copy_to_clipboard(&text);
                    }
                }
            }
            self.exit_copy_mode();
            return Ok(true);
        }

        if matches!(
            key.code,
            KeyCode::Up
                | KeyCode::Down
                | KeyCode::Left
                | KeyCode::Right
                | KeyCode::Home
                | KeyCode::End
                | KeyCode::PageUp
                | KeyCode::PageDown
        ) {
            let max_row = inner.height.saturating_sub(1) as u32;
            let max_col = inner.width.saturating_sub(1) as u32;
            let shift = key.modifiers.contains(KeyModifiers::SHIFT);
            let scroll = cm.apply_move(key.code, shift, max_row, max_col, inner.height as usize);

            // Apply the requested view scroll and measure how far the
            // view actually moved (vt100 clamps at the ends of
            // scrollback) so the anchor can track its content.
            match scroll {
                CopyModeScroll::Up(n) => {
                    if let Some(pane) = self.ws().panes.get(&cm.pane_id) {
                        let before = pane.scrollbar_info().0;
                        pane.scroll_up(n);
                        let moved = pane.scrollbar_info().0.saturating_sub(before);
                        cm.shift_anchor_for_scroll(moved, true, max_row);
                    }
                }
                CopyModeScroll::Down(n) => {
                    if let Some(pane) = self.ws().panes.get(&cm.pane_id) {
                        let before = pane.scrollbar_info().0;
                        pane.scroll_down(n);
                        let moved = before.saturating_sub(pane.scrollbar_info().0);
                        cm.shift_anchor_for_scroll(moved, false, max_row);
                    }
                }
                CopyModeScroll::None => {}
            }

            self.selection = cm.anchor.map(|(ar, ac)| TextSelection {
                target: SelectionTarget::Pane(cm.pane_id),
                start_row: ar,
                start_col: ac,
                end_row: cm.cursor_row,
                end_col: cm.cursor_col,
                content_rect: inner,
            });
            self.copy_mode = Some(cm);
            self.dirty = true;
            return Ok(true);
        }

        // Swallow everything else — copy mode must not leak keys.
        Ok(true)
    }
}

/// Extract text from a pane's vt100 screen within a selection range.
pub(crate) fn extract_selected_text(pane: &Pane, sr: u32, sc: u32, er: u32, ec: u32) -> String {
    let parser = pane.parser.lock().unwrap_or_else(|e| e.into_inner());
    extract_screen_text(parser.screen(), sr, sc, er, ec)
}

/// Selection extraction against a bare vt100 screen (split out from
/// [`extract_selected_text`] so tests don't need a live PTY).
pub(crate) fn extract_screen_text(
    screen: &vt100::Screen,
    sr: u32,
    sc: u32,
    er: u32,
    ec: u32,
) -> String {
    let mut lines = Vec::new();

    for row in sr..=er {
        let mut line = String::new();
        let col_start = if row == sr { sc } else { 0 };
        let col_end = if row == er { ec } else { 999 };

        for col in col_start..=col_end {
            if let Some(cell) = screen.cell(row as u16, col as u16) {
                // The second cell of a wide (CJK etc.) character has
                // empty contents; unlike a genuinely blank cell it must
                // not become a space.
                if cell.is_wide_continuation() {
                    continue;
                }
                let contents = cell.contents();
                if contents.is_empty() {
                    line.push(' ');
                } else {
                    line.push_str(contents);
                }
            }
        }
        lines.push(line.trim_end().to_string());
    }

    // Remove trailing empty lines
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }

    lines.join("\n")
}

/// Extract text from the file preview within a selection range.
/// `sr`/`er` are absolute line indices; `sc`/`ec` are char offsets
/// within the line (selection is stored in source coordinates so it
/// survives scrolling). Trailing empty lines are stripped.
pub(crate) fn extract_preview_selected_text(
    preview: &crate::preview::Preview,
    sr: u32,
    sc: u32,
    er: u32,
    ec: u32,
) -> String {
    let lines = &preview.lines;
    let mut out: Vec<String> = Vec::new();

    for abs_row in sr..=er {
        let idx = abs_row as usize;
        if idx >= lines.len() {
            break;
        }
        let line = &lines[idx];
        let chars: Vec<char> = line.chars().collect();

        let col_start = if abs_row == sr { sc as usize } else { 0 };
        let col_end_inclusive = if abs_row == er {
            ec as usize
        } else {
            chars.len().saturating_sub(1)
        };

        let start = col_start.min(chars.len());
        let end = (col_end_inclusive.saturating_add(1)).min(chars.len());
        let slice: String = if start < end {
            chars[start..end].iter().collect()
        } else {
            String::new()
        };
        out.push(slice);
    }

    // Strip trailing empty lines only.
    while out.last().is_some_and(|l| l.is_empty()) {
        out.pop();
    }

    out.join("\n")
}

/// Public wrapper for key_event_to_bytes (used by main.rs paste detection).
pub(crate) fn key_event_to_bytes_pub(key: &KeyEvent) -> Option<Vec<u8>> {
    key_event_to_bytes(key)
}

/// Convert a crossterm KeyEvent into bytes suitable for PTY input.
fn key_event_to_bytes(key: &KeyEvent) -> Option<Vec<u8>> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);

    match key.code {
        KeyCode::Char(c) => {
            if ctrl {
                let ctrl_byte = (c.to_ascii_lowercase() as u8)
                    .wrapping_sub(b'a')
                    .wrapping_add(1);
                if ctrl_byte <= 26 {
                    if alt {
                        // Alt+Ctrl+Char → ESC + ctrl byte
                        Some(vec![0x1b, ctrl_byte])
                    } else {
                        Some(vec![ctrl_byte])
                    }
                } else {
                    Some(c.to_string().into_bytes())
                }
            } else if alt {
                // Alt+Char → ESC + char (standard xterm behavior)
                let mut bytes = vec![0x1b];
                bytes.extend_from_slice(c.to_string().as_bytes());
                Some(bytes)
            } else {
                Some(c.to_string().into_bytes())
            }
        }
        // Alt+Enter → send newline (\n) for multi-line input in Claude Code
        KeyCode::Enter if alt => Some(vec![b'\n']),
        KeyCode::Enter => Some(vec![b'\r']),
        KeyCode::Backspace => Some(vec![0x7f]),
        KeyCode::Delete => Some(b"\x1b[3~".to_vec()),
        KeyCode::Tab => Some(vec![b'\t']),
        KeyCode::BackTab => Some(b"\x1b[Z".to_vec()),
        KeyCode::Esc => Some(vec![0x1b]),
        KeyCode::Up => Some(b"\x1b[A".to_vec()),
        KeyCode::Down => Some(b"\x1b[B".to_vec()),
        KeyCode::Right => Some(b"\x1b[C".to_vec()),
        KeyCode::Left => Some(b"\x1b[D".to_vec()),
        KeyCode::Home => Some(b"\x1b[H".to_vec()),
        KeyCode::End => Some(b"\x1b[F".to_vec()),
        KeyCode::PageUp => Some(b"\x1b[5~".to_vec()),
        KeyCode::PageDown => Some(b"\x1b[6~".to_vec()),
        KeyCode::Insert => Some(b"\x1b[2~".to_vec()),
        KeyCode::F(n) => {
            let seq = match n {
                1 => "\x1bOP",
                2 => "\x1bOQ",
                3 => "\x1bOR",
                4 => "\x1bOS",
                5 => "\x1b[15~",
                6 => "\x1b[17~",
                7 => "\x1b[18~",
                8 => "\x1b[19~",
                9 => "\x1b[20~",
                10 => "\x1b[21~",
                11 => "\x1b[23~",
                12 => "\x1b[24~",
                _ => return None,
            };
            Some(seq.as_bytes().to_vec())
        }
        _ => None,
    }
}
