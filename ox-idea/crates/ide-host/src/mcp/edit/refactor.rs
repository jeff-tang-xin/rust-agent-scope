//! `refactor_code_action`: the language server's semantic refactorings (extract
//! method/variable, inline, change signature, …) as a two-step tool — list what is
//! available at a position, then apply the chosen one. Backed by LSP code actions
//! filtered to `refactor.*` kinds, so the server updates every reference instead of
//! the agent hand-editing text. Quick fixes are out of scope here.

use crate::mcp::tool::{Call, Output, Param, ParamType, Tool};
use ide_core::{IdeError, IdeResult, Pos, Range};
use ide_lsp::types::CodeAction;
use serde_json::json;
use std::time::Duration;

/// Only refactorings, not quick fixes or source actions.
fn refactor_only(actions: Vec<CodeAction>) -> Vec<CodeAction> {
    actions.into_iter().filter(|a| a.kind.as_deref().is_some_and(|k| k.starts_with("refactor"))).collect()
}

pub(super) fn refactor_code_action() -> Tool {
    Tool::new(
        "refactor_code_action",
        "List and apply the language server's semantic refactorings at a position — extract method/variable, \
inline, change signature, and other refactor.* actions the server offers. Two steps: call without actionIndex \
to list the available actions (index + title + kind), then call again with the same position and actionIndex to \
apply one. To refactor a selection (e.g. extract an expression), pass endLine/endColumn too. Prefer this over \
hand-editing for structural refactorings: the server updates every reference. Only refactorings are offered \
(not quick fixes); an empty list means the server has no refactoring at that position. Applying asks the user first.",
        |call| async move { run(&call).await },
    )
    .write()
    .param(Param::required("pathInProject", ParamType::String, "Path relative to the project root"))
    .param(Param::required("line", ParamType::Integer, "1-based line of the code to refactor"))
    .param(Param::required("column", ParamType::Integer, "1-based UTF-16 column"))
    .param(Param::optional("endLine", ParamType::Integer, "Optional 1-based end line of a selection (with endColumn)"))
    .param(Param::optional("endColumn", ParamType::Integer, "Optional 1-based end UTF-16 column of a selection"))
    .param(Param::optional(
        "actionIndex",
        ParamType::Integer,
        "1-based pick from the actions list an earlier call returned; omit to list the available actions",
    ))
    .param(Param::optional(
        "kind",
        ParamType::String,
        "Optional kind filter, e.g. refactor.extract or refactor.inline; omit for all refactorings",
    ))
}

