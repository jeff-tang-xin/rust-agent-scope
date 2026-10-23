# AgentScope Rust 架构设计

> 仿写蓝本：AgentScope Java 2.0.4（https://github.com/agentscope-ai/agentscope-java）
> 状态：阶段 2 进行中（2026-10-22）——SSE 流式 / 多模态 / middleware / state 层已落地，session 包待做

## 1. 设计原则

1. **概念对齐**：模块划分、核心抽象、执行模型尽量与 Java 版一一对应，方便对照学习与迁移。
2. **Rust 惯用化**：Java 的类继承（UserMessage/AssistantMessage 子类）改为枚举组合
   （`Role` + `ContentBlock`）；Builder 模式保留；异步统一走 tokio。
3. **提供商模块化**：core 只定义 `Model` 契约，具体提供商（OpenAI/Anthropic/Gemini/Ollama）
   各自独立 crate，与 Java 2.0 的 extensions 拆分一致。
4. **小步演进**：先做通「消息 → 模型 → 工具 → ReAct 循环 → 事件流」主线，
   再按 Java 2.0.4 的真实机制补齐扩展点（middleware / permission 已接入，session 待接）。

## 2. 模块映射

| Java 模块 | Rust crate | 状态 |
|-----------|------------|------|
| `agentscope-core` | `crates/agentscope-core` | ✅ 阶段 1 |
| `agentscope-core`（formatter 层） | `crates/agentscope-core/src/formatter.rs` + openai crate 实现 | ✅ 阶段 2 |
| `agentscope-core`（middleware 层） | `crates/agentscope-core/src/middleware.rs` | ✅ 阶段 2（6 扩展点已接入循环） |
| `agentscope-core`（RuntimeContext） | `crates/agentscope-core/src/context.rs` | ✅ 阶段 2（replyId / session / user / 属性袋） |
| `agentscope-core`（state 层） | `crates/agentscope-core/src/state.rs` | ✅ 阶段 2（已接入循环：槽位加载/落盘） |
| `agentscope-core`（session 层，29 类） | `crates/agentscope-core/src/session/`（14 文件） | 🚧 日志/投影/收件箱/对账/记录器/执行器已做，导出与迁移待做 |
| `agentscope-core`（hook 包，2.0.4 已废弃） | 不复刻 | ⛔ 由 middleware 取代 |
| `agentscope-extensions-model-openai` | `crates/agentscope-model-openai` | ✅ 阶段 2（流式 + 多模态） |
| `agentscope-extensions-model-{anthropic,gemini,ollama,dashscope}` | 待建 | ⬜ |
| `agentscope-harness` | `crates/agentscope-harness` | 🚧 阶段 3 进行中（workspace / subagent / task / plan / agent / subagents_middleware / filesystem / 文件工具 / coordination / memory 全家桶已做；gateway/skill/sandbox/tools/team 等与分布式待做） |
| `agentscope-service` / `agentscope-distribution` | 待建 | ⬜ 远期 |

## 3. 核心抽象

### 3.1 消息（message）

```rust
pub struct Msg {
    id, name, role: Role, content: Vec<ContentBlock>, metadata,
    timestamp, usage, generate_reason,       // 与 Java Msg 字段对齐
}
pub enum Role { System, User, Assistant, Tool }
pub enum ContentBlock {                      // 对应 Java 9 种 Block
    Text { text },  Thinking { thinking },   // 推理模型思考内容（不回传 LLM）
    Image { source, min_pixels, max_pixels },
    Audio { source },
    Video { source, fps, max_frames, min_pixels, max_pixels, total_pixels },
    ToolUse { id, name, input, state, content },
    ToolResult { id, name, output: Vec<ContentBlock>, state, execution_details },
    Hint { id, hint, source },
    Data { source, name },
}
pub enum Source { Base64 { media_type, data }, Url { url, mime_type } }
```

- 一条消息可含多个内容块；`Msg::text()` 拼接纯文本，`Msg::tool_uses()` 过滤工具调用。
- `ToolResult.output` 是块列表（可带多模态），`ToolResultState` 区分
  success / error / interrupted / denied / running / suspended。
- 全量 serde 序列化支持（会话持久化的基础）。

### 3.2 模型（model）

```rust
#[async_trait]
pub trait Model: Send + Sync {
    async fn call(&self, request: ChatRequest) -> Result<ChatResponse>;
    async fn call_stream(&self, request: ChatRequest)
        -> Result<mpsc::Receiver<Result<ModelEvent>>>;   // 默认实现退化为非流式
}

pub enum ModelEvent {
    TextDelta { text },                                  // token 级增量
    ToolUse { id, name, input },                         // 聚合完成的工具调用
    Done { usage, finish_reason },                       // 流结束元信息（最后发出）
}
```

- `ChatRequest = Vec<Msg> + GenerateOptions`（model/temperature/max_tokens/tools）。
- `ChatResponse = Vec<ContentBlock> + Usage + finish_reason`：文本与工具调用统一用内容块表达。
- 流式契约对齐 Java 版：SSE chunk 按 `__fragment__` 占位聚合，拼完整后才产出
  `ToolUse` 事件；`stream_options.include_usage` 无条件携带（DashScope 兼容模式需要）；
  assistant 空 content 发 `""`（vLLM 严格校验）。

### 3.2.1 消息格式化层（formatter）

对应 Java `io.agentscope.core.formatter`。**职责边界**：Model 实现只做传输（HTTP / SSE），
「AgentScope 消息 → 提供商格式」的转换、生成参数装配、工具定义下发全部由 Formatter 完成。

```rust
pub trait Formatter: Send + Sync {
    type RequestMessage;                 // 提供商请求消息
    type RequestParams;                  // 请求体 / builder
    type Response;                       // 提供商响应

    fn format(&self, msgs: &[Msg], options: Option<&GenerateOptions>)
        -> Result<Vec<Self::RequestMessage>>;
    fn parse_response(&self, response: Self::Response) -> Result<ChatResponse>;
    fn apply_options(&self, params: &mut Self::RequestParams,
                     options: &GenerateOptions, defaults: Option<&GenerateOptions>);
    fn apply_tools(&self, params: &mut Self::RequestParams, tools: &[ToolSchema]);
    fn apply_tool_choice(&self, params: &mut Self::RequestParams,
                         tool_choice: Option<&ToolChoice>) {}   // 默认不下发
}
```

- Java 的 `Formatter<TReq, TResp, TParams>` 泛型 → Rust 关联类型；全部同步方法，不加 `async_trait`。
- 共享逻辑（Java `AbstractBaseFormatter`）在 Rust 里是自由函数，不做基类继承：
  `extract_text_content`（含 Text/Hint，跳过 Thinking）、`has_media_content`、
  `role_label`、`should_bypass_history`、`convert_tool_result_to_string`（媒体块转文本引用）。
