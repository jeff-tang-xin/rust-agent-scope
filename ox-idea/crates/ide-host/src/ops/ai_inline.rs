//! Cmd+K 行内浮动 AI（不离开编辑器）。
//!
//! 与侧栏 Agent（复杂任务）分工：本模块面向**简单任务**——就着当前代码答疑、
//! 读懂代码、光标生成、选区编辑、修复诊断。复用内置 OX agent（`ox-hareness`）
//! 的模型配置（其 `AGENTSCOPE_*` env），推理模型在此场景合适（用户显式触发、
//! 可等待）。上下文（imports + 外层声明 + 跨文件相关声明 + 诊断）只用**活文本
//! 轻量扫描**与前端传入的诊断，不查 LSP/索引，不给 jdtls 加负载。
//!
//! 前端浮动框（`inlineAi.tsx`，仿 VCS HunkPopup）消费 `InlineAiResult`：
//! `kind="code"` 做 diff 预览 + 接受/拒绝；`kind="answer"` 展示答疑文本。

use crate::Host;
use agentscope_core::{ChatRequest, ChatResponse, ContentBlock, GenerateOptions, Model, Msg};
use ide_core::{Diagnostic, DocId, IdeError, IdeResult, Range, RequestId};
use serde::{Deserialize, Serialize};
use specta::Type;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

/// 携带模型配置的内置 OX agent id。
const OX_AGENT: &str = "ox-hareness";
/// 发给模型的代码上下文字符上限。
const CONTEXT_CHARS: usize = 6000;
/// 语义上下文（imports/外层声明/相关声明）字符上限。
const SEMANTIC_CHARS: usize = 2000;
/// 用户显式触发，可等待；给一个有界超时。
const INLINE_TIMEOUT: Duration = Duration::from_secs(30);
/// 输出 token 上限：推理模型留足「思考 + 代码」空间（天花板非目标）。
const INLINE_MAX_TOKENS: u32 = 4096;
/// 最多扫描的其它打开文档数。
const MAX_SNIPPET_DOCS: usize = 8;
/// 跨文件相关声明条数上限。
const MAX_SNIPPETS: usize = 5;
/// 跨文件相关声明字符上限。
const SNIPPET_CHARS: usize = 400;

/// 行内 AI 模式。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// 替换选区代码。
    Edit,
    /// 光标处插入代码。
    Generate,
    /// 答疑/读懂代码（不改文件）。
    Ask,
    /// 修复诊断。
    Fix,
}

impl Mode {
    fn parse(s: &str) -> IdeResult<Self> {
        match s {
            "edit" => Ok(Self::Edit),
            "generate" => Ok(Self::Generate),
            "ask" => Ok(Self::Ask),
            "fix" => Ok(Self::Fix),
            _ => Err(IdeError::invalid(format!("unknown inline AI mode: {s}"))),
        }
    }
}

/// 行内 AI 结果：`kind="code"` 带 `code`（前端 diff 预览 + 应用）；
/// `kind="answer"` 带 `text`（前端展示答疑）。
#[derive(Clone, Debug, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct InlineAiResult {
    pub kind: String,
    pub code: Option<String>,
    pub text: Option<String>,
}

/// 内置 OX agent 的模型配置 env（`AGENTSCOPE_*`）；无该 agent → None。
fn ox_env(host: &Host, window: &str) -> Option<BTreeMap<String, String>> {
    let providers = host.project(window).ok()?.agents.providers();
    Some(providers.iter().find(|p| p.id == OX_AGENT)?.env.clone())
}

/// 保留尾部至多 `max` 个字符（按字符边界，不切断 UTF-8）。
fn clip_tail(s: &str, max: usize) -> &str {
    let total = s.chars().count();
    if total <= max {
        return s;
    }
    let skip = total - max;
    let idx = s.char_indices().nth(skip).map(|(i, _)| i).unwrap_or(0);
    &s[idx..]
}

