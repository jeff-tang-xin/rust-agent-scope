//! 工具系统（对应 Java 版 `io.agentscope.core.tool`）。
//!
//! 复刻要点：
//! - **工具分组**：[`ToolGroup`] 把工具编为逻辑组，只有 active 组的工具暴露给模型；
//!   scope 区分「agent 通过元工具自行管理」（Meta）与「开发者代码管理」（External）。
//! - **调用上下文**：[`ToolContext`] 携带 tool_call_id / agent 身份 / 事件出口，
//!   长任务工具可经 [`ToolContext::emit_progress`] 推送进度（对应 Java `ToolEmitter`，
//!   进度只进事件流，不进 LLM 上下文）。
//! - **调用拦截器**：[`ToolCallInterceptor`] 做 per-call 参数级业务校验（沙箱路径、审计、
//!   参数规整、策略拒绝）；工具级「能不能调」仍归 permission 引擎。
//! - **外部工具**：schema-only 注册的工具无本地实现，调用即挂起（对应 Java `SchemaOnlyTool`）。
//!
//! 形态说明：Java `ToolExecutionContext` 是以 Class 为键的类型化存储，Rust 中照搬需要
//! `Box<dyn Any>` downcast（反模式①）。此处改用显式字段 + JSON metadata 承载扩展数据。

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{Error, Result};
use crate::event::{AgentEvent, AgentEventEmitter, AgentEventKind};
use crate::message::{ContentBlock, ToolResultState};
use crate::model::ToolSchema;
use crate::permission::{PermissionContextState, PermissionDecision};

/// 工具执行输出（对应 Java `ToolResultBlock` 的内容 + state）。
#[derive(Debug, Clone, PartialEq)]
pub struct ToolOutput {
    pub content: String,
    pub state: ToolResultState,
    /// 执行明细（对应 Java `ToolResultBlock.withExecutionDetails`；`None` = 不携带）。
    pub execution_details: Option<crate::message::ToolExecutionDetails>,
}

impl ToolOutput {
    /// 成功输出。
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            state: ToolResultState::Success,
            execution_details: None,
        }
    }

    /// 失败输出（以错误信息回传给模型，模型可据此调整重试）。
    pub fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            state: ToolResultState::Error,
            execution_details: None,
        }
    }

    /// 被策略拒绝的输出（与执行错误区分：拒绝有独立状态，对齐 Java ToolResultState.DENIED）。
    pub fn denied(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            state: ToolResultState::Denied,
            execution_details: None,
        }
    }

    /// 挂起输出：等待外部执行结果（对齐 Java `ToolSuspendException` 的挂起语义）。
    pub fn suspended(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            state: ToolResultState::Suspended,
            execution_details: None,
        }
    }

    /// 是否为非成功结果（回传给模型时以错误信息看待）。
    pub fn is_error(&self) -> bool {
        self.state != ToolResultState::Success
    }

    /// 附加执行明细（对应 Java `withExecutionDetails`）。
    pub fn with_execution_details(mut self, details: crate::message::ToolExecutionDetails) -> Self {
        self.execution_details = Some(details);
        self
    }
}

/// 工具组的管理归属（对应 Java `ToolGroupScope`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolGroupScope {
    /// 由 agent 经元工具（`reset_equipped_tools`）管理，出现在元工具参数枚举中。
    #[default]
    Meta,
    /// 由开发者代码管理，对元工具不可见。
    External,
}

/// 工具组：带激活状态的逻辑分组（对应 Java `ToolGroup`）。
///
/// 只有 active 组的工具会出现在模型可见的 schema 列表中；
/// 不属于任何组的工具始终可见。
#[derive(Debug, Clone, PartialEq)]
pub struct ToolGroup {
    pub name: String,
    pub description: String,
    pub active: bool,
    pub scope: ToolGroupScope,
    pub tools: HashSet<String>,
}

impl ToolGroup {
    /// 创建工具组（默认：空描述、active、Meta scope）。
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: String::new(),
            active: true,
            scope: ToolGroupScope::Meta,
            tools: HashSet::new(),
        }
    }

    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = description.into();
        self
    }

    pub fn with_active(mut self, active: bool) -> Self {
        self.active = active;
        self
    }

    pub fn with_scope(mut self, scope: ToolGroupScope) -> Self {
        self.scope = scope;
        self
    }

    pub fn add_tool(&mut self, tool_name: impl Into<String>) {
        self.tools.insert(tool_name.into());
    }

    pub fn remove_tool(&mut self, tool_name: &str) {
        self.tools.remove(tool_name);
    }

    pub fn contains_tool(&self, tool_name: &str) -> bool {
        self.tools.contains(tool_name)
    }
}

/// 工具调用上下文（对应 Java `ToolExecutionContext` 的显式字段形态）。
#[derive(Debug, Clone)]
pub struct ToolContext {
    /// 本次调用所属的回复 id（事件流关联键）。
    pub reply_id: String,
    pub tool_call_id: String,
    pub tool_name: String,
    pub agent_id: String,
    pub agent_name: String,
    /// 会话 id（对应 Java 工具方法注入的 `RuntimeContext.sessionId`；
    /// 直接调用工具时为空串）。
    pub session_id: String,
    /// 用户 id（对应 Java `RuntimeContext.userId`；直接调用工具时为 `None`）。
    pub user_id: Option<String>,
    /// 扩展数据（Rust 形态的类型化存储替代：显式字段 + JSON metadata）。
    pub metadata: serde_json::Map<String, Value>,
    /// 事件出口（对应 Java `ToolEmitter`）；`None` 时进度静默丢弃。
    pub emitter: Option<AgentEventEmitter>,
    /// 本次调用的活状态（对应 Java `ToolContext` 挂的 `AgentState`）。
    /// 元工具通过它改写会话级工具激活组（`ToolContextState.activated_groups`）。
    pub state: Option<Arc<Mutex<crate::state::AgentState>>>,
}