- `media` 模块对应 Java `MediaUtils`，只保留纯本地逻辑（`is_local_file` /
  `determine_media_type` / `file_to_base64` / `to_file_protocol_url` / 扩展名校验 /
  base64 落临时文件）；URL 下载依赖 HTTP 客户端，由提供商 crate 负责（Java 用 JDK 内建）。
- 结构化输出：`JsonSchema { name, description, schema, strict }` +
  `ResponseFormat::{Text, JsonObject, JsonSchema}`，经 `GenerateOptions.response_format` 下发。
- `ToolChoice::{Auto, None, Required, Specific{tool_name}}` 对应 Java sealed 接口 4 个实现
  （枚举 + `match` 穷尽分派）。
- OpenAI 侧实现：`OpenAIChatFormatter`（`crates/agentscope-model-openai/src/formatter.rs`）
  = Java `OpenAIChatFormatter` + `OpenAIMessageConverter` + `OpenAIResponseParser`；
  `OpenAIChatModel::build_request` 组装请求体时依次调用 `format` / `apply_options` /
  `apply_tools` / `apply_tool_choice`。
- 待建：`OpenAIMultiAgentFormatter` / `OpenAIConversationMerger`（多 agent 历史合并，
  随 MsgHub / 多 agent 批次落地；`should_bypass_history` 钩子已就位）、各 compat provider
  （DeepSeek / GLM / Kimi / MiniMax）的 formatter 变体。

### 3.3 工具（tool）

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters(&self) -> Value;     // JSON Schema
    async fn call(&self, input: Value) -> Result<ToolOutput>;
    // 带上下文的执行入口：长任务工具覆写它，用 ctx.emit_progress 推送进度
    async fn call_with_context(&self, input: Value, ctx: &ToolContext) -> Result<ToolOutput>;
}
```

对照 Java `io.agentscope.core.tool`（43 类 + 4 子包）的核心语义：

| Java | Rust | 说明 |
|------|------|------|
| `AgentTool` | `Tool` | 名称/描述/JSON Schema/异步执行 |
| `ToolGroup` + `ToolGroupScope` | `ToolGroup` + `ToolGroupScope` | 逻辑分组（META=agent 元工具管理 / EXTERNAL=开发者管理）；只有 active 组的工具对模型可见，未分组工具恒可见 |
| `ToolGroupManager` | `Toolkit` 的分组方法 | create/add/update/set_active_groups/remove、`activated_notes()` |
| `ToolContextState.activatedGroups` | `ToolActivation` | **会话级**激活集（Arc\<RwLock\<Vec\<String\>\>\>）；共享 Toolkit 无状态，多会话可各自装备 |
| `MetaToolFactory` | `Toolkit::register_meta_tool` | `reset_equipped_tools` 元工具：替换语义重装备工具组，写会话激活集而非共享 Toolkit |
| `ToolExecutionContext` | `ToolContext` | reply_id / tool_call_id / agent 身份 / metadata / emitter（Java 用 Class 为键的类型化存储，Rust 改显式字段 + JSON，避免 `Box<dyn Any>`） |
| `ToolEmitter` | `ToolContext::emit_progress` | 工具执行中的进度事件（`ToolProgress`），只进事件流不进 LLM 上下文 |
| `ToolCallInterceptor` | `ToolCallInterceptor` | per-call 参数级拦截（before/after 两段式替代 `intercept(inv, next)` 责任链）；`order()` 大者在外层 |
| `ToolValidator` | `validate_input` | 必填字段校验，失败以 error 结果回传（不算 denied） |
| `ToolSuspendException` | `Error::ToolSuspended` | 工具要求外部执行 → `ToolOutput::suspended`（`ToolResultState::Suspended`） |
| `SchemaOnlyTool` | `Toolkit::register_schema` | 外部工具：进 schema、无本地实现、调用即挂起 |

调用链（对齐 Java 顺序）：外部工具判定 → 参数校验 → 拦截器前置链 → 工具执行 → 拦截器后置链。
拦截器短路时，已进入链的外层仍可通过 `after` 观察到短路结果（审计语义）。

- 工具执行失败不中断 ReAct 循环：错误以 `ToolOutput::error` 回传给模型自行决策；
  权限拒绝与执行错误状态区分（`Denied` / `Error`）。
- 待接：MCP 工具（`McpClientManager`）、subagent 工具、ToolExecutor 的超时/并发调度、preset 参数。

### 3.4 记忆（memory）

```rust
#[async_trait]
pub trait Memory: Send + Sync {
    async fn add(&self, msg: Msg) -> Result<()>;
    async fn get_messages(&self) -> Result<Vec<Msg>>;
    async fn clear(&self) -> Result<()>;
}
```

- `InMemoryMemory`：tokio RwLock 保护的消息序列。
- trait 定义为 async，为分布式后端（Redis/MySQL/PostgreSQL）预留空间。

**在 2.0 里 memory 不再是对话真相**（与 Java 一致：Java 2.0 的 `ReActAgent.Builder` 已无
`memory(...)`，内部 30 余处写入全是 `AgentState.context`）。对话上下文的唯一权威位置是
[`AgentState::context`]，`Memory` 只剩两个兼容角色：

| 类型 | 角色 | 对应 Java |
|------|------|-----------|
| `StateBackedMemory` | 读写全部委托 `AgentState.context` 的适配器 | `StateBackedMemory` |
| `AgentStateMemoryView` | 只读懒取视图，写操作报错 | `AgentStateMemoryView` |

`ReActAgent::memory(...)` 仍可配置，但只会**在每次调用结束收到一份历史镜像**（尽力而为，
镜像失败不影响调用结果）。新代码请读 [`ReActAgent::state`] 或直接读状态库/会话日志。

### 3.5 Agent 与 ReAct 循环（agent）

```rust
#[async_trait]
pub trait Agent: Send + Sync {
    async fn call(&self, msg: Msg) -> Result<Msg>;
    async fn stream_events(&self, msg: Msg) -> Result<mpsc::Receiver<AgentEvent>>;
}
```

`ReActAgent::run`（对应 Java ReActAgent 的 reasoning-acting 核心）。**对话真相是
`AgentState.context`**，每次调用先激活会话槽位、再跑循环、最后把状态落进持久层：

```
# 1) 激活槽位（activate_slot）
(user_id, session_id) = ctx.user_id / ctx.session_id ?? default_session_id
state = load(session_id)        # EVENT_LOG: 日志重放; LEGACY: 状态库整份读; 都没有: 新建
slots[(user, session)] = state  # 同槽位共享活状态
recorder = open_recorder()      # EVENT_LOG 模式: 持写者租约; 打开失败直接报错

