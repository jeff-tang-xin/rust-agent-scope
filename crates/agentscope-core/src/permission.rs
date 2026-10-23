//! 权限系统（对应 Java `io.agentscope.core.permission`）。
//!
//! 分三层：
//! - **裁决契约**：[`PermissionChecker`]（`Allow / Deny / RequireApproval`），已接入工具执行路径；
//! - **状态数据**：[`PermissionMode`] / [`PermissionBehavior`] / [`PermissionRule`] /
//!   [`AdditionalWorkingDirectory`] / [`PermissionContextState`]——会话级权限配置，
//!   随 [`crate::state::AgentState`] 一起持久化；
//! - **规则引擎**（Java `PermissionEngine`）：尚未实现，待权限批次接入。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 对一次工具调用的裁决。
#[derive(Debug, Clone, PartialEq)]
pub enum PermissionDecision {
    /// 放行。
    Allow,
    /// 拒绝，携带原因（会作为错误结果回传给模型）。
    Deny(String),
    /// 需要用户审批（human-in-the-loop，审批流未实现）。
    RequireApproval,
}

/// 权限检查器。默认实现全部放行。
#[async_trait::async_trait]
pub trait PermissionChecker: Send + Sync {
    async fn check(&self, _tool_name: &str, _input: &Value) -> PermissionDecision {
        PermissionDecision::Allow
    }
}

/// 默认放行检查器（builder 未指定 permission 时使用）。
pub struct AllowAll;

#[async_trait::async_trait]
impl PermissionChecker for AllowAll {}

// ---------- 规则引擎（对应 Java `PermissionEngine`） ----------

/// 会话级权限规则引擎：按固定优先级裁决一次工具调用。
///
/// 裁决顺序（对齐 Java `PermissionEngine.check`，工具自身检查 bypass-immune）：
/// ① deny 规则 → ② ask 规则 → ③ 工具自身检查（EXPLORE/ACCEPT_EDITS 只读处理 +
/// `Tool::check_permissions`）→ ④ allow 规则 → ⑤ BYPASS 兜底放行 →
/// ⑥ 默认策略（DontAsk → Deny，否则 RequireApproval）。
///
/// 引擎持有 [`PermissionContextState`] 快照：调用方在锁内 clone 出快照后构造引擎，
/// 引擎内部不再碰锁（避免 std Mutex 跨 await 持有）。
pub struct PermissionEngine {
    context: PermissionContextState,
}

impl PermissionEngine {
    pub fn new(context: PermissionContextState) -> Self {
        Self { context }
    }

    /// 引擎持有的权限上下文快照。
    pub fn context(&self) -> &PermissionContextState {
        &self.context
    }

    /// 裁决一次工具调用。
    ///
    /// `tool` 为 `None`（工具未注册 / schema-only 外部工具）时对齐 Java 非
    /// `ToolBase` 工具：跳过工具自身检查，仅按规则表与模式裁决。
    pub async fn check(
        &self,
        tool: Option<&dyn crate::tool::Tool>,
        tool_name: &str,
        input: &Value,
    ) -> PermissionDecision {
        // ① deny 规则（最高优先级，命中即拒）
        if let Some(decision) = self.check_rule_table(
            &self.context.deny_rules,
            PermissionBehavior::Deny,
            tool,
            input,
        ) {
            return decision;
        }
        // ② ask 规则（命中即要求审批）
        if let Some(decision) = self.check_rule_table(
            &self.context.ask_rules,
            PermissionBehavior::Ask,
            tool,
            input,
        ) {
            return decision;
        }
        // ③ 工具自身检查：模式级只读处理 + 工具覆写的 check_permissions
        if let Some(tool) = tool {
            if let Some(decision) = self.mode_check(tool) {
                return decision;
            }
            if let Some(decision) = tool.check_permissions(input, &self.context).await {
                return decision;
            }
        }
        // ④ allow 规则
        if let Some(decision) = self.check_rule_table(
            &self.context.allow_rules,
            PermissionBehavior::Allow,
            tool,
            input,
        ) {
            return decision;
        }
        // ⑤ BYPASS 兜底放行（工具自身检查已在上一步 bypass-immune 地拦截过）
        if self.context.mode == PermissionMode::Bypass {
            return PermissionDecision::Allow;
        }
        // ⑥ 默认策略：DontAsk 模式用户不可用 → 拒绝；否则要求审批
        if self.context.mode == PermissionMode::DontAsk {
            return PermissionDecision::Deny(format!(
                "tool {tool_name} is not allowed (dont_ask mode: user is not available)"
            ));
        }
        PermissionDecision::RequireApproval
    }