impl ToolContext {
    /// 最小上下文（直接调用工具时使用，无事件流）。
    pub fn new(tool_call_id: impl Into<String>, tool_name: impl Into<String>) -> Self {
        Self {
            reply_id: String::new(),
            tool_call_id: tool_call_id.into(),
            tool_name: tool_name.into(),
            agent_id: String::new(),
            agent_name: String::new(),
            session_id: String::new(),
            user_id: None,
            metadata: serde_json::Map::new(),
            emitter: None,
            state: None,
        }
    }

    /// 推送执行进度（对应 Java `ToolEmitter` 的 emit）。
    ///
    /// 进度只发往事件流与 hook（供监控/日志），**不作为工具结果发给 LLM**；
    /// 工具方法的最终返回值才是 tool result。
    pub async fn emit_progress(&self, content: ContentBlock) {
        if let Some(tx) = &self.emitter {
            let _ = tx
                .send(AgentEvent::new(AgentEventKind::ToolProgress {
                    reply_id: self.reply_id.clone(),
                    tool_call_id: self.tool_call_id.clone(),
                    tool_call_name: self.tool_name.clone(),
                    content,
                }))
                .await;
        }
    }
}

/// 一次工具调用的快照（对应 Java `ToolInvocation`）。
#[derive(Debug, Clone)]
pub struct ToolInvocation {
    pub tool_name: String,
    pub input: Value,
    pub context: ToolContext,
}

impl ToolInvocation {
    /// 替换调用参数（拦截器改写参数用；改写后不再重新做 schema 校验，对齐 Java 语义）。
    pub fn with_input(mut self, input: Value) -> Self {
        self.input = input;
        self
    }
}

/// 工具调用拦截器：在 schema 校验之后、真实执行之前做 per-call 参数级检查。
///
/// 对应 Java `ToolCallInterceptor.intercept(invocation, next)` 责任链，Rust 形态改为
/// 前置/后置两段式（避免为链式泛型引入 boxed future 环形结构）：
/// - [`ToolCallInterceptor::before`] 对应 `next` 调用之前的检查，返回 `Some` 即短路；
/// - [`ToolCallInterceptor::after`] 对应拿到结果后的观察/改写（审计、结果规整）。
///
/// 常见动作映射：放行 = `before` 返回 None；策略拒绝 = 返回
/// `Some(ToolOutput::denied(..))`；参数不合法 = 返回 `Some(ToolOutput::error(..))`；
/// 改写参数 = 修改 `inv.input`；挂起 = 工具侧返回 [`Error::ToolSuspended`]。
#[async_trait::async_trait]
pub trait ToolCallInterceptor: Send + Sync {
    /// 链中位置：值越大越靠外层（先拦截、最后收到结果）；同值按注册顺序。对齐 Java `order()`。
    fn order(&self) -> i32 {
        1
    }

    /// 前置钩子：返回 `Some` 短路整条链（结果为该输出）。
    async fn before(&self, _inv: &mut ToolInvocation) -> Option<ToolOutput> {
        None
    }

    /// 后置钩子：观察或改写真实执行结果。
    async fn after(&self, _inv: &ToolInvocation, output: ToolOutput) -> ToolOutput {
        output
    }
}

/// 工具抽象：名称 + 描述 + JSON Schema 参数 + 异步执行体。
#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    /// JSON Schema 格式的参数定义。
    fn parameters(&self) -> Value;
    /// 执行工具。入参为模型给出的 JSON 对象。
    async fn call(&self, input: Value) -> Result<ToolOutput>;

    /// 带上下文的执行入口：默认退化到 [`Tool::call`]。
    ///
    /// 需要推送进度（长任务）或读取 agent 身份的工具覆写本方法，
    /// 用 `ctx.emit_progress(..)` 报告中间状态（对应 Java 工具方法的 `ToolEmitter` 注入参数）。
    async fn call_with_context(&self, input: Value, _ctx: &ToolContext) -> Result<ToolOutput> {
        self.call(input).await
    }

    /// 是否只读（对齐 Java `ToolBase.isReadOnly`，默认 false）。
    ///
    /// EXPLORE 模式下只读工具自动放行、非只读一律拒绝；ACCEPT_EDITS 模式下
    /// 只读工具自动放行。该检查在规则引擎的工具自身检查层，bypass-immune。
    fn is_read_only(&self) -> bool {
        false
    }

    /// 规则内容匹配（对齐 Java `ToolBase.matchRule`）。
    ///
    /// `rule_content` 为 `None`（匹配整个工具）由规则引擎直接判定命中；
    /// 非 `None` 时默认不匹配（对齐 Java 默认实现 `ruleContent == null` 才命中），
    /// 需要参数级匹配的工具（如命令前缀）覆写本方法。
    fn matches_rule(&self, _rule_content: &str, _input: &Value) -> bool {
        false
    }

    /// 工具自身权限检查（对齐 Java `ToolBase.checkPermissions`，默认 Passthrough）。
    ///
    /// 返回 `None` = Passthrough，交给规则引擎的规则表与模式默认策略继续裁决；
    /// 返回 `Some` 即终审（bypass-immune，优先于 allow 规则与 BYPASS 模式）。
    async fn check_permissions(
        &self,
        _input: &Value,
        _context: &PermissionContextState,
    ) -> Option<PermissionDecision> {
        None
    }
}

