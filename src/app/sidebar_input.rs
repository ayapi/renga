use super::*;

impl App {
    pub(crate) fn clear_selection_if_preview(&mut self) {
        if matches!(
            self.selection.as_ref().map(|s| &s.target),
            Some(SelectionTarget::Preview)
        ) {
            self.selection = None;
        }
    }

    pub(crate) fn handle_rename_key(&mut self, key: KeyEvent) -> bool {
        let Some(buf) = self.rename_input.as_mut() else {
            return false;
        };
        let needs_relayout = !self.status_bar_visible;
        match key.code {
            KeyCode::Esc => {
                self.rename_input = None;
                if needs_relayout {
                    self.mark_layout_change();
                }
            }
            KeyCode::Enter => {
                let trimmed = buf.trim().to_string();
                self.ws_mut().custom_name = if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed)
                };
                self.rename_input = None;
                if needs_relayout {
                    self.mark_layout_change();
                }
            }
            KeyCode::Backspace => {
                buf.pop();
            }
            KeyCode::Char(c) => {
                if key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                {
                    return true;
                }
                if buf.chars().count() < 32 {
                    buf.push(c);
                }
            }
            _ => return true,
        }
        self.dirty = true;
        true
    }

    pub(crate) fn handle_file_tree_key(&mut self, key: KeyEvent) -> Result<bool> {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                self.ws_mut().file_tree.move_down();
                Ok(true)
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.ws_mut().file_tree.move_up();
                Ok(true)
            }
            KeyCode::Enter => {
                let path = self.ws_mut().file_tree.toggle_or_select();
                if let Some(path) = path {
                    self.clear_selection_if_preview();
                    let messages = self.messages();
                    let mut picker = self.image_picker.take();
                    self.ws_mut().preview.load(&path, picker.as_mut(), messages);
                    self.image_picker = picker;
                }
                Ok(true)
            }
            KeyCode::Char('.') => {
                self.ws_mut().file_tree.toggle_hidden();
                Ok(true)
            }
            KeyCode::Char('c') if key.modifiers == KeyModifiers::NONE => {
                self.spawn_claude_in_selected_dir(SplitDirection::Vertical)?;
                Ok(true)
            }
            KeyCode::Char('v') if key.modifiers == KeyModifiers::NONE => {
                self.spawn_claude_in_selected_dir(SplitDirection::Horizontal)?;
                Ok(true)
            }
            KeyCode::Char('h') => {
                self.ws_mut().file_tree.go_to_parent();
                Ok(true)
            }
            KeyCode::Char('l') => {
                self.ws_mut().file_tree.descend_into_selected();
                Ok(true)
            }
            KeyCode::Esc => {
                self.ws_mut().focus_target = FocusTarget::Pane;
                Ok(true)
            }
            _ => Ok(true),
        }
    }

    pub(crate) fn handle_preview_key(&mut self, key: KeyEvent) -> Result<bool> {
        match (key.modifiers, key.code) {
            (KeyModifiers::CONTROL, KeyCode::Char('w')) => {
                self.clear_selection_if_preview();
                self.ws_mut().preview.close();
                self.ws_mut().focus_target = FocusTarget::Pane;
                Ok(true)
            }
            (KeyModifiers::CONTROL, KeyCode::Char('p')) => {
                self.layout_swapped = !self.layout_swapped;
                Ok(true)
            }
            (_, KeyCode::Char('j')) | (_, KeyCode::Down) => {
                self.ws_mut().preview.scroll_down(1);
                Ok(true)
            }
            (_, KeyCode::Char('k')) | (_, KeyCode::Up) => {
                self.ws_mut().preview.scroll_up(1);
                Ok(true)
            }
            (_, KeyCode::PageDown) => {
                self.ws_mut().preview.scroll_down(20);
                Ok(true)
            }
            (_, KeyCode::PageUp) => {
                self.ws_mut().preview.scroll_up(20);
                Ok(true)
            }
            (KeyModifiers::NONE, KeyCode::Right)
            | (KeyModifiers::NONE, KeyCode::Char('l'))
            | (KeyModifiers::SHIFT, KeyCode::Right) => {
                self.ws_mut().preview.scroll_right(4);
                Ok(true)
            }
            (KeyModifiers::NONE, KeyCode::Left)
            | (KeyModifiers::NONE, KeyCode::Char('h'))
            | (KeyModifiers::SHIFT, KeyCode::Left) => {
                self.ws_mut().preview.scroll_left(4);
                Ok(true)
            }
            (KeyModifiers::NONE, KeyCode::Home) => {
                self.ws_mut().preview.h_scroll_offset = 0;
                Ok(true)
            }
            (_, KeyCode::Esc) => {
                self.ws_mut().focus_target = FocusTarget::Pane;
                Ok(true)
            }
            (KeyModifiers::CONTROL, KeyCode::Char('q')) => {
                self.should_quit = true;
                Ok(true)
            }
            (KeyModifiers::CONTROL, KeyCode::Right) => {
                self.focus_next_pane();
                Ok(true)
            }
            (KeyModifiers::CONTROL, KeyCode::Left) => {
                self.focus_prev_pane();
                Ok(true)
            }
            _ => Ok(true),
        }
    }

    /// Keys handled while [`FocusTarget::OrgSidebar`] holds focus.
    ///
    /// Split out from the file-tree / preview handlers rather than
    /// bolted onto one of them: the sidebar navigates a cross-tab row
    /// list, not a per-tab tree, and sharing a handler would mean
    /// re-deriving which panel a keystroke meant on every arm.
    ///
    /// Unmatched keys return `Ok(true)` (swallowed, like the other two
    /// panel handlers) so stray input never leaks into a PTY the user
    /// is not looking at.
    pub(crate) fn handle_org_sidebar_key(&mut self, key: KeyEvent) -> Result<bool> {
        match (key.modifiers, key.code) {
            // Mirrors the preview's Ctrl+W: close the panel rather than
            // falling through to the pane/tab close confirmation, which
            // is what an unhandled Ctrl+W here would otherwise reach.
            // (Ctrl+B never reaches here — `handle_key` takes it before
            // the per-panel dispatch so it works from any focus.)
            (KeyModifiers::CONTROL, KeyCode::Char('w')) => {
                self.toggle_org_sidebar();
                Ok(true)
            }
            (_, KeyCode::Char('j')) | (_, KeyCode::Down) => {
                self.org_sidebar_move_selection(1);
                Ok(true)
            }
            (_, KeyCode::Char('k')) | (_, KeyCode::Up) => {
                self.org_sidebar_move_selection(-1);
                Ok(true)
            }
            (_, KeyCode::PageDown) => {
                self.org_sidebar_move_selection(10);
                Ok(true)
            }
            (_, KeyCode::PageUp) => {
                self.org_sidebar_move_selection(-10);
                Ok(true)
            }
            (_, KeyCode::Home) => {
                self.org_sidebar_move_selection(isize::MIN / 2);
                Ok(true)
            }
            (_, KeyCode::End) => {
                self.org_sidebar_move_selection(isize::MAX / 2);
                Ok(true)
            }
            (_, KeyCode::Enter) => {
                self.org_sidebar_activate();
                Ok(true)
            }
            (_, KeyCode::Esc) => {
                self.ws_mut().focus_target = FocusTarget::Pane;
                Ok(true)
            }
            (KeyModifiers::CONTROL, KeyCode::Char('q')) => {
                self.should_quit = true;
                Ok(true)
            }
            (KeyModifiers::CONTROL, KeyCode::Right) => {
                self.focus_next_pane();
                Ok(true)
            }
            (KeyModifiers::CONTROL, KeyCode::Left) => {
                self.focus_prev_pane();
                Ok(true)
            }
            _ => Ok(true),
        }
    }

    /// Ctrl+B. Same three-way shape as [`Self::toggle_file_tree`]:
    /// visible+focused closes, visible+unfocused just takes focus,
    /// hidden opens and focuses.
    ///
    /// A no-op when `[ui] org_sidebar = off`. Note that the *key* is
    /// released back to the PTY by the dispatcher in `handle_key`, which
    /// declines to consume Ctrl+B unless the feature is enabled —
    /// returning early here only guards direct callers, and on its own
    /// would leave the keystroke swallowed. `off` is the documented
    /// escape hatch for readline / vim / nested tmux, the same way
    /// `[ime] mode = off` frees Ctrl+;.
    pub(crate) fn toggle_org_sidebar(&mut self) {
        if !self.org_sidebar_enabled() {
            return;
        }
        let was_visible = self.org_sidebar_visible;
        let focused = self.ws().focus_target == FocusTarget::OrgSidebar;

        if was_visible && focused {
            self.org_sidebar_visible = false;
            self.ws_mut().focus_target = FocusTarget::Pane;
        } else if was_visible {
            self.ws_mut().focus_target = FocusTarget::OrgSidebar;
        } else {
            self.org_sidebar_visible = true;
            self.ws_mut().focus_target = FocusTarget::OrgSidebar;
        }

        if was_visible != self.org_sidebar_visible {
            self.mark_layout_change();
        }
        self.dirty = true;
    }

    /// Move focus to the file tree, but only if it will actually be
    /// painted. On a terminal too narrow to fit it, Ctrl+F still flips
    /// the flag on (so the tree reappears when the window grows) —
    /// handing focus to a panel with no cells on screen would just
    /// swallow the user's next keystrokes.
    fn take_file_tree_focus(&mut self) {
        if self.file_tree_painted() {
            self.ws_mut().focus_target = FocusTarget::FileTree;
        }
    }

    pub(crate) fn toggle_file_tree(&mut self) {
        // In `replace` mode the sidebar occupies the tree's slot, so a
        // tab whose `file_tree_visible` flag is still set can have no
        // tree on screen. Branch on what is actually painted, not on
        // the raw flag — otherwise Ctrl+F would "focus" a tree the user
        // cannot see and swallow every subsequent keystroke.
        let suppressed_by_sidebar = self.org_sidebar_mode == crate::config::OrgSidebarMode::Replace
            && self.org_sidebar_visible;
        let sidebar_was_visible = self.org_sidebar_visible;
        // The *slot* predicate, not `file_tree_painted`: a tree the
        // degrade ladder squeezed out is still logically on, and Ctrl+F
        // should turn it off rather than "re-open" something that never
        // closed. `replace` mode is different — there the user really
        // did give the slot away.
        let showing = self.file_tree_slot_available();

        if showing && self.ws().focus_target == FocusTarget::FileTree {
            let ws = self.ws_mut();
            ws.file_tree_visible = false;
            ws.focus_target = if ws.preview.is_active() {
                FocusTarget::Preview
            } else {
                FocusTarget::Pane
            };
        } else if showing {
            self.take_file_tree_focus();
        } else {
            // Opening the tree in `replace` mode hands the slot back
            // from the sidebar.
            if suppressed_by_sidebar {
                self.org_sidebar_visible = false;
            }
            self.ws_mut().file_tree_visible = true;
            self.take_file_tree_focus();
        }

        let now_showing = self.file_tree_slot_available();
        if showing != now_showing || sidebar_was_visible != self.org_sidebar_visible {
            self.mark_layout_change();
        }
    }
}