/// 保留头部至多 `max` 个字符（按字符边界）。
fn clip_head(s: &str, max: usize) -> &str {
    match s.char_indices().nth(max) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

/// 一行是否是 import/include/use 等导入声明（跨语言启发式）。
fn is_import(line: &str) -> bool {
    ["use ", "import ", "from ", "#include", "require(", "package "].iter().any(|p| line.starts_with(p))
}

/// 一行是否是函数/类型等声明（跨语言启发式，剥常见修饰符后看关键字）。
fn is_declaration(line: &str) -> bool {
    let mut t = line;
    loop {
        let before = t;
        for m in [
            "pub ", "pub(crate) ", "async ", "export ", "default ", "static ", "final ", "abstract ", "private ",
            "protected ", "public ", "open ", "override ",
        ] {
            t = t.strip_prefix(m).unwrap_or(t);
        }
        if t == before {
            break;
        }
    }
    ["fn ", "func ", "def ", "function ", "class ", "struct ", "interface ", "enum ", "impl ", "trait ", "type ", "sub ", "method "]
        .iter()
        .any(|k| t.starts_with(k))
}

/// 活文本轻量语义上下文：导入行（最多 24 条）+ 光标外层声明。不查 LSP/索引。
fn semantic_context(text: &str, caret_byte: usize) -> (Vec<String>, Option<String>) {
    let imports: Vec<String> =
        text.lines().map(str::trim_start).filter(|l| is_import(l)).take(24).map(str::to_string).collect();
    let enclosing =
        text[..caret_byte].lines().rev().map(str::trim_start).find(|l| is_declaration(l)).map(str::to_string);
    (imports, enclosing)
}

/// 前缀尾部值得跨文件查找的标识符。
fn lookup_tokens(prefix: &str) -> Vec<String> {
    let line = prefix.lines().last().unwrap_or("");
    let mut tokens: Vec<String> = line
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|t| t.chars().count() >= 3)
        .map(str::to_string)
        .collect();
    tokens.dedup();
    tokens.into_iter().rev().take(4).collect()
}

/// 从其它打开文档的活文本里找与标识符匹配的声明行（不查 LSP/索引）。
fn related_declarations(texts: &[String], tokens: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if tokens.is_empty() {
        return out;
    }
    let mut chars = 0usize;
    for text in texts {
        for line in text.lines() {
            let t = line.trim_start();
            if !is_declaration(t) || !tokens.iter().any(|tok| t.contains(tok.as_str())) {
                continue;
            }
            let snippet = clip_head(t, 160).to_string();
            if out.contains(&snippet) {
                continue;
            }
            chars += snippet.chars().count();
            out.push(snippet);
            if out.len() >= MAX_SNIPPETS || chars >= SNIPPET_CHARS {
                return out;
            }
        }
    }
    out
}

/// 剥掉模型回复可能包裹的 markdown 围栏（```lang … ```）。
fn strip_fences(text: &str) -> &str {
    let t = text.trim();
    let Some(rest) = t.strip_prefix("```") else { return t };
    let body = match rest.find('\n') {
        Some(i) => &rest[i + 1..],
        None => rest,
    };
    body.strip_suffix("```").unwrap_or(body).trim()
}

/// 从 chat 回复提取首个文本块；空 → None。
fn response_text(response: &ChatResponse) -> Option<String> {
    let text = response.content.iter().find_map(|b| match b {
        ContentBlock::Text { text } => Some(text.as_str()),
        _ => None,
    })?;
    let t = text.trim();
    if t.is_empty() { None } else { Some(t.to_string()) }
}

/// 厂商特有参数透传（关思考等）：`AGENTSCOPE_INLINE_EXTRA_BODY`（JSON 对象）。
/// OpenAI 兼容生态此类参数碎片化，由用户按端点配置；缺失/非法/非对象 → None。
fn inline_extra_body(env: &BTreeMap<String, String>) -> Option<serde_json::Value> {
    let raw = env.get("AGENTSCOPE_INLINE_EXTRA_BODY")?.trim();
    if raw.is_empty() {
        return None;
    }
    match serde_json::from_str::<serde_json::Value>(raw) {
        Ok(v) if v.is_object() => Some(v),
        _ => None,
    }
}