/// 参数校验（对齐 Java `ToolValidator` 的最小语义）：input 为对象且必填字段齐全。
///
/// 校验失败按 Java 语义返回 error 结果（不算 denied），模型可据此修正参数重试。
fn validate_input(schema: &Value, input: &Value) -> std::result::Result<(), String> {
    if !input.is_object() {
        return Err("tool input must be a JSON object".to_string());
    }
    let Some(required) = schema.get("required").and_then(Value::as_array) else {
        return Ok(());
    };
    let missing: Vec<&str> = required
        .iter()
        .filter_map(Value::as_str)
        .filter(|key| input.get(*key).map(Value::is_null).unwrap_or(true))
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!("missing required parameters: {}", missing.join(", ")))
    }
}

/// 工具注册中心：注册、分组、schema 生成、按名分发（对应 Java `Toolkit`）。
#[derive(Default)]
pub struct Toolkit {
    tools: HashMap<String, Arc<dyn Tool>>,
    /// schema-only 外部工具：无本地实现，调用即挂起（对齐 Java `SchemaOnlyTool`）。
    external: HashMap<String, ToolSchema>,
    groups: HashMap<String, ToolGroup>,
    /// 调用拦截器链（注册时按 order 降序排列，保证外层在前）。
    interceptors: Vec<Arc<dyn ToolCallInterceptor>>,
}

impl Toolkit {
    pub fn new() -> Self {
        Self::default()
    }

    // ---------- 注册 ----------