# 2) 循环（drive → reasoning / acting）
input = middlewares.on_agent(call_input); state.context.push(input)
middlewares.on_agent_state_ready(ctx, input, &state)   # 可读可改真实状态
recorder.start(input, state)                           # run/start, turn/start, input/received
for i in 0..max_iters:
    recorder.step(i)                                   # step/end → step/start
    messages = [system_prompt] + state.context
    tools = toolkit.schemas_with(activation)            # 会话激活集过滤后的工具面
    recorder.checkpoint(before_model)                   # 模型调用前快照
    resp = model.call(messages, tools)                  # reasoning
    assistant = Msg::assistant(resp.content)
    if assistant 无 ToolUse:
        final = assistant.with_generate_reason(ModelStop)
        state.context.push(final); recorder.result(final)
        return final                                    # 最终回复
    state.context.push(assistant)
    for tool_use in assistant:                          # acting
        recorder.append(action/start)                   # 动作开始
        perm = permission.check(name, input)             # Allow / Deny / RequireApproval
        recorder.append(tool/decision, tool/dispatch)    # dispatch 无配对 end ⇒ 结果未知
        output = toolkit.call_with_context(name, input, &ToolContext { emitter, .. })
        recorder.append(action/end)                      # 挂结果，清掉「未知」
        state.context.push(Msg::tool_result(...))        # 失败/拒绝/挂起→对应 state 结果回传
Err(MaxIterations)                                       # 终态记 failed

# 3) 收尾
recorder.finish(state, status)  # turn/output, step/end, turn/<status>, run/end; 否则 state_store.save_if_version
memory_mirror(state.context)    # 仅当配置了 1.x 兼容 memory
```

- `call()`：同步语义，返回最终回复；错误（含超迭代）走 `Result`，且**状态照样落盘**
  （写历史失败同样上报，不把「写不进历史」伪装成成功）。
- `stream_events()`：tokio::spawn 驱动同一循环，事件经 mpsc channel 推送，
  最后一条必为 `AgentResult`；出错时 channel 关闭（错误语义由 `call` 承担）。
- **槽位缓存不替代持久层**：配了持久层时每次调用都重新加载（分布式下同一 `sessionId`
  可能已被别的实例推进过，用本地缓存会写出一份陈旧状态把它覆盖掉）。
- **执行入口（对应 Java `SessionExecution`）**：`call_with_execution(execution, ctx, msgs)` /
  `stream_events_with_execution(...)` 按收件箱命令驱动一次运行——
  `prepare`（准入：submit 开新回合并携带输入 / resume 须为最新未终结回合 / respond 校验
  requestId 仍 pending）→ 每步 `before_step`（把 steer/inject 命令的消息注入对话真相并记
  `inbox/applied`）→ 最终回复处 `continue_or_close`（收件箱还有未消费的 steer 就继续下一轮，
  否则 `tx.close` 关回合）→ `close`。命令的生产者是 harness 层 `AgentSession`（Java 侧），
  core 只消费；Rust 版显式传参而非挂 RuntimeContext 属性袋。
  注意：`prepare` 的 Result 要先绑定再 `if let`——if-let 条件里的 `MutexGuard` 临时值
  会活到整个块结束，块内再 lock 同一把锁就是自死锁。

### 3.6 事件流（event）

对齐 Java `io.agentscope.core.event` 的 32 类事件：信封 `AgentEvent { id, created_at(Epoch 秒),
source, metadata }` + `#[serde(flatten)] kind`（组合代替继承），
`AgentEventKind` 32 变体（`tag = "type"`、SCREAMING_SNAKE_CASE），由 `event_type()` 穷尽映射。
已产出：`AgentStart/End`、`ModelCallStart/End`、`TextBlockDelta`、`ToolCall{Start,Delta,End}`、
`ToolResult{Start,TextDelta,End}`、`ToolProgress`、`ExceedMaxIters`。
其余变体（HITL 确认、多模态增量、subagent 等）已定义待接线。

### 3.7 中间件与运行时上下文（middleware / context）

对齐 Java 2.0.4 的 `io.agentscope.core.middleware`。**不复刻 `hook` 包**：Java 2.0.4 已把整个
`io.agentscope.core.hook` 标为 `@Deprecated(forRemoval = true)`，官方说明由 `MiddlewareBase`
取代（`onAgent` / `onReasoning` / `onActing` / `onModelCall` / `onSystemPrompt`）。

6 个扩展点（全部有默认透传实现，只需覆盖关心的点）：

| 扩展点 | 模式 | Java | Rust |
|--------|------|------|------|
| `on_agent` | 洋葱 | `Flux onAgent(agent, ctx, AgentInput, next)` | `AgentInput -> AgentInput` |
| `on_reasoning` | 洋葱 | 同上，`ReasoningInput` | `ReasoningInput -> ReasoningInput` |
| `on_acting` | 洋葱 | 同上，`ActingInput` | `ActingInput -> ActingInput` |
| `on_model_call` | 洋葱 | 同上，`ModelCallInput`（可换 model） | `ModelCallInput -> ModelCallInput` |
| `on_system_prompt` | 管道 | `Mono<String>` | `String -> String` |
| `on_agent_state_ready` | 通知 | `void`（就地改输入副本） | `&mut Vec<Msg>` + `&Arc<Mutex<AgentState>>` |

**形态映射（为什么不是 `Function<Input, Flux<AgentEvent>> next`）**：Java 的洋葱靠 `next` 回调，
中间件既能改写输入，也能用 Flux 操作符包装下游事件流。Rust 侧事件出口是**推模式**
（`mpsc::Sender`），没有可包装的下游流，故拆成两个正交机制：

- **输入变换**：中间件返回新输入（纯函数），由 `MiddlewareChain` 依次应用 —— 即反模式禁令③
  要求的「返回 patch，由统一方应用」，不传 `&mut Context`；
- **事件旁路**：`EventFilter`（`filter(&mut self, event) -> Vec<AgentEvent>`），Java 里包事件流的
  Flux 操作符的等价物；实例**每次调用新建**（对齐 Java 在 `Flux.defer` 内建 `RoundState`），
  调用结束 `finish()` 清理，异常路径也不泄漏状态。

顺序语义与 Java 一致：`order()` 越大越靠外层，同值保持注册顺序（稳定排序）；`active_points()`
是**参与开关**而非「覆盖了哪些方法」，未激活的扩展点即使覆盖也不调用。

已实现的中间件：`FinalAnswerFilter`（对应 Java `FinalAnswerFilterMiddleware`，只放行最终回复轮的
文本事件，opt-in）。

运行时上下文 `RuntimeContext`（对应 Java `io.agentscope.core.RuntimeContext`）承载 replyId、
sessionId、userId 与调用级属性袋（`Arc<RwLock<_>>`，clone 为浅共享）。`agent.call()` 自动创建默认
上下文，需要会话/用户信息时用 `call_with_context` / `stream_events_with_context`。
`AgentStart` 事件的 `session_id` 由此填充。

### 3.8 权限（permission）

