//! AI inline completion（fill-in-middle）。
//!
//! 与 LSP 服务器、插件 provider 并列的第三个补全来源。复用内置 OX agent
//! （`ox-hareness`）的模型配置（其 `AGENTSCOPE_*` env）。两条路径：
//! - **路B 专用 FIM**：配置 `AGENTSCOPE_FIM_MODEL`（+可选 `AGENTSCOPE_FIM_BASE_URL`/
//!   `AGENTSCOPE_FIM_STYLE`）时，走 OpenAI 兼容 `/completions` 的 FIM token 快路径
//!   （Qwen2.5-Coder/StarCoder/CodeLlama 等代码模型，延迟低）。
//! - **路A chat-FIM 回退**：未配 FIM 或快路径失败时，用现有 chat provider 以
//!   prefix/suffix 提示模拟 FIM（语义上下文增强）。
//!
//! 语义上下文（imports + 外层声明 + 跨文件相关声明）只用**活文本轻量扫描**：
//! 跨文件片段取自其它打开文档的活文本（按前缀尾部标识符匹配声明行，有界），
//! 不查 LSP/索引——补全路径不给 jdtls 加负载。结果再做后处理（去掉与后缀重叠的
//! 尾部，防接受后重复）。未配置/失败/超时/取消 → `None`，回退 LSP，零回归。
//! 前端 ghost-text 管线（`inlineCompletion.ts`）照常消费 `InlineCompletionItem`，无需改动。

use crate::Host;
use agentscope_core::{ChatRequest, ChatResponse, ContentBlock, GenerateOptions, Model, Msg};
use ide_core::{CancellationToken, DocId, Pos, ServerId};
use ide_lsp::types::InlineCompletionItem;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

/// AI 建议的 server 标识（区别于真实 LSP server 与 `PLUGIN_SERVER = 0`）。
const AI_SERVER: ServerId = ServerId(u32::MAX);
/// 携带模型配置的内置 OX agent id。
const OX_AGENT: &str = "ox-hareness";
/// 发给模型的前/后缀字符上限（延迟与成本）。
const CONTEXT_CHARS: usize = 4000;
/// 语义上下文（imports/外层声明/相关声明）字符上限。
const SEMANTIC_CHARS: usize = 2000;
/// chat FIM 比 LSP 慢，单独给一个有界超时。
const AI_TIMEOUT: Duration = Duration::from_secs(8);
/// 专用 FIM 快路径超时（代码模型应快得多）。
const FIM_TIMEOUT: Duration = Duration::from_secs(3);
/// 补全输出 token 上限（行内补全宜短）。
const MAX_COMPLETION_TOKENS: u32 = 256;

const SYSTEM_PROMPT: &str = "You are an inline code completion engine. You receive the code before \
and after the cursor and output ONLY the code that fills the gap at the cursor. No explanations, \
no markdown code fences, and do not repeat the surrounding code. Keep the completion short \
(typically one line or a few lines) and consistent with the surrounding style.";

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
            "pub ",
            "pub(crate) ",
            "async ",
            "export ",
            "default ",
            "static ",
            "final ",
            "abstract ",
            "private ",
            "protected ",
            "public ",
            "open ",
            "override ",
        ] {
            t = t.strip_prefix(m).unwrap_or(t);
        }
        if t == before {
            break;
        }
    }
    [
        "fn ",
        "func ",
        "def ",
        "function ",
        "class ",
        "struct ",
        "interface ",
        "enum ",
        "impl ",
        "trait ",
        "type ",
        "sub ",
        "method ",
    ]
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

/// 把前缀里缺失的导入行补到 FIM 前缀头部（已在窗口内的不重复）。
fn augment_prefix(prefix: &str, imports: &[String]) -> String {
    let extra: Vec<&str> = imports.iter().filter(|i| !prefix.contains(i.as_str())).map(String::as_str).collect();
    if extra.is_empty() {
        return prefix.to_string();
    }
    format!("{}\n{}", extra.join("\n"), prefix)
}