    /// 注册一个工具；同名注册会覆盖旧工具（对齐 Java `registerAgentTool`）。
    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        self.tools.insert(tool.name().to_string(), tool);
    }

    /// 注册 schema-only 外部工具（对齐 Java `registerSchema`）：
    /// 出现在模型可见 schema 中，但本地无实现，调用方负责外部执行。
    pub fn register_schema(&mut self, schema: ToolSchema) {
        self.external.insert(schema.name.clone(), schema);
    }

    /// 注册工具调用拦截器（按 `order` 降序维护链）。
    pub fn add_interceptor(&mut self, interceptor: Arc<dyn ToolCallInterceptor>) {
        self.interceptors.push(interceptor);
        // 稳定排序：order 大在外层；同 order 保持注册顺序
        self.interceptors
            .sort_by_key(|b| std::cmp::Reverse(b.order()));
    }

    // ---------- 查询 ----------

    /// 按名取工具实例。
    pub fn get(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        self.tools.get(name)
    }

    /// 是否已注册指定名称的本地工具。
    pub fn contains(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }

    /// 是否为 schema-only 外部工具。
    pub fn is_external(&self, name: &str) -> bool {
        self.external.contains_key(name)
    }

    /// 全部已注册工具（本地 + 外部）的名称。
    pub fn tool_names(&self) -> Vec<String> {
        self.tools
            .keys()
            .chain(self.external.keys())
            .cloned()
            .collect()
    }

    /// 生成模型可见的工具 schema 列表：只含 active 组的工具 + 未分组工具 + 外部工具。
    pub fn schemas(&self) -> Vec<ToolSchema> {
        let mut schemas: Vec<ToolSchema> = self
            .tools
            .values()
            .filter(|t| self.is_tool_visible(t.name()))
            .map(|t| ToolSchema {
                name: t.name().to_string(),
                description: t.description().to_string(),
                parameters: t.parameters(),
            })
            .collect();
        schemas.extend(self.external.values().cloned());
        schemas
    }

    /// 工具是否对模型可见：未分组工具始终可见；已分组工具需至少一个所属组 active。
    fn is_tool_visible(&self, tool_name: &str) -> bool {
        let groups: Vec<&ToolGroup> = self
            .groups
            .values()
            .filter(|g| g.contains_tool(tool_name))
            .collect();
        groups.is_empty() || groups.iter().any(|g| g.active)
    }

    // ---------- 分组 ----------

    /// 创建并注册工具组（对齐 Java `createToolGroup`）。
    pub fn create_group(
        &mut self,
        name: impl Into<String>,
        description: impl Into<String>,
        active: bool,
        scope: ToolGroupScope,
    ) {
        let group = ToolGroup::new(name)
            .with_description(description)
            .with_active(active)
            .with_scope(scope);
        self.groups.insert(group.name.clone(), group);
    }

    /// 注册现成的工具组（覆盖同名组）。
    pub fn register_group(&mut self, group: ToolGroup) {
        self.groups.insert(group.name.clone(), group);
    }

    /// 把工具加入组；组不存在时报错（对齐 Java `addToolToGroup`）。
    pub fn add_tool_to_group(&mut self, group_name: &str, tool_name: impl Into<String>) -> Result<()> {
        let group = self
            .groups
            .get_mut(group_name)
            .ok_or_else(|| Error::Tool(format!("tool group not found: {group_name}")))?;
        group.add_tool(tool_name);
        Ok(())
    }

    /// 取工具组。
    pub fn get_group(&self, name: &str) -> Option<&ToolGroup> {
        self.groups.get(name)
    }

    /// 设置单个组的激活状态。
    pub fn set_group_active(&mut self, name: &str, active: bool) -> Result<()> {
        let group = self
            .groups
            .get_mut(name)
            .ok_or_else(|| Error::Tool(format!("tool group not found: {name}")))?;
        group.active = active;
        Ok(())
    }

    /// 当前激活的组名列表（对齐 Java `getActiveGroups`）。
    pub fn active_groups(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .groups
            .values()
            .filter(|g| g.active)
            .map(|g| g.name.clone())
            .collect();
        names.sort();
        names
    }

    /// 替换式设置激活组：只保留列出的组为 active，其余停用（对齐 Java `setActiveGroups`）。
    pub fn set_active_groups(&mut self, groups: &[&str]) -> Result<()> {
        for name in groups {
            if !self.groups.contains_key(*name) {
                return Err(Error::Tool(format!("tool group not found: {name}")));
            }
        }
        for (name, group) in self.groups.iter_mut() {
            group.active = groups.contains(&name.as_str());
        }
        Ok(())
    }

    /// 批量更新组的激活状态（对齐 Java `updateToolGroups`）。
    pub fn update_tool_groups(&mut self, groups: &[&str], active: bool) -> Result<()> {
        for name in groups {
            self.set_group_active(name, active)?;
        }
        Ok(())
    }

    /// 移除工具组（不会移除工具本身）。
    pub fn remove_group(&mut self, name: &str) -> Option<ToolGroup> {
        self.groups.remove(name)
    }

    // ---------- 移除 ----------

    /// 移除本地工具（对齐 Java `removeTool`）。
    pub fn remove_tool(&mut self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.remove(name)
    }

    // ---------- 执行 ----------

    /// 按名执行工具（无上下文的轻量路径）。
    pub async fn call(&self, name: &str, input: Value) -> Result<ToolOutput> {
        let ctx = ToolContext::new("", name);
        self.call_with_context(name, input, &ctx).await
    }

    /// 按名执行工具：外部工具挂起 → schema 校验 → 拦截器前置链 → 执行 → 后置链。
    ///
    /// 流程顺序对齐 Java：schema 校验先于拦截器；拦截器改写参数后不重新校验；
    /// 拦截器短路时，已进入链的外层仍能通过 `after` 观察到短路结果（审计语义）。
    pub async fn call_with_context(
        &self,
        name: &str,
        input: Value,
        ctx: &ToolContext,
    ) -> Result<ToolOutput> {
        // 1. 外部工具无本地实现 → 挂起，等待外部执行结果
        if self.is_external(name) {
            return Err(Error::ToolSuspended(format!(
                "tool `{name}` requires external execution"
            )));
        }
        let tool = self
            .tools
            .get(name)
            .ok_or_else(|| Error::Tool(format!("tool not registered: {name}")))?;

        let mut inv = ToolInvocation {
            tool_name: name.to_string(),
            input,
            context: ctx.clone(),
        };

        // 2. 参数校验（对齐 Java：先于拦截器；失败以 error 结果回传，不算 denied）
        if let Err(msg) = validate_input(&tool.parameters(), &inv.input) {
            return Ok(ToolOutput::error(msg));
        }

        // 3. 拦截器前置链（数组按 order 降序，外层在前）
        for i in 0..self.interceptors.len() {
            if let Some(mut out) = self.interceptors[i].before(&mut inv).await {
                // 短路：已进入链的外层可观察最终输出
                for j in (0..i).rev() {
                    out = self.interceptors[j].after(&inv, out).await;
                }
                return Ok(out);
            }
        }

        // 4. 真实执行
        let mut out = tool.call_with_context(inv.input.clone(), &inv.context).await?;

        // 5. 拦截器后置链（逆序：外层最后收到结果）
        for interceptor in self.interceptors.iter().rev() {
            out = interceptor.after(&inv, out).await;
        }
        Ok(out)
    }

    /// 深拷贝工具集（对齐 Java `Toolkit.copy`）：工具/拦截器按 Arc 共享，组状态独立克隆。
    pub fn copy(&self) -> Toolkit {
        Toolkit {
            tools: self.tools.clone(),
            external: self.external.clone(),
            groups: self.groups.clone(),
            interceptors: self.interceptors.clone(),
        }
    }

    // ---------- 会话级激活集 ----------

    /// 模型可见 schema：按会话激活集过滤（对齐 Java `getToolSchemas(activeGroups)`）。
    ///
    /// 未分组工具始终可见；已分组工具需所属组在 `active_groups` 中。
    pub fn schemas_with(&self, active_groups: &[String]) -> Vec<ToolSchema> {
        let mut schemas: Vec<ToolSchema> = self
            .tools
            .values()
            .filter(|t| self.is_tool_visible_with(t.name(), active_groups))
            .map(|t| ToolSchema {
                name: t.name().to_string(),
                description: t.description().to_string(),
                parameters: t.parameters(),
            })
            .collect();
        schemas.extend(self.external.values().cloned());
        schemas
    }

    /// 按会话激活集判断工具可见性（对齐 Java `isActiveTool(toolName, activeGroups)`）。
    fn is_tool_visible_with(&self, tool_name: &str, active_groups: &[String]) -> bool {
        let groups: Vec<&ToolGroup> = self
            .groups
            .values()
            .filter(|g| g.contains_tool(tool_name))
            .collect();
        groups.is_empty() || groups.iter().any(|g| active_groups.contains(&g.name))
    }

    /// META scope 的组名（对齐 Java `getMetaGroupNames`，供元工具参数枚举）。
    pub fn meta_group_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .groups
            .values()
            .filter(|g| g.scope == ToolGroupScope::Meta)
            .map(|g| g.name.clone())
            .collect();
        names.sort();
        names
    }

    /// META 组的激活状态说明（对齐 Java `getNotes`，拼进元工具描述让模型了解当前装备）。
    pub fn activated_notes(&self) -> String {
        let mut activated = String::from("Activated tool groups:\n");
        let mut inactive = String::from("Inactive tool groups:\n");
        let (mut has_activated, mut has_inactive) = (false, false);
        let mut groups: Vec<&ToolGroup> = self
            .groups
            .values()
            .filter(|g| g.scope == ToolGroupScope::Meta)
            .collect();
        groups.sort_by(|a, b| a.name.cmp(&b.name));
        for group in groups {
            if group.active {
                has_activated = true;
                activated.push_str(&format!("- {}: {}\n", group.name, group.description));
            } else {
                has_inactive = true;
                inactive.push_str(&format!("- {}: {}\n", group.name, group.description));
            }
        }
        if !has_activated {
            activated.push_str("No tool groups are currently activated.\n");
        }
        if !has_inactive {
            inactive.push_str("No tool groups are currently inactive.\n");
        }
        activated.push_str(&inactive);
        activated
    }

    /// 原地替换 META 组的激活状态（对齐 Java `ToolGroupManager.replaceMetaActiveGroups`）：
    /// 未列出的 META 组停用，列出的激活；EXTERNAL 组不受影响。
    ///
    /// 注意：会话场景应改用 [`ToolActivation::replace_meta`] 写会话激活集，
    /// 本方法用于无会话的共享配置（对齐 Java 中 group manager 与 per-session state 的分工）。
    pub fn replace_meta_active_groups(&mut self, to_activate: &[String]) -> Result<()> {
        for name in to_activate {
            let group = self
                .groups
                .get(name)
                .ok_or_else(|| Error::Tool(format!("tool group not found: {name}")))?;
            if group.scope != ToolGroupScope::Meta {
                return Err(Error::Tool(format!(
                    "group `{name}` is not manageable by this tool"
                )));
            }
        }
        for group in self.groups.values_mut() {
            if group.scope == ToolGroupScope::Meta {
                group.active = to_activate.contains(&group.name);
            }
        }
        Ok(())
    }

    /// 注册 `reset_equipped_tools` 元工具（对齐 Java `registerMetaTool`）。
    ///
    /// 模型可借它按当前任务重新装备工具组（替换语义）。激活状态写入 `activation`
    /// （会话级，对应 Java `ToolContextState.activatedGroups`），共享 Toolkit 不被修改。
    /// 注册 `reset_equipped_tools` 元工具（激活集写会话状态，见 [`ToolContext::state`]）。
    ///
    /// 注册时快照 META 组名与描述（Java 每次动态读 group manager；
    /// Rust 侧组定义在注册后基本稳定，动态建组场景待 skill 批次再解决）。
    pub fn register_meta_tool(&mut self) {
        let mut meta_groups: Vec<(String, String)> = self
            .groups
            .values()
            .filter(|g| g.scope == ToolGroupScope::Meta)
            .map(|g| (g.name.clone(), g.description.clone()))
            .collect();
        meta_groups.sort_by(|a, b| a.0.cmp(&b.0));
        self.register(Arc::new(ResetEquippedTools {
            meta_groups,
            notes: self.activated_notes(),
        }));
    }
}

