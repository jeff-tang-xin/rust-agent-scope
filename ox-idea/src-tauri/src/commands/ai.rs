//! IPC commands of `ide_host::ops::ai_inline` (Cmd+K inline AI).

use crate::state::AppState;
use ide_core::{Diagnostic, DocId, IdeResult, Range, RequestId};
use ide_host::ops::ai_inline::InlineAiResult;
use tauri::{State, Window};

/// Cmd+K inline AI: generate/edit/answer/fix against the current code, using the
/// built-in OX agent model. `range` is the selection (or a zero-width caret).
#[tauri::command]
#[specta::specta]
pub async fn ai_inline(
    window: Window,
    state: State<'_, AppState>,
    request: RequestId,
    doc: DocId,
    range: Range,
    instruction: String,
    mode: String,
    diagnostics: Vec<Diagnostic>,
) -> IdeResult<InlineAiResult> {
    ide_host::ops::ai_inline::inline_ai(&state.host, window.label(), request, doc, range, instruction, mode, diagnostics)
        .await
}