/// 跨文件相关声明的字符上限（活文本扫描，不查 LSP）。
const SNIPPET_CHARS: usize = 400;
/// 跨文件相关声明条数上限。
const MAX_SNIPPETS: usize = 5;
/// 最多扫描的其它打开文档数（控制补全热路径开销）。
const MAX_SNIPPET_DOCS: usize = 8;

/// 前缀尾部值得跨文件查找的标识符（光标正在输入的词）。
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

/// 从其它打开文档的活文本里找与标识符匹配的声明行（仿 Tabby 的本地相关片段，不查 LSP/索引）。
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

/// 语言的行注释符（把相关声明作为注释上下文喂给 FIM 模型）。
fn comment_token(language: &str) -> &'static str {
    match language {
        "python" | "ruby" | "shell" | "bash" | "yaml" | "toml" | "perl" | "r" => "#",
        "lua" | "sql" | "haskell" => "--",
        _ => "//",
    }
}

/// 把相关声明格式化为注释头（路B FIM 前缀用）；空 → 空串。
fn snippet_header(snippets: &[String], language: &str) -> String {
    if snippets.is_empty() {
        return String::new();
    }
    let c = comment_token(language);
    let mut s = format!("{c} Related declarations:\n");
    for sn in snippets {
        s.push_str(c);
        s.push(' ');
        s.push_str(sn);
        s.push('\n');
    }
    s
}

/// 去掉补全尾部与后缀头部最长重叠（模型回显后缀时，避免接受后出现重复代码）。
fn trim_suffix_overlap<'a>(completion: &'a str, suffix: &str) -> &'a str {
    let comp: Vec<char> = completion.chars().collect();
    let suf: Vec<char> = suffix.chars().take(comp.len()).collect();
    let max = comp.len().min(suf.len());
    let mut best = 0;
    for k in (1..=max).rev() {
        if comp[comp.len() - k..] == suf[..k] {
            best = k;
            break;
        }
    }
    if best == 0 {
        return completion;
    }
    let keep_bytes: usize = comp[..comp.len() - best].iter().map(|c| c.len_utf8()).sum();
    &completion[..keep_bytes]
}

/// chat-FIM 提示：语义上下文（imports/外层声明）+ 光标前缀/后缀 + 语言。
fn fim_messages(
    prefix: &str,
    suffix: &str,
    language: &str,
    imports: &[String],
    enclosing: Option<&str>,
    snippets: &[String],
) -> Vec<Msg> {
    let prefix = clip_tail(prefix, CONTEXT_CHARS);
    let suffix = clip_head(suffix, CONTEXT_CHARS);
    let mut context = String::new();
    if !imports.is_empty() {
        context.push_str("Relevant imports:\n");
        for i in imports {
            context.push_str(i);
            context.push('\n');
        }
    }
    if let Some(e) = enclosing {
        context.push_str("Enclosing declaration:\n");
        context.push_str(e);
        context.push('\n');
    }
    if !snippets.is_empty() {
        context.push_str("Related declarations:\n");
        for s in snippets {
            context.push_str(s);
            context.push('\n');
        }
    }
    let context = clip_head(&context, SEMANTIC_CHARS);
    let user = format!(
        "Language: {language}\n{context}<code_before_cursor>\n{prefix}\n</code_before_cursor>\n\
         <code_after_cursor>\n{suffix}\n</code_after_cursor>\n\
         Output only the code that goes at the cursor position."
    );
    vec![Msg::system("oxidea-completion", SYSTEM_PROMPT), Msg::user("oxidea-completion", user)]
}

/// FIM token 风格（不同代码模型的填充标记）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FimStyle {
    Qwen,
    StarCoder,
    CodeLlama,
}

impl FimStyle {
    fn tokens(self) -> (&'static str, &'static str, &'static str) {
        match self {
            FimStyle::Qwen => ("<|fim_prefix|>", "<|fim_suffix|>", "<|fim_middle|>"),
            FimStyle::StarCoder => ("<fim_prefix>", "<fim_suffix>", "<fim_middle>"),
            FimStyle::CodeLlama => ("<PRE>", "<SUF>", "<MID>"),
        }
    }
}