async fn run(call: &Call) -> IdeResult<Output> {
    let path = call.resolve_readable(&call.str("pathInProject")?).await?;
    let line = call.opt_i64("line")?.ok_or_else(|| IdeError::invalid("missing line"))?;
    let col = call.opt_i64("column")?.ok_or_else(|| IdeError::invalid("missing column"))?;
    let action_index = call.opt_i64("actionIndex")?;
    let kind = call.opt_str("kind")?;

    let project = call.project.clone();
    let p = path.clone();
    struct Document(std::sync::Arc<crate::Project>, ide_core::DocId);
    impl Drop for Document {
        fn drop(&mut self) {
            self.0.documents.close(self.1);
        }
    }
    let (text, document) = call
        .host
        .blocking(move || {
            let text = project.text_of(&p)?;
            let doc = project.documents.open(&p)?;
            Ok((text, Document(project, doc.id)))
        })
        .await?;
    let doc = document.1;

    let start = point(&text, line, col)?;
    let range = match (call.opt_i64("endLine")?, call.opt_i64("endColumn")?) {
        (Some(el), Some(ec)) => Range::new(start, point(&text, el, ec)?),
        _ => Range::point(start),
    };

    let only = kind.map(|k| vec![k]).unwrap_or_default();
    let work = call.project.lsp.code_action_query(doc, range, only, call.cancel.clone());
    let query = tokio::select! {
        r = tokio::time::timeout(Duration::from_secs(20), work) => {
            r.map_err(|_| IdeError::unavailable("The language server did not answer in time; retry later."))??
        }
        _ = call.cancel.cancelled() => return Err(IdeError::cancelled()),
    };
    let actions = refactor_only(query.actions);

    // Step 1: list.
    let Some(index) = action_index else {
        let list: Vec<_> = actions
            .iter()
            .enumerate()
            .map(|(i, a)| json!({"index": i + 1, "title": a.title, "kind": a.kind}))
            .collect();
        return Output::json(json!({
            "actions": list,
            "note": if list.is_empty() { "No refactorings available at this position." } else { "Call again with actionIndex to apply one." },
        }));
    };

    // Step 2: apply the chosen action.
    if index < 1 || index as usize > actions.len() {
        return Err(IdeError::invalid(format!(
            "actionIndex {index} is out of range; {} action(s) available. Call without actionIndex to list them.",
            actions.len()
        )));
    }
    let chosen = actions[index as usize - 1].clone();
    if let Some(reason) = &chosen.disabled_reason {
        return Err(IdeError::invalid(format!("The action is disabled: {reason}")));
    }
    let resolved = call.project.lsp.resolve_code_action(doc, chosen).await?;
    let Some(edit) = resolved.edit else {
        return Err(IdeError::unavailable(
            "This action produces no workspace edit (it may need an interactive command); apply it in the editor instead.",
        ));
    };
    for f in &edit.files {
        let paths: Vec<&std::path::Path> = match f {
            ide_core::FileEdit::Edit { path, .. }
            | ide_core::FileEdit::Create { path, .. }
            | ide_core::FileEdit::Delete { path, .. } => vec![path],
            ide_core::FileEdit::Rename { from, to } => vec![from, to],
        };
        for p in paths {
            call.check_writable(p).await?;
        }
    }
    call.confirm("refactor_code_action", Some(resolved.title.clone())).await?;
    crate::ops::lsp::apply_workspace_edit(&call.host, call.window(), edit).await?;
    Output::json(json!({"applied": true, "title": resolved.title}))
}

/// Validates a 1-based line / UTF-16 column against the text and returns a 0-based [`Pos`].
fn point(text: &str, line: i64, col: i64) -> IdeResult<Pos> {
    if line < 1 || line > u32::MAX as i64 {
        return Err(IdeError::invalid("line is out of bounds"));
    }
    let line_text = usize::try_from(line - 1)
        .ok()
        .and_then(|l| text.split('\n').nth(l))
        .ok_or_else(|| IdeError::invalid("Line number is out of bounds of the document"))?;
    if col < 1 || col as usize > line_text.encode_utf16().count() + 1 {
        return Err(IdeError::invalid("Column number is out of bounds of the document"));
    }
    Ok(Pos::new((line - 1) as u32, (col - 1) as u32))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn action(kind: Option<&str>) -> CodeAction {
        CodeAction {
            title: "t".into(),
            kind: kind.map(Into::into),
            is_preferred: false,
            disabled_reason: None,
            edit: None,
            command: None,
            data: None,
            server: ide_core::ServerId(1),
        }
    }

    #[test]
    fn only_refactor_kinds_are_kept() {
        let kept = refactor_only(vec![
            action(Some("refactor.extract")),
            action(Some("quickfix")),
            action(Some("refactor.inline")),
            action(Some("source.organizeImports")),
            action(None),
        ]);
        assert_eq!(kept.len(), 2);
        assert_eq!(kept[0].kind.as_deref(), Some("refactor.extract"));
        assert_eq!(kept[1].kind.as_deref(), Some("refactor.inline"));
    }

    #[test]
    fn point_validates_bounds() {
        let text = "abc\ndef";
        assert_eq!(point(text, 1, 1).unwrap(), Pos::new(0, 0));
        assert_eq!(point(text, 2, 4).unwrap(), Pos::new(1, 3));
        assert!(point(text, 0, 1).is_err());
        assert!(point(text, 3, 1).is_err());
        assert!(point(text, 1, 5).is_err());
    }
}