/// 会话级工具激活状态（对应 Java `ToolContextState.activatedGroups`）。
///
/// 共享 [`Toolkit`] 只保存工具组定义与默认激活状态（无会话可变状态）；
/// 「当前装备哪些组」属于会话，多个 agent 会话可各自装备不同工具。
#[derive(Debug, Clone, Default)]
pub struct ToolActivation {
    groups: Arc<tokio::sync::RwLock<Vec<String>>>,
}

impl ToolActivation {
    /// 以给定激活组初始化（通常取 `Toolkit::active_groups()`）。
    pub fn new(groups: Vec<String>) -> Self {
        Self {
            groups: Arc::new(tokio::sync::RwLock::new(groups)),
        }
    }

    /// 当前激活组快照。
    pub async fn snapshot(&self) -> Vec<String> {
        self.groups.read().await.clone()
    }

    /// 替换 META 组激活集（对齐 Java `ToolContextState.replaceActivatedGroups`）：
    /// 保留不在 `meta_names` 中的组（EXTERNAL / skill 激活的组），
    /// 把 META 组整体替换为 `to_activate`。
    pub async fn replace_meta(&self, meta_names: &[String], to_activate: &[String]) {
        let mut groups = self.groups.write().await;
        groups.retain(|g| !meta_names.contains(g));
        for name in to_activate {
            if !groups.contains(name) {
                groups.push(name.clone());
            }
        }
    }
}

/// `reset_equipped_tools` 元工具（对应 Java `MetaToolFactory`）。
struct ResetEquippedTools {
    /// META 组快照：(组名, 描述)
    meta_groups: Vec<(String, String)>,
    /// 注册时的激活状态说明（拼进描述）
    notes: String,
}

#[async_trait::async_trait]
impl Tool for ResetEquippedTools {
    fn name(&self) -> &str {
        "reset_equipped_tools"
    }

    fn description(&self) -> &str {
        // 描述常量化后拼上 notes：Box::leak 会泄漏，故直接构造静态描述 + 动态 notes 分开。
        // Tool::description 返回 &str，动态拼接要在 parameters 阶段暴露不了，
        // 因此这里把说明放进 parameters 的 to_activate.description（见下）。
        "Reset your equipped tools based on your current task requirements. \
         These tools are organized into different groups, and you can activate/deactivate them \
         by specifying which groups to keep active.\n\n\
         **Important: The input list is the FINAL set of active tool groups, not incremental \
         changes.** Any group not included in the list will be deactivated, regardless of its \
         previous state.\n\n\
         **Best practice**: Activate only what you need for the current task, and promptly \
         deactivate groups as soon as they are no longer needed to conserve context space."
    }

    fn parameters(&self) -> Value {
        let names: Vec<&str> = self.meta_groups.iter().map(|(n, _)| n.as_str()).collect();
        let mut items = serde_json::json!({ "type": "string" });
        if !names.is_empty() {
            items["enum"] = serde_json::json!(names);
        }
        serde_json::json!({
            "type": "object",
            "properties": {
                "to_activate": {
                    "type": "array",
                    "items": items,
                    "description": format!(
                        "The FINAL list of tool group names to keep active. \
                         Groups NOT in this list will be deactivated. \
                         Pass an empty list to deactivate all groups.\n\n{}",
                        self.notes
                    )
                }
            },
            "required": ["to_activate"]
        })
    }