/// 专用 FIM 快路径配置（`AGENTSCOPE_FIM_MODEL` 存在才启用）。
struct FimConfig {
    model: String,
    base_url: String,
    api_key: String,
    style: FimStyle,
}

fn fim_config(env: &BTreeMap<String, String>) -> Option<FimConfig> {
    let model = env.get("AGENTSCOPE_FIM_MODEL")?.trim().to_string();
    if model.is_empty() {
        return None;
    }
    let base_url = env
        .get("AGENTSCOPE_FIM_BASE_URL")
        .filter(|s| !s.trim().is_empty())
        .or_else(|| env.get("AGENTSCOPE_BASE_URL"))
        .filter(|s| !s.trim().is_empty())?
        .trim()
        .to_string();
    let api_key = env.get("AGENTSCOPE_API_KEY").cloned().unwrap_or_default();
    let style = match env.get("AGENTSCOPE_FIM_STYLE").map(String::as_str) {
        Some("starcoder") => FimStyle::StarCoder,
        Some("codellama") => FimStyle::CodeLlama,
        _ => FimStyle::Qwen,
    };
    Some(FimConfig { model, base_url, api_key, style })
}

/// FIM token 提示：`<prefix>前缀<suffix>后缀<middle>`。
fn fim_prompt(style: FimStyle, prefix: &str, suffix: &str) -> String {
    let (p, s, m) = style.tokens();
    format!("{p}{prefix}{s}{suffix}{m}")
}