- `PermissionChecker`：`Allow / Deny(reason) / RequireApproval` 裁决契约，**已接入**工具执行路径；
  `Deny` / `RequireApproval` 以 `ToolResultState::Denied` 结果回传给模型（模型可换策略），
  不中断循环。
- 状态数据：`PermissionMode`（default / accept_edits / explore / bypass / dont_ask）、
  `PermissionBehavior`、`PermissionRule`、`AdditionalWorkingDirectory` 与
  `PermissionContextState`（随 `AgentState` 持久化），见 §3.9。
- 待接：`PermissionEngine`（规则匹配引擎）与 `RequireApproval` 的审批事件/恢复机制（对应 Java HITL）。

### 3.9 状态与会话存储（state）

对应 Java `io.agentscope.core.state`（19 类）。**单一真相是 [`AgentState`]**：一个会话一行 JSON，
内含对话缓冲、滚动摘要、reply 标识、迭代计数与四组子上下文。

```rust
pub struct AgentState {
    session_id, user_id, summary, context: Vec<Msg>, reply_id,
    cur_iter, shutdown_interrupted,
    permission_context: PermissionContextState,   // 模式 + allow/deny/ask 规则表
    tool_context: ToolContextState,               // 激活工具组 + 读缓存 + subagent 登记
    tasks_context: TaskContextState,              // 任务 + 需求 + 验证（带 revision 乐观锁）
    plan_mode_context: PlanModeContextState,      // 计划模式开关 + 当前计划文件
}
pub trait AgentStateStore {                       // 寻址：(userId, sessionId) → key → 值
    fn save_raw(.., value: serde_json::Value);
    fn save_list_raw(.., values: Vec<Value>);
    fn get_raw(..) -> Option<Value>;  fn get_list_raw(..) -> Vec<Value>;
    fn get_versioned_raw(..) -> VersionedState<Value>;   // 支持 CAS 的后端
    fn save_if_version_raw(.., expected_version) -> i64;
    fn exists / delete_session / delete_key / list_session_ids;
}
pub trait AgentStateStoreExt: AgentStateStore {   // 泛型便利层（T: State + serde）
    fn save<T>(..);  fn get<T>(..) -> Option<T>;  fn get_list<T>(..);  fn save_if_version<T>(..);
}
```

- **为什么底层收 `Value`**：Java 接口靠 `Class<T>`（反射）反序列化；Rust 若把 `T: State` 写进
  方法签名，trait 就带泛型方法、失去 object safety，没法 `Arc<dyn AgentStateStore>` 换后端。
  所以底层 object-safe 收 `serde_json::Value`，类型安全由 `AgentStateStoreExt` 在编译期保证，
  全程不碰 `Box<dyn Any>`（遵守反模式禁令①）。
- **`State` 是 serde 化的标记 trait**：`pub trait State: Serialize + DeserializeOwned + Send + Sync`
  + blanket impl，等价于 Java 的空标记接口 `State`。
- **CAS / 乐观并发**：`InMemoryAgentStateStore` 支持版本化（`save_if_version` 返回新版本，
  冲突返回 `UNVERSIONED`）；`JsonFileAgentStateStore` 与 Java 一致不支持版本化，
  `save_if_version` 退化为无条件写。
- **任务子上下文**：`TaskContextState` 用 `revision` 做写入门槛（过期修订号 → `ConcurrentModification`），
  `contract_version` 在契约变化（换任务 / 裁决需求 / 重钉被检对象版本）时递增；
  验证结论用 `EvidenceBinding{task_id, contract_version, subject_version}` 钉住，
  三者任一变化即失效。**验证记录不代表任务完成**。
- **工具子上下文**：`ToolContextState` 持久化「会话级激活工具组」与「subagent 登记表」；
  文件读缓存是运行态（LRU，文件数与总字节双上限，命中时校验 mtime）。
  ⚠️ 目前 `tool::ToolActivation` 仍用进程内状态，尚未读写 `ToolContextState`（见 §6）。
- **文件后端布局**：`<root>/<user>/<session>/<key>.json`（单值）/ `<key>.jsonl`（列表）。
  会话目录名做安全化（非 `[A-Za-z0-9_.-]` 的段用 `~` + URL-safe Base64）。

### 3.10 会话日志（session）

`session` 包把「会话」从「整份状态覆盖写」改成**只追加的事件日志**：每次模型调用、
工具执行、状态变化都落成一条 `SessionEvent`，按 batch 提交，链头是一次带围栏
（fence）的 CAS。恢复 = 重放已提交前缀。

| 文件 | 职责 | 对应 Java |
|------|------|-----------|
| `events.rs` | 事件信封 + 内置事件类型表 + 扩展 codec 登记表 | `SessionEvent` / `SessionEventTypes` / `SessionEventCodecRegistry` |
| `key.rs` | 会话身份（用户 / agent / 会话三级）、头部与历史模式 | `SessionKey` / `SessionHeader` / `SessionHistoryMode` |
| `storage.rs` | 原子存储契约（CAS + 发现）与内存实现 | `AtomicSessionStorage` |
| `journal.rs` | 提交链、写者租约与围栏、大负载外置 blob | `JournalSessionLog` |
| `store.rs` | 日志后端；打开即读视图，不抢写者、不修复历史 | `SessionLogStore` / `InMemorySessionLogStore` |
| `projection.rs` | 只读重放：状态快照 + 未决工具调用 + 活跃运行 | `SessionProjection` |
| `interactions.rs` | 未决交互视图；续跑前校验请求合法性 | `SessionInteractions` |
| `turns.rs` | 逻辑回合视图（回合 ≠ 运行） | `SessionTurns` |
| `inbox.rs` | 持久化命令收件箱（独立子日志、短租约、幂等接收） | `SessionInbox` |
| `state_access.rs` | 只读快照 / 有围栏的管理性状态变更 | `SessionStateAccess` |
| `recovery.rs` | 显式对账：取消未决交互、接收外部结论 | `SessionRecovery` |
| `recorder.rs` | 把一次运行写成事件：状态快照 + diff、生命周期钩子、16MB 缓冲 + 1s 心跳 | `SessionRecorder` |
| `execution.rs` | 消费收件箱命令：准入校验、beforeStep 注入去重、回合结算串行化 | `SessionExecution` |

**三条不变式**（比类结构更重要）：

1. **事实不可变**：日志只追加。改状态就追加新的 `state/checkpoint`（新基线），
   不回写旧事件；同一前缀在任何进程里重放结果一致。
2. **未知不等于成功**：`tool/dispatch` 之后没有 `action/end` 的调用进入
   `uncertain_tool_calls`，此时拒绝一切管理性状态变更；对账必须由外部提供
   **恰好覆盖**该集合的结果，框架绝不重放副作用（`cancel_pending` 只合成
   `interrupted` 结果并写明「副作用未经验证」）。