    /// 直接调用（无会话上下文）：无法改激活集，报错而不是静默无效。
    async fn call(&self, _input: Value) -> Result<ToolOutput> {
        Ok(ToolOutput::error(
            "No session state is attached to this tool call; tool groups cannot be reset.",
        ))
    }

    /// 带上下文执行：激活集写会话状态（`ToolContextState.activated_groups`）。
    async fn call_with_context(&self, input: Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let Some(to_activate) = input.get("to_activate") else {
            return Ok(ToolOutput::error("Missing required parameter: to_activate"));
        };
        let Some(to_activate) = to_activate.as_array() else {
            return Ok(ToolOutput::error("to_activate must be an array of strings"));
        };
        let to_activate: Vec<String> = to_activate
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect();

        // 校验：必须是已存在的 META 组（EXTERNAL 组不可由元工具管理）
        for name in &to_activate {
            match self.meta_groups.iter().find(|(n, _)| n == name) {
                Some(_) => {}
                None => {
                    return Ok(ToolOutput::error(format!(
                        "Group '{name}' is not manageable by this tool."
                    )))
                }
            }
        }

        let meta_names: Vec<String> = self.meta_groups.iter().map(|(n, _)| n.clone()).collect();
        // 激活集写会话状态（对应 Java `ToolContextState.replaceActivatedGroups`）：
        // 保留不在 META 集合里的组（EXTERNAL / skill 激活的组），META 组整体替换。
        let Some(state) = &ctx.state else {
            return Ok(ToolOutput::error(
                "No session state is attached to this tool call; tool groups cannot be reset.",
            ));
        };
        {
            let mut guard = state.lock().unwrap_or_else(|error| error.into_inner());
            guard
                .tool_context
                .replace_activated_groups(Some(&meta_names), Some(&to_activate));
        }

        if to_activate.is_empty() {
            return Ok(ToolOutput::ok("All META tool groups are currently deactivated."));
        }

        let mut result = format!(
            "The currently activated tool group(s): {}.\n",
            to_activate.join(", ")
        );
        // 带描述的组附上工具使用说明（对齐 Java tool-instructions 块）
        let described: Vec<(&str, &str)> = to_activate
            .iter()
            .filter_map(|name| {
                self.meta_groups
                    .iter()
                    .find(|(n, _)| n == name)
                    .map(|(n, d)| (n.as_str(), d.as_str()))
            })
            .filter(|(_, d)| !d.is_empty())
            .collect();
        if !described.is_empty() {
            result.push_str("<tool-instructions>\n");
            for (name, desc) in described {
                result.push_str(&format!("<group name=\"{name}\">{desc}</group>\n"));
            }
            result.push_str("</tool-instructions>");
        }
        Ok(ToolOutput::ok(result))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Mutex;

    struct AddTool;

    #[async_trait::async_trait]
    impl Tool for AddTool {
        fn name(&self) -> &str {
            "add"
        }
        fn description(&self) -> &str {
            "两数相加"
        }
        fn parameters(&self) -> Value {
            json!({
                "type": "object",
                "properties": {
                    "a": { "type": "number" },
                    "b": { "type": "number" }
                },
                "required": ["a", "b"]
            })
        }
        async fn call(&self, input: Value) -> Result<ToolOutput> {
            let a = input["a"].as_f64().unwrap_or(0.0);
            let b = input["b"].as_f64().unwrap_or(0.0);
            Ok(ToolOutput::ok((a + b).to_string()))
        }
    }

    struct NamedTool(&'static str);

    #[async_trait::async_trait]
    impl Tool for NamedTool {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            "测试工具"
        }
        fn parameters(&self) -> Value {
            json!({"type": "object", "properties": {}})
        }
        async fn call(&self, _input: Value) -> Result<ToolOutput> {
            Ok(ToolOutput::ok(self.0))
        }
    }

    #[tokio::test]
    async fn register_and_call() {
        let mut toolkit = Toolkit::new();
        toolkit.register(Arc::new(AddTool));
        assert!(toolkit.contains("add"));

        let schemas = toolkit.schemas();
        assert_eq!(schemas.len(), 1);
        assert_eq!(schemas[0].name, "add");

        let out = toolkit.call("add", json!({ "a": 1, "b": 2 })).await.unwrap();
        assert_eq!(out.content, "3");
        assert_eq!(out.state, ToolResultState::Success);
    }

    #[tokio::test]
    async fn call_unregistered_tool() {
        let toolkit = Toolkit::new();
        let err = toolkit.call("nope", json!({})).await.unwrap_err();
        assert!(matches!(err, Error::Tool(_)));
    }

    /// 参数校验：缺 required 字段 → error 结果（不是 denied），模型可修正重试。
    #[tokio::test]
    async fn missing_required_parameter_returns_error() {
        let mut toolkit = Toolkit::new();
        toolkit.register(Arc::new(AddTool));
        let out = toolkit.call("add", json!({ "a": 1 })).await.unwrap();
        assert_eq!(out.state, ToolResultState::Error);
        assert!(out.content.contains("missing required parameters: b"));
    }

    /// 分组可见性：inactive 组的工具不出现在 schema 中；未分组工具始终可见。
    #[tokio::test]
    async fn group_visibility() {
        let mut toolkit = Toolkit::new();
        toolkit.register(Arc::new(AddTool));
        toolkit.register(Arc::new(NamedTool("admin_tool")));
        toolkit.create_group("admin", "管理工具", false, ToolGroupScope::Meta);
        toolkit.add_tool_to_group("admin", "admin_tool").unwrap();

        // admin 组 inactive → 只有 add 可见
        let names: Vec<String> = toolkit.schemas().into_iter().map(|s| s.name).collect();
        assert_eq!(names, vec!["add"]);

        // 激活后可见
        toolkit.set_group_active("admin", true).unwrap();
        assert_eq!(toolkit.schemas().len(), 2);
        assert_eq!(toolkit.active_groups(), vec!["admin"]);
    }

    /// setActiveGroups 替换语义：只保留列出的组为 active。
    #[tokio::test]
    async fn set_active_groups_is_replacement() {
        let mut toolkit = Toolkit::new();
        toolkit.create_group("g1", "", true, ToolGroupScope::Meta);
        toolkit.create_group("g2", "", true, ToolGroupScope::Meta);
        toolkit.set_active_groups(&["g2"]).unwrap();
        assert_eq!(toolkit.active_groups(), vec!["g2"]);

        let err = toolkit.set_active_groups(&["missing"]).unwrap_err();
        assert!(matches!(err, Error::Tool(_)));
    }

    /// 拦截器：deny 短路（结果状态为 Denied）、order 大的先执行、外层能观察短路结果。
    struct DenyInterceptor {
        order: i32,
        /// 是否检查 `forbidden` 参数并拒绝（用于构造「内层拒绝、外层观察」场景）
        deny: bool,
        log: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait::async_trait]
    impl ToolCallInterceptor for DenyInterceptor {
        fn order(&self) -> i32 {
            self.order
        }
        async fn before(&self, inv: &mut ToolInvocation) -> Option<ToolOutput> {
            self.log.lock().unwrap().push(format!("before({})", self.order));
            if self.deny && inv.input.get("forbidden").is_some() {
                return Some(ToolOutput::denied("策略拒绝"));
            }
            None
        }
        async fn after(&self, _inv: &ToolInvocation, output: ToolOutput) -> ToolOutput {
            self.log.lock().unwrap().push(format!("after({})", self.order));
            output
        }
    }

    #[tokio::test]
    async fn interceptor_chain_order_and_short_circuit() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut toolkit = Toolkit::new();
        toolkit.register(Arc::new(AddTool));
        // 先注册 order=1，再注册 order=5：链应为 5 → 1（大的在外层）
        toolkit.add_interceptor(Arc::new(DenyInterceptor {
            order: 1,
            deny: true,
            log: log.clone(),
        }));
        toolkit.add_interceptor(Arc::new(DenyInterceptor {
            order: 5,
            deny: false,
            log: log.clone(),
        }));

        // 正常放行：before 外层→内层，after 内层→外层
        let out = toolkit.call("add", json!({"a": 1, "b": 2})).await.unwrap();
        assert_eq!(out.state, ToolResultState::Success);
        assert_eq!(
            *log.lock().unwrap(),
            vec!["before(5)", "before(1)", "after(1)", "after(5)"]
        );

        // 内层拒绝（order=1 的 before 短路）：已进入链的外层 order=5 仍能观察到拒绝结果
        log.lock().unwrap().clear();
        let out = toolkit
            .call("add", json!({"a": 1, "b": 2, "forbidden": true}))
            .await
            .unwrap();
        assert_eq!(out.state, ToolResultState::Denied);
        assert_eq!(
            *log.lock().unwrap(),
            vec!["before(5)", "before(1)", "after(5)"]
        );
    }