    /// 按规则表裁决：命中返回对应行为，未命中返回 `None` 交给下一层。
    ///
    /// 规则内容为 `None` 或空串匹配整个工具；非空时交给
    /// [`crate::tool::Tool::matches_rule`]（工具不存在则不匹配，对齐 Java 非
    /// ToolBase 工具走默认 matchRule 的语义）。
    fn check_rule_table(
        &self,
        rules: &BTreeMap<String, Vec<PermissionRule>>,
        behavior: PermissionBehavior,
        tool: Option<&dyn crate::tool::Tool>,
        input: &Value,
    ) -> Option<PermissionDecision> {
        let matched = rules
            .get(tool_name_of(tool))
            .into_iter()
            .flatten()
            .any(|rule| match rule.rule_content.as_deref() {
                None | Some("") => true,
                Some(content) => tool.is_some_and(|tool| tool.matches_rule(content, input)),
            });
        matched.then(|| match behavior {
            PermissionBehavior::Deny => {
                PermissionDecision::Deny("denied by permission rule".into())
            }
            PermissionBehavior::Ask => PermissionDecision::RequireApproval,
            _ => PermissionDecision::Allow,
        })
    }

    /// 模式级只读处理（对齐 Java `checkExploreMode`）：
    /// EXPLORE 下非只读工具直接拒绝；ACCEPT_EDITS 下只读工具自动放行。
    fn mode_check(&self, tool: &dyn crate::tool::Tool) -> Option<PermissionDecision> {
        match self.context.mode {
            PermissionMode::Explore => {
                if tool.is_read_only() {
                    Some(PermissionDecision::Allow)
                } else {
                    Some(PermissionDecision::Deny(
                        "explore mode does not allow modifications".into(),
                    ))
                }
            }
            PermissionMode::AcceptEdits if tool.is_read_only() => {
                Some(PermissionDecision::Allow)
            }
            _ => None,
        }
    }
}

/// 规则表按工具名组织；`tool` 为 `None` 时用空串兜底键（不会命中任何注册规则）。
fn tool_name_of(tool: Option<&dyn crate::tool::Tool>) -> &str {
    tool.map(|tool| tool.name()).unwrap_or("")
}

// ---------- 状态数据（对应 Java permission 包的纯数据类） ----------

/// 权限模式（对应 Java `PermissionMode`，wire 值为小写下划线）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    /// 默认：按规则表裁决，未命中则询问。
    #[default]
    Default,
    /// 自动接受文件编辑类工具。
    AcceptEdits,
    /// 只读探查模式。
    Explore,
    /// 全部放行（绕过规则表）。
    Bypass,
    /// 不问用户：未命中规则一律拒绝。
    DontAsk,
}

/// 规则行为（对应 Java `PermissionBehavior`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionBehavior {
    Allow,
    Deny,
    Ask,
    /// 交给下一层（更宽的规则集或默认策略）裁决。
    Passthrough,
}

/// 一条权限规则（对应 Java `PermissionRule`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionRule {
    #[serde(rename = "tool_name")]
    pub tool_name: String,
    /// 规则内容（如 `Bash(git commit:*)` 的命令前缀），`None` 表示匹配整个工具。
    #[serde(rename = "rule_content", default, skip_serializing_if = "Option::is_none")]
    pub rule_content: Option<String>,
    pub behavior: PermissionBehavior,
    /// 规则来源（`user` / `project` / `session` 等，仅用于审计）。
    pub source: String,
}

/// 额外允许访问的工作目录（对应 Java `AdditionalWorkingDirectory`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdditionalWorkingDirectory {
    pub path: String,
    pub source: String,
}

/// 会话级权限配置快照（对应 Java `PermissionContextState`）。
///
/// 规则按「工具名 → 规则列表」组织。Java 用 `LinkedHashMap` 保留插入顺序，
/// Rust 用 `BTreeMap` 让序列化结果稳定（差异仅在遍历顺序）。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PermissionContextState {
    #[serde(default)]
    pub mode: PermissionMode,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub working_directories: BTreeMap<String, AdditionalWorkingDirectory>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub allow_rules: BTreeMap<String, Vec<PermissionRule>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub deny_rules: BTreeMap<String, Vec<PermissionRule>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub ask_rules: BTreeMap<String, Vec<PermissionRule>>,
}