3. **身份不可改**：管理性变更必须保留 `session_id` / `user_id`，否则恢复锚点漂移。

**恢复视图**：`state/checkpoint` / `state/restored` / `migration/baseline` 建立基线，
其后的 `action/end`（工具结果）、`verification/result`（任务验证）与扩展事件是增量；
新基线出现即丢弃旧增量。恢复出的工具结果消息 id 固定为 `recovered_<callId>`，
保证同一前缀重复投影幂等（已存在的 id 与 `suspended` 占位结果不重复追加）。

**收件箱**：执行中的会话已被写者占住，外部投递走 `<session>/inbox` 子日志
（租约 2 分钟、与执行租约互不重叠，重试 99 次后放弃）。`submit` / `resume` /
`respond` 驱动执行，`cancel` / `steer` 只做标记；同幂等键同内容只记一次
（`timestamp` 不参与判等），同键不同内容直接报错。未知事件类型视为日志损坏。

**写者租约**：管理性变更（`state_access` / `recovery`）与收件箱都用 RAII 守卫持租约，
即使中途 panic 也会释放，不让租约泄漏。

**记录器（`recorder.rs`）**——一次运行的事件写入口，对应 Java `SessionRecorder`：

- **周期性快照 + diff，不是逐条埋点**：`capture_state(state, reason)` 先判整体是否变化，
  再按前缀 diff 出被替换段（`context/replaced`）与逐条新增，新增按类型分派
  `tool/result` / `message/assistant` / `message/system` / `message/user`，
  每个 `ToolUseBlock` 额外展开一条 `tool/requested`；三组子上下文按字段 diff
  （`task/changed` / `plan/changed` / `permission/changed`），最后写
  `state/checkpoint{projectorVersion, stateJson, reason}`。
- **生命周期钩子**对齐 Java：`start`（`migration/baseline` / `recovery/applied` /
  `run/start` / `turn/start`|`turn/resumed` / `input/received`）、`step(iter)`（前一步
  `step/end` + 新 `step/start`）、`prepare_model`（`state/checkpoint{before_model}` +
  `request/prepared` + `model/dispatch`）、`observe(event)`（交互/停止/提示/验证等旁路）、
  `finish(state, status)`（`turn/output` / `step/end` / `turn/<status>` / `run/end` + 关闭）。
- **`outcome(GenerateReason)`**：`TOOL_SUSPENDED` / `PERMISSION_ASKING` / `*_STOP_REQUESTED`
  → `suspended`，`MAX_ITERATIONS` / `ALL_TOOLS_DENIED` → `failed`，其余 `completed`/`interrupted`。
- **工程约束**：待写缓冲 16MB 上限、1 秒周期心跳（flush + 续租 + 导出水位）；
  一旦写失败即置 `failure` 锁，后续 `append` 直接抛错——**持久化失败必须炸出**，
  不能降级成「模型调用成功」。

**执行器（`execution.rs`）**——对应 Java `SessionExecution`，把收件箱命令翻译成一次运行：

| 命令 | 准入条件 | 输入来源 |
|------|----------|----------|
| `submit` | 最新回合须已终结、无未决交互 | 命令自带 `messages` |
| `resume` | 须为**最新未终结**回合 | 该回合的 `input/received`（过滤带 `agentscope_confirm_results` 元数据的消息） |
| `respond` | 每个 `requestId` 仍 pending 且属本回合 | 命令携带的确认结果 |

准入通过后写 `inbox/started` 并开回合/运行；`before_step` 按消息 id 去重注入
`inject` / `steer`，注入后立即 `checkpoint(session_input)` + `inbox/applied`；
`continue_or_close` 把回合结算与转向接受串行化。

**仍未接线**：`SessionTranscriptExport` / `SessionExportSink` / `SessionLogExporter`、
`SessionModels` / `SessionModelPolicy`、`SessionMigration` / `LegacyStateLoader`。

### 3.11 harness 层（agentscope-harness，进行中）

对应 Java `agentscope-harness`（19 包 400+ 类），按「本地优先、分布式最后」推进。
已落地四个模块（25 测试）：

- **workspace**（`workspace.rs`）：`WorkspaceManager`（两层读：filesystem 优先、
  本地兜底取并集；原子写「临时文件 + 改名」；列表写入「前缀判断 → 追加/整表重写」；
  TaskRecord JSON 存储 + 孤儿清扫 `fail_orphaned_tasks`）、`LocalFilesystem`
  （Rooted/Sandboxed/Unrestricted 三模式 + PathPolicy 白名单）、
  `WorkspacePathNormalizer`（多前缀最长匹配剥离）、`validate_path`（`..` 穿越拒绝）、
  `PlanModeManager`（`plans/plan-<sessionId>.md` 进入/读写/退出）。
- **subagent**（`subagent.rs`）：`WorkspaceMode`（Isolated/Shared 两值决策表，
  对齐 Java `SubagentUtil.resolveWorkspaceRoot`）、`SubagentDeclaration`
  （workspace.path / URL / inline body 三源互斥校验）、`AgentSpecLoader`
  （frontmatter 解析：块列表、一层嵌套、camelCase 别名、description 缺失即
  malformed 跳过）、`DefaultAgentManager`（注册表 + PRIMARY-only 拒绝 spawn +
  快照整体替换）。
- **task**（`task.rs`）：`BackgroundTask` + `TaskRunSpec`（Local 执行体 /
  Adopted 接管句柄 / Remote 仅登记）、`InMemoryTaskRepository`（put → 轮询
  settle → `find_pending_deliveries` → `mark_delivered`；终态一次性投递；
  `CompletionCallback` 回调；`cancel_task` abort）、`TaskStatus` 五态
  （Pending/Running/Completed/Failed/Cancelled）。
- **plan**（`plan.rs`）：对齐 Java `PlanModeManager`——操作 `AgentState.planModeContext`
  （`plan_active` + `current_plan_file`），经 `agent_state_for` / `save_agent_state_for`
  读写会话状态，默认计划文件 `plans/PLAN.md`，进入幂等。
- **agent**（`agent.rs`）：`HarnessAgent`（编排门面，对应 Java `HarnessAgent` 3207 行）
  + `HarnessAgentBuilder`（对应 `HarnessAgentBuilderSupport` 928 行）。装配链：
  workspace manager → 任务仓库（默认 `WorkspaceTaskRepository`，workspace 持久化 +
  孤儿清扫）→ 子代理注册表（general-purpose 工厂 + `AGENTS.md` 声明加载）→
  ReActAgent delegate。子代理工厂对齐 Java：`derive_child_session_id`（ISOLATED 按
  父身份分桶 `name@sid#uid` / SHARED 单桶）、`build_subagent_sys_prompt`（base +
  SUBAGENT_CONTEXT_SECTION）、`allowlisted_inherited_toolkit`（白名单过滤继承工具）、
  `resolve_declared_workspace`（Isolated 自动建 `agents/<name>/workspace`）、
  `resolve_declared_sys_prompt_base`（AGENTS.md 优先于 inline body）。管理性操作：
  `enter/exit_plan_mode`、`is_plan_mode_active`、`clear_context`（清对话上下文、
  保留会话身份、不取消 in-flight，对齐 Java delegate API）。