    /// 拦截器改写参数：改写后的 input 传给工具，不再重新校验。
    struct RewriteInterceptor;

    #[async_trait::async_trait]
    impl ToolCallInterceptor for RewriteInterceptor {
        async fn before(&self, inv: &mut ToolInvocation) -> Option<ToolOutput> {
            inv.input = json!({"a": 10, "b": 20});
            None
        }
    }

    #[tokio::test]
    async fn interceptor_can_rewrite_input() {
        let mut toolkit = Toolkit::new();
        toolkit.register(Arc::new(AddTool));
        toolkit.add_interceptor(Arc::new(RewriteInterceptor));
        let out = toolkit.call("add", json!({"a": 1, "b": 2})).await.unwrap();
        assert_eq!(out.content, "30");
    }

    /// 外部工具（schema-only）：出现在 schema 列表，调用即挂起。
    #[tokio::test]
    async fn external_tool_suspends_on_call() {
        let mut toolkit = Toolkit::new();
        toolkit.register_schema(ToolSchema {
            name: "browser_click".into(),
            description: "浏览器点击".into(),
            parameters: json!({"type": "object"}),
        });
        assert!(toolkit.is_external("browser_click"));
        assert_eq!(toolkit.schemas().len(), 1);

        let err = toolkit.call("browser_click", json!({})).await.unwrap_err();
        assert!(matches!(err, Error::ToolSuspended(_)));
    }

    /// 工具通过 call_with_context 推送进度：ToolProgress 事件进入事件流。
    struct ProgressTool;

    #[async_trait::async_trait]
    impl Tool for ProgressTool {
        fn name(&self) -> &str {
            "long_task"
        }
        fn description(&self) -> &str {
            "长任务"
        }
        fn parameters(&self) -> Value {
            json!({"type": "object", "properties": {}})
        }
        async fn call(&self, _input: Value) -> Result<ToolOutput> {
            Ok(ToolOutput::ok("done"))
        }
        async fn call_with_context(&self, input: Value, ctx: &ToolContext) -> Result<ToolOutput> {
            ctx.emit_progress(ContentBlock::Text {
                text: "进度 50%".into(),
            })
            .await;
            self.call(input).await
        }
    }

    #[tokio::test]
    async fn tool_emits_progress_events() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let ctx = ToolContext {
            reply_id: "reply_1".into(),
            emitter: Some(tx),
            ..ToolContext::new("call_1", "long_task")
        };
        let mut toolkit = Toolkit::new();
        toolkit.register(Arc::new(ProgressTool));

