# rust-agent-scope

**AgentScope 多智能体框架的 Rust 复刻**，外加两个建立在这个框架之上的产品：
**Agent Studio**（智能体桌面应用）与 **OXidea**（内置智能体的 Rust IDE）。

| 内容 | 路径 | 基于什么 |
|------|------|----------|
| **AgentScope Rust 框架** | `crates/`（10 个 crate） | **AgentScope Java 2.0.4**（[agentscope-ai/agentscope-java](https://github.com/agentscope-ai/agentscope-java)）逐模块复刻；Java 类继承改 Rust 枚举组合，异步统一 tokio，逐模块映射见 [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) |
| **Agent Studio** | `apps/agent-studio` | **独立产品**（原创，非 fork），消费本仓库框架层 |
| **OXidea IDE** | `ox-idea/`（39 个 crate） | **OpenIDEA**（Rust 复刻 IntelliJ IDEA 体验的开源 IDE）发展而来，已切断 upstream 独立演进；内置智能体框架层依赖本仓库 `crates/` |

---

## AI 能力

框架层（`crates/`）提供的全部 AI 能力，两个产品直接复用：

- **ReAct 智能体循环**：消息 → 模型 → 工具调用 → 结果回填 → 事件流（token 级流式），
  9 种内容块（文本 / 思考 / 图像 / 音视频 / 工具调用与结果…）；
- **6 个模型提供商**：OpenAI（Chat Completions 与 Responses API）、Anthropic、
  Gemini、Ollama、DashScope——全部支持 SSE 流式、多模态输入与 function calling；
- **工具系统**：Toolkit 注册 / 分组 / schema 生成 / 调用分发；内置浏览器自动化
  （CDP 驱动本机 Chromium：导航、点击、输入、截图、执行 JS）与数据库工具
  （只读查询免确认、写操作走确认）；
- **权限（HITL）**：只读工具自动放行，写操作逐次人工确认，可按工具永久授权；
- **Harness 编排**：workspace（工作区与文件工具）、subagent（子代理声明与调度）、
  task（后台任务）、plan（计划模式）、coordination、memory；
- **会话与状态**：对话状态持久化、会话恢复、上下文压缩（compaction）。

### 框架 crate 一览

| crate | 做什么 |
|-------|--------|
| `agentscope-core` | 框架核心：消息、模型契约（Model/Formatter）、Toolkit、ReAct 循环、middleware 6 扩展点、state/session 层、权限 |
| `agentscope-model-openai` | OpenAI Chat Completions（兼容 DashScope 兼容模式、DeepSeek、vLLM 等一切 OpenAI 协议端点） |
| `agentscope-model-openai-official` | OpenAI Responses API |
| `agentscope-model-anthropic` | Anthropic Messages API |
| `agentscope-model-gemini` | Google Gemini generateContent |
| `agentscope-model-ollama` | Ollama 原生 `/api/chat` |
| `agentscope-model-dashscope` | 阿里 DashScope 原生协议 |
| `agentscope-harness` | Harness 编排层：workspace / subagent / task / plan / 文件工具 / coordination / memory |
| `agentscope-browser` | 浏览器自动化工具（CDP） |
| `agentscope-db` | 数据库工具（db_query / db_dml / 连接档案） |

最小示例：`examples/quickstart`（对应 Java 版 Quickstart，
`cargo run -p quickstart`，需 `OPENAI_API_KEY`）。

---

## 产品

### Agent Studio（`apps/agent-studio`，独立产品）

Tauri + React 桌面应用：定义智能体、和它对话、观察并控制它的每一步。
智能体 = 纯定义（Settings 管理），工作区 = 环境（协同 / 运维 / 数据库 / 测试四类）。
详见 [apps/agent-studio/README.md](apps/agent-studio/README.md)。

### OXidea（`ox-idea/`，基于 OpenIDEA）

Rust 桌面 IDE：编辑器、代码洞察、调试器、Git、终端、数据库、HTTP 客户端，
60+ 语言。内置编码智能体 **OX Hareness**（ACP 协议接入本仓库框架），并能以
同一协议驱动外部智能体（Claude Code、Codex、Gemini CLI…）、通过内置 MCP server
把 IDE 能力暴露给 IDE 之外的 AI。详见 [ox-idea/README.md](ox-idea/README.md)。

---

## 快速开始

```bash
cargo run -p quickstart                      # 跑通最小示例（需 OPENAI_API_KEY）
cargo test --workspace                       # 框架测试
cd apps/agent-studio && pnpm i && pnpm tauri dev   # Agent Studio
cd ox-idea && pnpm i && pnpm tauri dev             # OXidea
```

## License

框架层 `MIT OR Apache-2.0`；各产品目录下的 License 以其自身声明为准
（OXidea 为 Apache 2.0）。