- **subagents_middleware**（`subagents_middleware.rs`）：对齐 Java
  `SubagentsMiddleware`——把子代理机制面呈给主代理。稳定指引段（`## Subagents`
  工具用法 + 目录）经 `prepend_to_system_message` 并入首条 SYSTEM 消息
  （字节稳定，prefix-cache 友好；无 SYSTEM 头则头部插入）；易变运行态
  （`<RUNTIME_STATE>` 可见代理目录 + 任务摘要）每轮重建为尾消息，不累积。
  投递推送：`find_pending_deliveries` → `<system-reminder>` 聚合消息
  （`MAX_DELIVERIES_PER_REMINDER` 条封顶，超出尾行计数）→ 先持久化进
  `AgentState.context` 再 `mark_delivered`（崩溃只产生重复投递，不丢消息）→
  本轮同时注入。`is_visible_to_llm` 过滤 hidden/PRIMARY 声明。delegate 引用经
  `OnceLock<Weak<ReActAgent>>` 回填（构建顺序：middleware 先注册进链、
  delegate 后构建，Weak 防循环持有）。

有意差异- **filesystem**（`filesystem.rs`）：`Filesystem` 抽象的三个实现——
  `OverlayFilesystem`（上层写上浮 COW + 共享保护、下层只读合并，LinkedHashMap
  去重上层优先）、`CompositeFilesystem`（最长前缀路由排他 + 精确文件路由 +
  路径重映射 + 跨后端 move）、`InMemoryFilesystem`（测试与子代理隔离工作区）。
- **tools_filesystem**（`tools_filesystem.rs`）：对齐 Java `FilesystemTool` 的
  6 个 LLM 工具（read_file / write_file / edit_file / grep_files / glob_files /
  list_files），共享 `Filesystem` 后端与 `WorkspacePathNormalizer`；
  `bounded_listing` 截断（200 条 / 16K 字符 + 续读指引）、`fuzzy_search`
  三级模糊阶梯（精确 → 去行尾空白 → 折叠空白，逐字节映射回原串区间）、
  `compile_keywords`（phrase/all/any 三模式）。`FilesystemToolSet::all()`
  一次产出 6 个工具供 `Toolkit::register` 装配。
- **coordination**（`coordination.rs`）：周期性后台任务的节流协调——
  `IsolationScope`（Session/User/Agent/Global 四维隔离，缺失/空白 id 归一
  空串共享槽位）、`PeriodicGate` trait + `LocalPeriodicGate`（进程级共享
  最近占用表 + 锁内「读-判-写」原子占用，对齐 Java static map 跨重建存活）。
  `StoreBackedPeriodicGate`（分布式 CAS）留分布式阶段。
- **memory**（`memory.rs` + `middleware_memory.rs` + `tools_memory.rs`）：
  两层记忆模型——Layer 1 每日台账 `memory/YYYY-MM-DD.md`（只追加），
  Layer 2 策展 `MEMORY.md`（consolidation 唯一写入者）。`MemoryConfig`
  统一配置（FlushTrigger Always/Never/Throttled + isolation_scope +
  consolidation 提示词两 `%d` 占位符校验）；`MemoryFlushManager` 每次调用后
  抽取（serialize 跳过 SYSTEM/`<session_context>`/compaction 摘要，工具
  input 截 500、结果截 1000 字符；NO_REPLY 跳过；MEMORY.md 与今日台账作为
  只读上下文去重）；`MemoryConsolidator` 按 watermark（`memory/.consolidation_state`，
  ISO-8601 字典序比较）合并新增每日文件（mtime 未知一律包含），覆写
  MEMORY.md 并推进 watermark；`MemoryBackgroundTasks` 在途计数 +
  `await_quiescence` 供关闭等待静默。两个中间件挂 core 新增的
  `on_agent_complete` 扩展点（对应 Java `on_agent doOnComplete`）：
  `MemoryFlushMiddleware` per-conversation 队列合并（在途任务吸收后续调用）
  + 节流；`MemoryMaintenanceMiddleware` 过期台账 move 到 `memory/archive/`
  （仅 `YYYY-MM-DD.md` 形态）+ consolidation，经 PeriodicGate 节流。
  3 个 LLM 工具：`memory_save`（MEMORY.md + 今日台账双写，`-` 行计数）、
  `memory_get`（1-based 闭区间分页 `N|line`）、`memory_search`
  （大小写不敏感 phrase/all/any，`Source: path#N: line`，上限 100 条）。

- **tools**（`tools.rs`，批次 A）：对齐 Java `HarnessTools` 的 8 个 LLM 工具——
  `ShellExecuteTool`（working_directory 校验含 Windows 盘符与 UNC 前缀、
  超时与输出截断）、`WaitAsyncResultsTool`（轮询 `TaskRepository::get_task`
  惰性结算句柄——工具侧直接读状态永不推进，必须经 repo 刷新）、
  `AgentGenerateTool`（自维护 generated 名单防重名，`manager.has_agent` 双查）、
  `TaskCreate/TaskGet/TaskList/TaskUpdate/TaskSendMessage`（经
  `WorkspaceTaskRepository` 持久化 + 投递）。`task_result_state` 映射：
  Failed→Error、Cancelled→Interrupted、Pending/Running→Running、
  Completed→Success。
- **skill**（`skill.rs` + `tools_skill.rs`，批次 B）：对齐 Java skill 生态——
  `AgentSkill`（name/description/license/allowed-tools/metadata，frontmatter
  解析复用 `subagent::frontmatter_string`）、`WorkspaceSkillRepository`
  （只读/可写两形态；list/read/save/delete/move；glob 路径经
  `normalize_glob_path` 归一为相对技能根——Local 带前缀与 InMemory/Overlay
  相对 cwd 两种形态都稳健）、`SkillSecurityScanner`（规则表 + 三级裁决
  Safe/Warning/Dangerous，Dangerous 阻断写入）。
  `SkillManageTool` 六动作（create/edit/patch/write_file/remove_file/delete）：
  默认落草稿目录 `skills/_drafts`（`auto_promote` 可直写主目录）、name 与
  frontmatter 一致性校验、重名拒绝、patch 唯一命中/多命中需 replace_all/
  三级模糊阶梯（复用 filesystem 的 fuzzy 语义）、子文件路径白名单
  （scripts/references/templates/assets）、安全扫描失败即回滚（先写后扫、
  扫不过删净）、usage store（草稿/补丁遥测）+ audit log（`manage` 条目）。
  `ProposeSkillTool`（name/description/body/scripts 批量上传，落草稿目录）。
  `HarnessPlatformTools`（平台工具名识别：skill_manage/propose_skill/team/
  agent_spawn/wait_async_results 等不进子代理白名单）。