/// 按模式组装提示：system 角色 + user（语言/指令/语义上下文/代码/诊断）。
fn inline_messages(
    mode: Mode,
    language: &str,
    instruction: &str,
    file_text: &str,
    caret_byte: usize,
    sel_text: &str,
    imports: &[String],
    enclosing: Option<&str>,
    snippets: &[String],
    diagnostics: &[Diagnostic],
) -> Vec<Msg> {
    let system = match mode {
        Mode::Edit => "You are an inline code editor embedded in an IDE. The user selected code and gives an instruction. Output ONLY the replacement code for the selection, preserving the surrounding indentation. No markdown fences, no explanation.",
        Mode::Generate => "You are an inline code generator embedded in an IDE. Output ONLY the code to insert at the cursor, consistent with the surrounding style. No markdown fences, no explanation.",
        Mode::Fix => "You are an inline code fixer embedded in an IDE. Fix the reported problems. Output ONLY the corrected code (for the selection if given, otherwise the whole file). No markdown fences, no explanation.",
        Mode::Ask => "You are a code assistant embedded in an IDE. Answer the user's question about the code concisely and accurately. You may use markdown. Do not output unrelated code.",
    };
    let mut ctx = String::new();
    if !imports.is_empty() {
        ctx.push_str("Relevant imports:\n");
        for i in imports {
            ctx.push_str(i);
            ctx.push('\n');
        }
    }
    if let Some(e) = enclosing {
        ctx.push_str("Enclosing declaration:\n");
        ctx.push_str(e);
        ctx.push('\n');
    }
    if !snippets.is_empty() {
        ctx.push_str("Related declarations:\n");
        for s in snippets {
            ctx.push_str(s);
            ctx.push('\n');
        }
    }
    let ctx = clip_head(&ctx, SEMANTIC_CHARS);
    let mut user = format!("Language: {language}\nInstruction: {instruction}\n{ctx}");
    if !diagnostics.is_empty() && matches!(mode, Mode::Fix | Mode::Ask) {
        user.push_str("Reported problems:\n");
        for d in diagnostics.iter().take(20) {
            user.push_str(&format!("- line {}: {}\n", d.range.start.line + 1, d.message));
        }
    }
    if mode == Mode::Ask {
        let code = if sel_text.is_empty() { clip_head(file_text, CONTEXT_CHARS) } else { sel_text };
        user.push_str(&format!("<code>\n{code}\n</code>\nAnswer the instruction about this code."));
    } else if !sel_text.is_empty() {
        let after_byte = caret_byte + sel_text.len();
        let before = clip_tail(&file_text[..caret_byte], 1500);
        let after = clip_head(&file_text[after_byte..], 1500);
        user.push_str(&format!(
            "<selected_code>\n{sel_text}\n</selected_code>\nSurrounding before:\n{before}\nSurrounding after:\n{after}\nOutput only the replacement code for the selection."
        ));
    } else if mode == Mode::Fix {
        let code = clip_head(file_text, CONTEXT_CHARS);
        user.push_str(&format!("<file>\n{code}\n</file>\nOutput the corrected whole file."));
    } else {
        let before = clip_tail(&file_text[..caret_byte], CONTEXT_CHARS);
        let after = clip_head(&file_text[caret_byte..], CONTEXT_CHARS);
        user.push_str(&format!(
            "<code_before_cursor>\n{before}\n</code_before_cursor>\n<code_after_cursor>\n{after}\n</code_after_cursor>\nOutput only the code to insert at the cursor."
        ));
    }
    vec![Msg::system("oxidea-inline-ai", system), Msg::user("oxidea-inline-ai", user)]
}

/// Cmd+K 行内 AI：按模式就着当前代码生成/编辑/答疑/修复。
pub async fn inline_ai(
    host: &Host,
    window: &str,
    request: RequestId,
    doc: DocId,
    range: Range,
    instruction: String,
    mode: String,
    diagnostics: Vec<Diagnostic>,
) -> IdeResult<InlineAiResult> {
    let mode = Mode::parse(&mode)?;
    let instruction = instruction.trim().to_string();
    if instruction.is_empty() {
        return Err(IdeError::invalid("empty instruction"));
    }
    let env = ox_env(host, window).ok_or_else(|| IdeError::unavailable("OX agent model is not configured"))?;
    let project = host.project(window)?;
    let snap = project.documents.snapshot(doc)?;
    let text = ide_text::Document::new(&snap.text);
    let caret_byte = text.offset_to_byte(text.pos_to_offset(range.start));
    let end_byte = text.offset_to_byte(text.pos_to_offset(range.end));
    let sel_text = if end_byte > caret_byte { snap.text[caret_byte..end_byte].to_string() } else { String::new() };
    let (imports, enclosing) = semantic_context(&snap.text, caret_byte);
    let snippets = {
        let tokens = lookup_tokens(&snap.text[..caret_byte]);
        let mut texts = Vec::new();
        for id in project.documents.open_documents() {
            if texts.len() >= MAX_SNIPPET_DOCS {
                break;
            }
            if id == doc {
                continue;
            }
            if let Ok(s) = project.documents.text_snapshot(id) {
                if !s.large {
                    texts.push(s.text);
                }
            }
        }
        related_declarations(&texts, &tokens)
    };
    let messages = inline_messages(
        mode, &snap.language.0, &instruction, &snap.text, caret_byte, &sel_text, &imports, enclosing.as_deref(), &snippets, &diagnostics,
    );
    let pairs: Vec<(String, String)> = env.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    let model: Arc<dyn Model> =
        agentscope_acp::session::build_model(&pairs).map_err(|e| IdeError::unavailable(format!("model unavailable: {e}")))?;
    let chat = ChatRequest {
        messages,
        options: GenerateOptions {
            temperature: Some(0.0),
            max_tokens: Some(INLINE_MAX_TOKENS),
            extra_body: inline_extra_body(&env),
            ..Default::default()
        },
    };
    let cancel = host.begin_request(request);
    let response = tokio::select! {
        _ = cancel.cancelled() => {
            host.end_request(request);
            return Err(IdeError::cancelled());
        }
        r = tokio::time::timeout(INLINE_TIMEOUT, model.call(chat)) => {
            host.end_request(request);
            r.map_err(|_| IdeError::unavailable("inline AI timed out"))?
                .map_err(|e| IdeError::internal(format!("model call failed: {e}")))?
        }
    };
    let raw = response_text(&response).ok_or_else(|| IdeError::internal("model returned no text"))?;
    if mode == Mode::Ask {
        Ok(InlineAiResult { kind: "answer".into(), code: None, text: Some(raw) })
    } else {
        let code = strip_fences(&raw).trim_end().to_string();
        if code.is_empty() {
            return Err(IdeError::internal("model returned empty code"));
        }
        Ok(InlineAiResult { kind: "code".into(), code: Some(code), text: None })
    }
}