/// 调 OpenAI 兼容 `/completions` FIM 快路径；失败/取消/空 → None。
async fn fim_complete(cfg: &FimConfig, prompt: &str, cancel: &CancellationToken) -> Option<String> {
    let (p, s, m) = cfg.style.tokens();
    let body = serde_json::json!({
        "model": cfg.model,
        "prompt": prompt,
        "max_tokens": MAX_COMPLETION_TOKENS,
        "temperature": 0,
        "stop": [p, s, m],
    });
    let url = format!("{}/completions", cfg.base_url.trim_end_matches('/'));
    let client = reqwest::Client::new();
    let request = async {
        let resp = client.post(&url).bearer_auth(&cfg.api_key).json(&body).timeout(FIM_TIMEOUT).send().await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let v: serde_json::Value = resp.json().await.ok()?;
        v["choices"][0]["text"].as_str().map(str::to_string)
    };
    let text = tokio::select! {
        _ = cancel.cancelled() => return None,
        r = request => r?,
    };
    let trimmed = text.trim_end();
    if trimmed.is_empty() { None } else { Some(trimmed.to_string()) }
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

/// 从 chat 回复提取补全文本：首个文本块 → 剥围栏 → 去尾部空白；空 → None。
fn completion_text(response: &ChatResponse) -> Option<String> {
    let text = response.content.iter().find_map(|b| match b {
        ContentBlock::Text { text } => Some(text.as_str()),
        _ => None,
    })?;
    let completion = strip_fences(text).trim_end();
    if completion.is_empty() { None } else { Some(completion.to_string()) }
}

/// 查询 AI 补全来源。无配置/失败/取消 → None（调用方回退 LSP+插件）。
pub(crate) async fn suggest(
    host: &Host,
    window: &str,
    doc: DocId,
    pos: Pos,
    cancel: CancellationToken,
) -> Option<Vec<InlineCompletionItem>> {
    if cancel.is_cancelled() {
        return None;
    }
    let env = ox_env(host, window)?;
    let project = host.project(window).ok()?;
    let snap = project.documents.snapshot(doc).ok()?;
    if snap.large {
        return None;
    }
    let text = ide_text::Document::new(&snap.text);
    let byte = text.offset_to_byte(text.pos_to_offset(pos));
    let prefix = &snap.text[..byte];
    let suffix = &snap.text[byte..];
    if prefix.trim().is_empty() {
        return None;
    }
    let (imports, enclosing) = semantic_context(&snap.text, byte);
    // 跨文件相关声明：其它打开文档的活文本（不查 LSP/索引，不给 jdtls 加负载）。
    let snippets = {
        let tokens = lookup_tokens(prefix);
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

    // 路B：专用 FIM 快路径（纯代码前缀，补缺失 imports）。
    let fast = match fim_config(&env) {
        Some(cfg) => {
            let augmented = augment_prefix(prefix, &imports);
            let header = snippet_header(&snippets, &snap.language.0);
            let full_prefix = format!("{header}{}", clip_tail(&augmented, CONTEXT_CHARS));
            let prompt = fim_prompt(cfg.style, &full_prefix, clip_head(suffix, CONTEXT_CHARS));
            fim_complete(&cfg, &prompt, &cancel).await
        }
        None => None,
    };

    // 路A：chat-FIM 回退（语义上下文增强）。
    let completion = match fast {
        Some(c) => c,
        None => {
            let pairs: Vec<(String, String)> = env.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
            let model: Arc<dyn Model> = agentscope_acp::session::build_model(&pairs).ok()?;
            let request = ChatRequest {
                messages: fim_messages(prefix, suffix, &snap.language.0, &imports, enclosing.as_deref(), &snippets),
                options: GenerateOptions {
                    temperature: Some(0.0),
                    max_tokens: Some(MAX_COMPLETION_TOKENS),
                    ..Default::default()
                },
            };
            let response = tokio::select! {
                _ = cancel.cancelled() => return None,
                r = tokio::time::timeout(AI_TIMEOUT, model.call(request)) => r.ok()?.ok()?,
            };
            completion_text(&response)?
        }
    };
    // 后处理：去掉与后缀重叠的尾部（防接受后重复），清理尾部空白；空 → 回退 LSP。
    let cleaned = trim_suffix_overlap(&completion, suffix).trim_end();
    if cleaned.is_empty() || cancel.is_cancelled() {
        return None;
    }
    Some(vec![InlineCompletionItem {
        insert_text: cleaned.to_string(),
        range: None,
        filter_text: None,
        command: None,
        server: AI_SERVER,
    }])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_fences_removes_language_tag_and_closing_fence() {
        assert_eq!(strip_fences("```rust\nlet x = 1;\n```"), "let x = 1;");
        assert_eq!(strip_fences("```\nlet x = 1;\n```"), "let x = 1;");
        assert_eq!(strip_fences("let x = 1;"), "let x = 1;");
        assert_eq!(strip_fences("  let x = 1;  "), "let x = 1;");
    }

    #[test]
    fn clip_keeps_char_boundaries() {
        assert_eq!(clip_tail("abcdef", 3), "def");
        assert_eq!(clip_head("abcdef", 3), "abc");
        assert_eq!(clip_tail("短", 3), "短");
        assert_eq!(clip_head("你好世界", 2), "你好");
        assert_eq!(clip_tail("你好世界", 2), "世界");
    }

    #[test]
    fn completion_text_extracts_strips_and_trims() {
        let r = ChatResponse::from_text("```java\nreturn name;\n```\n");
        assert_eq!(completion_text(&r).as_deref(), Some("return name;"));
        assert_eq!(completion_text(&ChatResponse::from_text("   ")), None);
        assert_eq!(completion_text(&ChatResponse::default()), None);
    }

    #[test]
    fn semantic_context_collects_imports_and_enclosing_declaration() {
        let text = "use std::fmt;\nimport java.util.List;\n\npub fn render(x: i32) {\n    let y = ";
        let caret = text.len();
        let (imports, enclosing) = semantic_context(text, caret);
        assert_eq!(imports, vec!["use std::fmt;", "import java.util.List;"]);
        assert_eq!(enclosing.as_deref(), Some("pub fn render(x: i32) {"));
    }

    #[test]
    fn semantic_context_ignores_non_import_lines() {
        let text = "let use = 1;\n// import fake\nfn main() {}\n";
        let (imports, _) = semantic_context(text, text.len());
        assert!(imports.is_empty());
    }

    #[test]
    fn augment_prefix_adds_only_missing_imports() {
        let imports = vec!["use a;".to_string(), "use b;".to_string()];
        assert_eq!(augment_prefix("use a;\nfn f() {", &imports), "use b;\nuse a;\nfn f() {");
        assert_eq!(augment_prefix("use a;\nuse b;\nfn f() {", &imports), "use a;\nuse b;\nfn f() {");
    }

    #[test]
    fn fim_prompt_wraps_prefix_suffix_with_tokens() {
        assert_eq!(fim_prompt(FimStyle::Qwen, "a", "b"), "<|fim_prefix|>a<|fim_suffix|>b<|fim_middle|>");
        assert_eq!(fim_prompt(FimStyle::StarCoder, "a", "b"), "<fim_prefix>a<fim_suffix>b<fim_middle>");
        assert_eq!(fim_prompt(FimStyle::CodeLlama, "a", "b"), "<PRE>a<SUF>b<MID>");
    }

    #[test]
    fn fim_config_requires_model_and_base_url() {
        let mut env = BTreeMap::new();
        assert!(fim_config(&env).is_none());
        env.insert("AGENTSCOPE_FIM_MODEL".into(), "qwen2.5-coder".into());
        assert!(fim_config(&env).is_none()); // 缺 base_url
        env.insert("AGENTSCOPE_BASE_URL".into(), "http://localhost:8000/v1".into());
        let cfg = fim_config(&env).unwrap();
        assert_eq!(cfg.model, "qwen2.5-coder");
        assert_eq!(cfg.style, FimStyle::Qwen);
        env.insert("AGENTSCOPE_FIM_STYLE".into(), "starcoder".into());
        assert_eq!(fim_config(&env).unwrap().style, FimStyle::StarCoder);
    }

    #[test]
    fn fim_prompt_carries_context_prefix_suffix_and_language() {
        let imports = vec!["use std::fmt;".to_string()];
        let snippets = vec!["fn helper(x: i32) -> i32".to_string()];
        let msgs = fim_messages("fn main() {\n    let x = ", ";\n}", "rust", &imports, Some("fn main() {"), &snippets);
        assert_eq!(msgs.len(), 2);
        let user = msgs[1].text();
        assert!(user.contains("Language: rust"));
        assert!(user.contains("use std::fmt;"));
        assert!(user.contains("Enclosing declaration:"));
        assert!(user.contains("Related declarations:"));
        assert!(user.contains("fn helper(x: i32) -> i32"));
        assert!(user.contains("let x = "));
        assert!(user.contains(";\n}"));
    }

    #[test]
    fn lookup_tokens_takes_trailing_identifiers() {
        let tokens = lookup_tokens("fn main() {\n    let total = calculateTot");
        assert!(tokens.contains(&"calculateTot".to_string()));
        assert!(tokens.contains(&"total".to_string()));
        assert!(lookup_tokens("ab").is_empty());
    }

    #[test]
    fn related_declarations_matches_names_and_dedups() {
        let texts = vec![
            "pub fn calculateTotal(x: i32) -> i32 {\n    x\n}\n".to_string(),
            "class Other {}\n".to_string(),
            "pub fn calculateTotal(x: i32) -> i32 {\n    x\n}\n".to_string(),
        ];
        let tokens = vec!["calculateTotal".to_string()];
        let out = related_declarations(&texts, &tokens);
        assert_eq!(out, vec!["pub fn calculateTotal(x: i32) -> i32 {"]);
        assert!(related_declarations(&texts, &[]).is_empty());
    }

    #[test]
    fn snippet_header_uses_language_comment() {
        let snippets = vec!["def helper(x):".to_string()];
        assert_eq!(comment_token("python"), "#");
        assert_eq!(comment_token("rust"), "//");
        assert_eq!(snippet_header(&snippets, "python"), "# Related declarations:\n# def helper(x):\n");
        assert_eq!(snippet_header(&[], "rust"), "");
    }

    #[test]
    fn trim_suffix_overlap_removes_echoed_suffix() {
        assert_eq!(trim_suffix_overlap("compute();\n}", ";\n}"), "compute()");
        assert_eq!(trim_suffix_overlap("foo", "bar"), "foo");
        assert_eq!(trim_suffix_overlap("x;", ";\n}"), "x");
        assert_eq!(trim_suffix_overlap("你好", "好世界"), "你");
    }
}