        let out = toolkit
            .call_with_context("long_task", json!({}), &ctx)
            .await
            .unwrap();
        assert_eq!(out.state, ToolResultState::Success);

        let event = rx.recv().await.expect("progress event");
        match event.kind {
            AgentEventKind::ToolProgress {
                reply_id,
                tool_call_id,
                tool_call_name,
                content,
            } => {
                assert_eq!(reply_id, "reply_1");
                assert_eq!(tool_call_id, "call_1");
                assert_eq!(tool_call_name, "long_task");
                assert!(matches!(content, ContentBlock::Text { text } if text == "进度 50%"));
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    /// copy()：组状态独立，工具按 Arc 共享。
    #[tokio::test]
    async fn copy_is_independent_for_groups() {
        let mut toolkit = Toolkit::new();
        toolkit.register(Arc::new(NamedTool("t1")));
        toolkit.create_group("g", "", true, ToolGroupScope::Meta);
        toolkit.add_tool_to_group("g", "t1").unwrap();

        let mut cloned = toolkit.copy();
        cloned.set_group_active("g", false).unwrap();
        assert!(toolkit.get_group("g").unwrap().active);
        assert!(!cloned.get_group("g").unwrap().active);
    }

    /// 元工具：模型按任务重新装备工具组（替换语义），激活集写会话状态而非共享 Toolkit。
    #[tokio::test]
    async fn meta_tool_resets_equipped_groups() {
        let mut toolkit = Toolkit::new();
        toolkit.register(Arc::new(NamedTool("web_search_tool")));
        toolkit.register(Arc::new(NamedTool("db_query_tool")));
        toolkit.create_group("web", "联网检索", false, ToolGroupScope::Meta);
        toolkit.create_group("db", "数据库查询", false, ToolGroupScope::Meta);
        toolkit.add_tool_to_group("web", "web_search_tool").unwrap();
        toolkit.add_tool_to_group("db", "db_query_tool").unwrap();

        toolkit.register_meta_tool();
        let state = Arc::new(Mutex::new(crate::state::AgentState::with_session_id("s")));
        let groups = || {
            state
                .lock()
                .unwrap()
                .tool_context
                .activated_groups()
                .to_vec()
        };

        // 初始无激活组：只有未分组的元工具自身可见
        let names: Vec<String> = toolkit
            .schemas_with(&groups())
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert_eq!(names, vec!["reset_equipped_tools"]);

        // 元工具参数枚举只含 META 组名
        let meta_tool = toolkit.get("reset_equipped_tools").unwrap();
        assert_eq!(
            meta_tool.parameters()["properties"]["to_activate"]["items"]["enum"],
            json!(["db", "web"])
        );

        // 装备 web 组（激活集写会话状态）
        let ctx = ToolContext {
            state: Some(state.clone()),
            ..ToolContext::new("call_1", "reset_equipped_tools")
        };
        let out = toolkit
            .call_with_context("reset_equipped_tools", json!({"to_activate": ["web"]}), &ctx)
            .await
            .unwrap();
        assert_eq!(out.state, ToolResultState::Success);
        assert!(out.content.contains("web"));
        assert!(out.content.contains("<tool-instructions>"));
        assert_eq!(groups(), vec!["web".to_string()]);

        let names: Vec<String> = toolkit
            .schemas_with(&groups())
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert!(names.contains(&"web_search_tool".to_string()));
        assert!(!names.contains(&"db_query_tool".to_string()));

        // 替换语义：只留 db
        toolkit
            .call_with_context("reset_equipped_tools", json!({"to_activate": ["db"]}), &ctx)
            .await
            .unwrap();
        assert_eq!(groups(), vec!["db".to_string()]);

        // 共享 Toolkit 的组激活状态未被元工具修改（激活状态属会话）
        assert!(!toolkit.get_group("web").unwrap().active);
        assert!(!toolkit.get_group("db").unwrap().active);

        // 空列表 → 全部 META 组停用
        let out = toolkit
            .call_with_context("reset_equipped_tools", json!({"to_activate": []}), &ctx)
            .await
            .unwrap();
        assert_eq!(out.content, "All META tool groups are currently deactivated.");
        assert!(groups().is_empty());

        // 不可管理的组名 → error 结果（不是 denied）
        let out = toolkit
            .call_with_context("reset_equipped_tools", json!({"to_activate": ["nope"]}), &ctx)
            .await
            .unwrap();
        assert_eq!(out.state, ToolResultState::Error);
        assert!(out.content.contains("not manageable"));
    }

    /// 元工具在无会话状态时报错而不是 panic。
    #[tokio::test]
    async fn meta_tool_without_state_returns_error() {
        let mut toolkit = Toolkit::new();
        toolkit.create_group("web", "联网检索", false, ToolGroupScope::Meta);
        toolkit.register_meta_tool();
        let ctx = ToolContext::new("call_1", "reset_equipped_tools");
        let out = toolkit
            .call_with_context("reset_equipped_tools", json!({"to_activate": ["web"]}), &ctx)
            .await
            .unwrap();
        assert_eq!(out.state, ToolResultState::Error);
        assert!(out.content.contains("No session state"), "{out:?}");
    }

    /// replace_meta 只替换 META 组，保留 EXTERNAL/skill 激活的组。
    #[tokio::test]
    async fn activation_replace_meta_keeps_external_groups() {
        let activation = ToolActivation::new(vec!["skill_group".into(), "web".into()]);
        activation
            .replace_meta(&["web".into(), "db".into()], &["db".into()])
            .await;
        let mut snapshot = activation.snapshot().await;
        snapshot.sort();
        assert_eq!(snapshot, vec!["db", "skill_group"]);
    }
}
