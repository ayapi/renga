use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;

mod app_core;
mod app_state;
mod codex_peer;
pub(crate) mod frame_diagnostics;
mod ipc_handlers;
mod keyboard_input;
mod layout_ops;
mod layout_tree;
mod pointer_input;
mod runtime;
mod selection;
mod sidebar_input;
mod workspace_state;

#[cfg(test)]
pub(crate) use self::app_state::set_ipc_enqueue_timing_test_override;
pub(crate) use self::app_state::{
    with_ipc_enqueue_timing, AppCommandTiming, SplitOutcome, CLAUDE_PEER_LAUNCH_CMD,
};
pub use self::app_state::{App, AppCommand, AppEvent};
#[cfg(test)]
pub(crate) use self::codex_peer::set_codex_peer_debug_log_path_test_override;
pub(crate) use self::codex_peer::{
    append_codex_peer_debug_record, codex_peer_debug_log_path, take_codex_peer_debug_write_failures,
};
#[cfg(test)]
use self::codex_peer::{
    codex_prompt_allows_peer_nudge_on_screen, format_codex_peer_message, screen_tail_lines,
    PendingCodexPeerMessage,
};
use self::codex_peer::{
    write_input_to_pane, CodexPeerDebugObservation, CodexPeerNotificationState, PeerHandover,
    PendingCodexPeerDelivery, PendingPeerInboxDelivery, PendingPeerInboxMessage,
};
pub(crate) use self::keyboard_input::key_event_to_bytes_pub;
use self::keyboard_input::{extract_preview_selected_text, extract_selected_text};
use self::layout_ops::{
    default_command_for_role, dir_name, resolve_optional_cwd, strip_verbatim_prefix,
};
pub use self::layout_tree::{LayoutNode, SplitDirection};
#[cfg(test)]
use self::pointer_input::{
    detect_outer_edge, detect_shared_boundary, mouse_forward_disabled, pane_local_coords,
    pane_local_coords_clamped, split_intent_for_edge, EdgeSide,
};
pub use self::selection::{CopyModeScroll, CopyModeState, SelectionTarget, TextSelection};
#[cfg(test)]
use self::workspace_state::resolve_pane_ref_impl;
pub(crate) use self::workspace_state::{pane_content_rect, pane_scrollbar_col};
pub use self::workspace_state::{DragTarget, FocusTarget, Workspace};
use crate::filetree::FileTree;
use crate::ipc::{self, PaneInfo, PaneRef, PeerClientKind, PeerInfo};
use crate::layout_config::{DirectionSpec, LayoutConfig, LayoutNodeSpec};
use crate::pane::{Pane, PointerAction, PointerButton};
use crate::preview::Preview;

// ─── IME composition overlay ──────────────────────────────
//
// `OverlayState` and its modal key handler (`handle_overlay_key`) live
// in [`crate::input::overlay`] — first slice of Issue #66. Re-exported
// here so downstream code can keep referring to `crate::app::OverlayState`
// during the rest of the #66 rollout.
pub use crate::input::overlay::OverlayState;
#[cfg(test)]
mod tests;