- **team**（`base_store.rs` + `team.rs` + `tools_team.rs`，批次 C1/C2）：对齐
  Java team 包与 `TeamTool`——`BaseStore` trait + `InMemoryStore`（JSON 值
  存储，camelCase serde 对齐 Java 字段名）；`team.rs` 的 Team/Member/Task/
  Message 模型与 `TeamManager`（lead 身份、成员注册、任务分派/流转、
  消息广播）；`TeamTool`（lead 上下文注入 + 18 个动作别名方法对齐 Java，
  动作经 `team_action_tools` 注册进 Toolkit，门控：无活动任务时拒绝
  assign/complete 类动作）。
- **agent_spawn**（`tools_agent_spawn.rs`，批次 C3）：对齐 Java
  `AgentSpawnTool` 的本地路径——三工具 `agent_spawn` / `agent_send` /
  `agent_list` 共享 `AgentSpawnCore`（进程内实例表 + label 反查表）。
  spawn：PRIMARY-only 拒绝、深度上限 3、persist_session 声明走
  `deterministic_hash`（SHA-256 前 6 字节 hex，对齐
  `SessionIdUtils.deterministicHash`，跨进程可复现 key）否则 UUID；
  label 冲突检查（persist 同 key 复用放行）；实例取建 computeIfAbsent
  语义（锁外建 + 锁内双检）；SpawnEntry 经 `AgentState::put_spawn_entry`
  登记进父会话状态。执行：同步等待（缺省 30s、上限 600s）+
  超时晋升（超时→`TaskRunSpec::Adopted` 登记进 TaskRepository 返回
  task_id；forceSync→中断返回 status: timeout）；`timeout_seconds=0`
  走 `TaskRunSpec::Local` 后台提交。send：agent_key/label 二选一，
  label 反查缓存 miss 后从父会话状态 spawn 登记表恢复（小写匹配），
  实例丢失时经 `create_agent_if_present` 懒重建（对齐
  `tryRestoreFromState`）。per-call 覆盖（force_sync /
  force_sync_timeout_seconds）从 `ToolContext::metadata` 读取。
  与 Java 的有意差异：远端声明（url）报 not supported（transport 随
  分布式批次）、gateway expose 解析后暂不生效、子代理事件转发与
  plan/DENY 传播待后续接线。

（`lib.rs` 文档声明）：不复刻 WorkspaceIndex（SQLite best-effort）、
NamespaceFactory/remote filesystem（数据路径无命名空间前缀）；TaskRecord 时间
字段用 epoch 毫秒（Java 为 ISO-8601 字符串）；投递消息持久化走
`state_for`/`save_agent_state_for`（Java 为 `RuntimeContext.resolveAgentState`）。
HarnessAgent 本批做 workspace/subagent/task/plan/subagents-middleware 编排；
sandbox/skill/MCP/teams/filesystem 工具留扩展点。

**待做**：sandbox / gateway / bus / context / observation /
verification / artifact / session / testing 包、
分布式（Remote* / AgentProtocol* / DistributedStore / StoreBackedPeriodicGate、
AgentSpawnTool 远端 transport）。

## 4. 技术选型

| 依赖 | 用途 |
|------|------|
| tokio | 异步运行时（spawn / mpsc / RwLock） |
| async-trait | trait 异步方法 + dyn 分发（Model/Tool/Memory/Agent） |
| serde / serde_json | 消息与工具参数序列化 |
| thiserror | 统一错误类型 |
| base64 | 媒体块 base64 编解码（formatter::media，仅 core） |
| uuid | Msg id |
| reqwest | OpenAI 兼容 HTTP 调用（仅 model-openai crate；`stream` feature 支持 SSE） |
| futures-util | SSE 字节流 `Stream` 适配（仅 model-openai crate） |

## 5. 验证方式

- `cargo build`：workspace 全量编译通过。
- `cargo test`：443 个单元测试全绿（core 242 + harness 184 + model-openai 17），`cargo clippy --workspace --all-targets` 0 警告，
  `cargo doc --no-deps` 0 警告。
  - core 88 个：消息构造/序列化往返、formatter 共享逻辑（文本提取跳过 Thinking、
    工具结果转字符串、媒体引用、结构化输出线格式、media 工具）、Toolkit 分组与激活、
    拦截器链、参数校验、元工具、Memory 读写、RuntimeContext（replyId/属性浅共享）、
    中间件链（order 顺序、扩展点开关、提示词管道、输入改写、状态就绪通知、
    FinalAnswerFilter 过滤语义）、state 层（AgentState JSON 往返与缺字段补默认、
    任务修订号乐观锁/换任务清空/需求去重与裁决/证据绑定失效、激活组顺序与去重、
    读缓存条数与 mtime 淘汰、内存 store 的匿名桶与 CAS 语义、文件 store 往返/
    jsonl 追加与整表重写/非法段编码的可逆性/删除与 SessionInfo）、
    session 层（事件信封 wire 名与序号连续性、内置类型表、扩展 codec 保留名/重复名校验、
    会话身份编码与头部格式、日志提交链与批内连续性、围栏写者与租约接管、永久封存、
    提交前缀幂等重放、大负载外置 blob、导出水位单调推进、只读检查不抢写者、
    投影（空日志 / 检查点往返 / dispatch 未收尾 / action_end 回填 / 重复与 suspended 不重复追加 /
    新基线丢弃旧增量 / 未知 required 事件失败关闭 / 验证记录回放进任务上下文）、
    未决交互集合与续跑回合校验、逻辑回合视图（运行生命周期 / 续跑不改位置 / 取消 /
    输入按 id 去重 / 收件箱命令追踪 / 基线重置）、收件箱（幂等接收与冲突、open/close 归属、
    rejected/handled 退出待执行集、未知事件失败关闭、租约与执行写者互不干扰）、
    状态访问（空日志兜底 / 检查点提交 / 未决工具调用拒绝 / 未决交互时禁止换上下文 /
    身份不可改 / 过期快照拒绝 / 子上下文变更记事实）、恢复（取消合成 interrupted 结果 /
    重复取消幂等 / 拒绝 steer 指令 / 对账须精确覆盖））、
    记录器（快照 diff：新增消息按类型分派 / 工具调用展开 requested / 前缀替换 /
    三组子上下文字段 diff；`outcome` 四值映射；失败后 `append` 抛错）、
    执行器（submit 开回合并携带输入 / resume 须为最新未终结回合 / respond 校验
    requestId 仍 pending）、权限状态 wire 值、ReAct 循环（MockModel 驱动：带工具调用、
    直接回答、超迭代、工具报错不中断、流式事件顺序、中间件改写输入/替换模型/
    sessionId 透传、事件过滤器抑制中间轮文本、permission 拒绝回传、Session 存取）、
    **状态接线**（同一默认会话累积历史 / LEGACY 状态库跨实例重载 / EVENT_LOG 写日志并重放
    历史、日志首末事件与 `action/start→tool/dispatch→action/end` 顺序 / 工具调用后无
    「结果未知」/ 1.x memory 收到历史镜像 / `on_agent_state_ready` 拿到可读可改的真实状态 /
    持久层配置时空闲槽位被淘汰、无持久层时槽位保留）、
    **执行入口**（submit 命令驱动运行并关闭回合 / resume 未知回合被拒且命令留在收件箱 /
    运行中途 steer 使结算后继续一轮、转向消息由 `before_step` 注入对话真相）、
    **工具激活集**（元工具把激活组写进会话状态、reasoning 从状态读激活集）。
  - model-openai 17 个：Msg ↔ OpenAI 协议消息转换（含 name sanitize、assistant 空
    content 发 `""`、SYSTEM 携 ToolResult 转 tool 角色）、响应解析（含 tool_calls）、
    多模态映射（image → image_url、非 image MIME 文本占位）、apply_options 回退链、
    apply_tools / apply_tool_choice 四种策略、response_format 下发、
    SSE 流解析与工具调用分片聚合。