pub(crate) fn register(d: &mut crate::rpc::Dispatcher) {
    d.add("ai_inline", |c| async move {
        inline_ai(
            &c.host,
            &c.window,
            c.arg("request")?,
            c.arg("doc")?,
            c.arg("range")?,
            c.arg("instruction")?,
            c.arg("mode")?,
            c.arg("diagnostics")?,
        )
        .await
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use ide_core::{Pos, Severity};

    #[test]
    fn mode_parse_rejects_unknown() {
        assert_eq!(Mode::parse("edit").unwrap(), Mode::Edit);
        assert_eq!(Mode::parse("generate").unwrap(), Mode::Generate);
        assert_eq!(Mode::parse("ask").unwrap(), Mode::Ask);
        assert_eq!(Mode::parse("fix").unwrap(), Mode::Fix);
        assert!(Mode::parse("bogus").is_err());
    }

    #[test]
    fn strip_fences_removes_language_fence() {
        assert_eq!(strip_fences("```java\nint x = 1;\n```"), "int x = 1;");
        assert_eq!(strip_fences("plain"), "plain");
    }

    #[test]
    fn edit_prompt_carries_selection_and_instruction() {
        let msgs = inline_messages(
            Mode::Edit, "java", "make it return json", "class A {\n  void m() {}\n}", 10, "void m() {}", &[], None, &[], &[],
        );
        assert_eq!(msgs.len(), 2);
        let user = msgs[1].text();
        assert!(user.contains("Instruction: make it return json"));
        assert!(user.contains("<selected_code>"));
        assert!(user.contains("void m() {}"));
    }

    #[test]
    fn generate_prompt_uses_before_after_cursor() {
        let msgs = inline_messages(Mode::Generate, "rust", "add a test", "fn main() {\n", 12, "", &[], None, &[], &[]);
        let user = msgs[1].text();
        assert!(user.contains("<code_before_cursor>"));
        assert!(user.contains("<code_after_cursor>"));
    }

    #[test]
    fn ask_prompt_uses_code_context() {
        let msgs = inline_messages(Mode::Ask, "rust", "what does this do", "fn main() {}", 0, "", &[], None, &[], &[]);
        let user = msgs[1].text();
        assert!(user.contains("<code>"));
        assert!(user.contains("fn main() {}"));
    }

    #[test]
    fn fix_prompt_lists_problems() {
        let d = Diagnostic {
            range: Range::point(Pos::new(2, 0)),
            severity: Severity::Error,
            message: "cannot find symbol".into(),
            source: None,
            code: None,
            unnecessary: false,
            deprecated: false,
            server: None,
        };
        let msgs = inline_messages(Mode::Fix, "java", "fix", "class A {}", 0, "", &[], None, &[], &[d]);
        let user = msgs[1].text();
        assert!(user.contains("Reported problems:"));
        assert!(user.contains("line 3: cannot find symbol"));
    }

    #[test]
    fn extra_body_object_only() {
        let mut env = BTreeMap::new();
        assert_eq!(inline_extra_body(&env), None);
        env.insert("AGENTSCOPE_INLINE_EXTRA_BODY".into(), "{\"enable_thinking\":false}".into());
        assert_eq!(inline_extra_body(&env), Some(serde_json::json!({"enable_thinking": false})));
        env.insert("AGENTSCOPE_INLINE_EXTRA_BODY".into(), "nope".into());
        assert_eq!(inline_extra_body(&env), None);
    }
}