impl PermissionContextState {
    /// 是否为空配置（默认模式且无任何规则/目录）。
    pub fn is_trivial(&self) -> bool {
        self.mode == PermissionMode::Default
            && self.working_directories.is_empty()
            && self.allow_rules.is_empty()
            && self.deny_rules.is_empty()
            && self.ask_rules.is_empty()
    }

    /// 切换模式（对应 Java `withMode`；值不变时原样返回）。
    pub fn with_mode(&self, mode: PermissionMode) -> Self {
        if mode == self.mode {
            return self.clone();
        }
        let mut next = self.clone();
        next.mode = mode;
        next
    }

    /// 追加一条 allow 规则。
    pub fn add_allow_rule(&mut self, rule: PermissionRule) {
        self.allow_rules
            .entry(rule.tool_name.clone())
            .or_default()
            .push(rule);
    }

    /// 追加一条 deny 规则。
    pub fn add_deny_rule(&mut self, rule: PermissionRule) {
        self.deny_rules
            .entry(rule.tool_name.clone())
            .or_default()
            .push(rule);
    }

    /// 追加一条 ask 规则。
    pub fn add_ask_rule(&mut self, rule: PermissionRule) {
        self.ask_rules
            .entry(rule.tool_name.clone())
            .or_default()
            .push(rule);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 测试用工具：可配置只读性与自检裁决。
    struct EchoTool {
        read_only: bool,
        self_decision: Option<PermissionDecision>,
    }

    #[async_trait::async_trait]
    impl crate::tool::Tool for EchoTool {
        fn name(&self) -> &str {
            "echo"
        }
        fn description(&self) -> &str {
            "回声"
        }
        fn parameters(&self) -> Value {
            json!({"type": "object"})
        }
        async fn call(
            &self,
            _input: Value,
        ) -> crate::error::Result<crate::tool::ToolOutput> {
            Ok(crate::tool::ToolOutput::ok("echo"))
        }
        fn is_read_only(&self) -> bool {
            self.read_only
        }
        async fn check_permissions(
            &self,
            _input: &Value,
            _context: &PermissionContextState,
        ) -> Option<PermissionDecision> {
            self.self_decision.clone()
        }
    }

    fn rule(tool: &str, content: Option<&str>, behavior: PermissionBehavior) -> PermissionRule {
        PermissionRule {
            tool_name: tool.into(),
            rule_content: content.map(str::to_string),
            behavior,
            source: "user".into(),
        }
    }

    #[tokio::test]
    async fn deny_rule_beats_allow_rule() {
        let mut context = PermissionContextState::default();
        context.add_allow_rule(rule("echo", None, PermissionBehavior::Allow));
        context.add_deny_rule(rule("echo", None, PermissionBehavior::Deny));
        let engine = PermissionEngine::new(context);
        let tool = EchoTool { read_only: false, self_decision: None };
        let decision = engine.check(Some(&tool), "echo", &json!({})).await;
        assert!(matches!(decision, PermissionDecision::Deny(_)));
    }

    #[tokio::test]
    async fn ask_rule_requires_approval() {
        let mut context = PermissionContextState::default();
        context.add_ask_rule(rule("echo", None, PermissionBehavior::Ask));
        let engine = PermissionEngine::new(context);
        let tool = EchoTool { read_only: false, self_decision: None };
        let decision = engine.check(Some(&tool), "echo", &json!({})).await;
        assert_eq!(decision, PermissionDecision::RequireApproval);
    }

    #[tokio::test]
    async fn allow_rule_allows() {
        let mut context = PermissionContextState::default();
        context.add_allow_rule(rule("echo", None, PermissionBehavior::Allow));
        let engine = PermissionEngine::new(context);
        let tool = EchoTool { read_only: false, self_decision: None };
        let decision = engine.check(Some(&tool), "echo", &json!({})).await;
        assert_eq!(decision, PermissionDecision::Allow);
    }

    #[tokio::test]
    async fn unmatched_rule_content_falls_to_default() {
        // 默认 matches_rule 对非空 rule_content 不命中（对齐 Java matchRule），
        // 未命中任何规则 → Default 模式默认策略 → RequireApproval
        let mut context = PermissionContextState::default();
        context.add_allow_rule(rule("echo", Some("git commit"), PermissionBehavior::Allow));
        let engine = PermissionEngine::new(context);
        let tool = EchoTool { read_only: false, self_decision: None };
        let decision = engine.check(Some(&tool), "echo", &json!({})).await;
        assert_eq!(decision, PermissionDecision::RequireApproval);
    }

    #[tokio::test]
    async fn bypass_mode_allows_unmatched() {
        let engine = PermissionEngine::new(PermissionContextState::default().with_mode(PermissionMode::Bypass));
        let tool = EchoTool { read_only: false, self_decision: None };
        let decision = engine.check(Some(&tool), "echo", &json!({})).await;
        assert_eq!(decision, PermissionDecision::Allow);
    }

    #[tokio::test]
    async fn dont_ask_denies_unmatched() {
        let engine = PermissionEngine::new(PermissionContextState::default().with_mode(PermissionMode::DontAsk));
        let tool = EchoTool { read_only: false, self_decision: None };
        let decision = engine.check(Some(&tool), "echo", &json!({})).await;
        assert!(matches!(decision, PermissionDecision::Deny(_)));
    }

    #[tokio::test]
    async fn explore_mode_gates_read_only() {
        let engine = PermissionEngine::new(PermissionContextState::default().with_mode(PermissionMode::Explore));
        let read_only = EchoTool { read_only: true, self_decision: None };
        assert_eq!(
            engine.check(Some(&read_only), "echo", &json!({})).await,
            PermissionDecision::Allow
        );
        let mutating = EchoTool { read_only: false, self_decision: None };
        assert!(matches!(
            engine.check(Some(&mutating), "echo", &json!({})).await,
            PermissionDecision::Deny(_)
        ));
    }

    #[tokio::test]
    async fn accept_edits_allows_read_only_only() {
        let engine = PermissionEngine::new(PermissionContextState::default().with_mode(PermissionMode::AcceptEdits));
        let read_only = EchoTool { read_only: true, self_decision: None };
        assert_eq!(
            engine.check(Some(&read_only), "echo", &json!({})).await,
            PermissionDecision::Allow
        );
        // 非只读：模式层不拦截，落到默认策略
        let mutating = EchoTool { read_only: false, self_decision: None };
        assert_eq!(
            engine.check(Some(&mutating), "echo", &json!({})).await,
            PermissionDecision::RequireApproval
        );
    }

    #[tokio::test]
    async fn tool_self_check_is_bypass_immune() {
        // 工具自检 Deny 优先于 allow 规则与 BYPASS 模式（对齐 Java bypass-immune）
        let mut context = PermissionContextState::default();
        context.add_allow_rule(rule("echo", None, PermissionBehavior::Allow));
        let engine = PermissionEngine::new(context.with_mode(PermissionMode::Bypass));
        let tool = EchoTool {
            read_only: false,
            self_decision: Some(PermissionDecision::Deny("sandbox violation".into())),
        };
        let decision = engine.check(Some(&tool), "echo", &json!({})).await;
        assert_eq!(decision, PermissionDecision::Deny("sandbox violation".into()));
    }

    #[tokio::test]
    async fn unregistered_tool_skips_self_check() {
        // 工具未注册（None）：跳过工具自身检查，规则表未命中 → 默认策略
        let mut context = PermissionContextState::default();
        context.add_allow_rule(rule("other", None, PermissionBehavior::Allow));
        let engine = PermissionEngine::new(context);
        let decision = engine.check(None, "echo", &json!({})).await;
        assert_eq!(decision, PermissionDecision::RequireApproval);
    }

    #[test]
    fn mode_wire_values_are_snake_case() {
        let json = serde_json::to_string(&PermissionMode::AcceptEdits).unwrap();
        assert_eq!(json, "\"accept_edits\"");
        let back: PermissionMode = serde_json::from_str("\"dont_ask\"").unwrap();
        assert_eq!(back, PermissionMode::DontAsk);
    }

    #[test]
    fn trivial_state_serializes_compactly() {
        let state = PermissionContextState::default();
        assert!(state.is_trivial());
        assert_eq!(
            serde_json::to_string(&state).unwrap(),
            "{\"mode\":\"default\"}"
        );
    }

    #[test]
    fn with_mode_keeps_rules() {
        let mut state = PermissionContextState::default();
        state.add_deny_rule(PermissionRule {
            tool_name: "Bash".into(),
            rule_content: Some("rm -rf".into()),
            behavior: PermissionBehavior::Deny,
            source: "user".into(),
        });
        let next = state.with_mode(PermissionMode::Bypass);
        assert_eq!(next.mode, PermissionMode::Bypass);
        assert_eq!(next.deny_rules.len(), 1);
    }
}