- 端到端：`cargo run -p quickstart`（需真实 API key，未在本次验证）。

## 6. 已知边界与风险

1. **middleware 的 Java 内置件未全做**：`TaskReminderMiddleware` / `TaskContextProjection`
   （依赖 AgentState 的任务投影）与 `ModelRequestPreparer`（模型请求准备器 + `context_build`
   事件）尚未实现，随 session/state 批次接入；`hook` 包按官方废弃声明不复刻。
2. **中间件只有输入变换与事件旁路**：Java 的洋葱允许在 `next` 之后执行副作用，Rust 侧用
   `EventFilter` 观察下游事件来等价实现；需要「替换下游事件流」的复杂场景（如把一次调用
   变成多次）暂不支持。
3. **流式错误语义**：`stream_events` 出错时 channel 直接关闭，错误不经事件流传递
   （错误语义由 `call` 承担）；后续可引入 `AgentEvent::Error` 变体。
4. **并发安全**：`ReActAgent` 不可变共享（字段全 Arc），同一 agent 可并发 `call`；
   状态按 `(userId, sessionId)` 分槽，**同槽位串行、异槽位并行**（与 Java `stateCache` +
   per-slot 锁一致）。跨进程并发写同一会话由日志的写者租约 + 链头 CAS 拦下
   （后者失败即报错，不做静默覆盖）。
5. **quickstart 未做真实网络验证**：需要 API key，留给使用者按环境变量配置后运行。
6. **formatter 未覆盖多 agent 合并**：Java 的 `OpenAIMultiAgentFormatter` /
   `OpenAIConversationMerger`（多 agent 历史合并、`_bypass_multiagent_history_merge` 旁路）
   尚未实现——当前只有 `should_bypass_history` 钩子；随 MsgHub / 多 agent 批次补齐。
7. **formatter 的 provider 变体未做**：Java 为 DeepSeek / GLM / Kimi / MiniMax 各提供了
   formatter 子类（如 DeepSeek 不支持 `strict`、Kimi 无 `tool_choice`），Rust 侧目前只有
   基础 `OpenAIChatFormatter`，`supports_strict` 之类的开关留待接入这些提供商时再抽。
8. **`GenerateOptions` 参数面窄于 Java**：只有 model/temperature/max_tokens/tools/
   tool_choice/response_format；缺 top_p、seed、penalties、parallel_tool_calls、
   reasoning_effort、additional_body_params 等（formatter 侧留好了扩展位置）。
9. **SSE 解析未覆盖真实服务商长流**：流式解析以单元测试（构造 SSE 文本）验证，
   未对真实 DashScope/OpenAI 长响应做过联调；分片边界（跨 chunk 的 `data:` 行拆分）
   已按行缓冲处理，但极端情况（多行 data、CRLF）依赖测试覆盖的形态。

10. **状态已接入循环，工具激活集也已入状态**：`AgentState.context` 是唯一对话真相，
    `ReActAgent` 按槽位加载/落盘状态，`on_agent_state_ready` 投递真实可改状态；
    元工具（`reset_equipped_tools`）把激活组写进会话状态的 `ToolContextState`，
    reasoning 从状态读激活集——跨进程续跑时工具装备状态不再丢失。
    仍未接入的是 `ToolCallState` 的迁移语义与 `AgentState.summary` 的滚动摘要。
11. **state 层与 Java 的存储差异**：文件后端列表写入用「前缀判断 + 追加 / 整表重写」代替
    Java 的 `.hash` 旁路文件（语义等价，写入放大略高）；会话目录名对非法字符加 `~` 前缀
    以保证编解码可逆（Java 的 Base64 段与安全字符集重叠，无法反解）。内存后端不持久化，与 Java 一致。
12. **permission 只有状态与裁决契约**：`PermissionEngine`（规则匹配、pattern 语法、
    模式→行为映射）未实现，`RequireApproval` 的审批事件与恢复也未做；
    `PermissionContextState` 目前只是可持久化的配置载体。
13. **任务子上下文无调用方**：`TaskContextState` 的 `begin_task` / `propose` / `decide` /
    `record_verification` 已按 Java 语义实现并测试，但还没有任务工具与
    `TaskReminderMiddleware` 去驱动它。
14. **执行入口已接，生产者留待 harness**：`ReActAgent::call_with_execution` /
    `stream_events_with_execution` 消费收件箱命令（`prepare` 准入 → 每步 `before_step`
    注入 steer/inject → 最终回复处 `continue_or_close` 结算 → `close`），「提交 → 中断 →
    续跑」闭环在 core 层已通。Java 侧的生产者是 harness 层 `AgentSession`（从收件箱取
    命令、装配 `SessionExecution` 挂进 RuntimeContext），Rust 版随 harness 批次补齐。
15. **事件日志重放已做增量**：`SlotEntry` 缓存上次投影快照，`load_state` 优先走
    `SessionProjection::advance`（只重放快照之后的事件），无缓存（新槽位/只读查询）才
    全量 `read`。长会话的重复重放开销已消除；快照随槽位淘汰一起释放。
16. **session 导出与迁移已做，LegacyStateLoader 未做**：`SessionLogExporter`
    （外部读者按水位增量导出）、`SessionModelPolicy`（模型切换的事实记录）、
    `SessionMigration`（LEGACY 整份快照迁入事件日志）均已实现并有测试；
    145 行旧格式的 `LegacyStateLoader` 迁移器尚未复刻。
