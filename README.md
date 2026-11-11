# rust-agent-scope

AgentScope 多智能体框架的 Rust 复刻，外加两个建立在这个框架之上的产品：
**Agent Studio**（智能体桌面应用）与 **OXidea**（Rust IDE，内置智能体）。

> **框架出处**：本仓库的框架层（`crates/`）是对 **AgentScope Java 2.0.4**
> （[agentscope-ai/agentscope-java](https://github.com/agentscope-ai/agentscope-java)）
> 的 Rust 仿写。模块划分、核心抽象、执行模型与 Java 版一一对应；
> Java 的类继承改为 Rust 枚举组合，异步统一走 tokio。
> 逐模块的映射关系、有意差异与进度见 [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)。

## 仓库包含什么

本仓库由三部分组成：**框架**（`crates/`，10 个 crate）、**框架示例**（`examples/`）、
**两个产品**（`apps/agent-studio` 与 `ox-idea/`）。

### 框架层 `crates/`（独立可用，不依赖任何产品）

| crate | 做什么 |
|-------|--------|
| `agentscope-core` | 框架核心（对应 Java `agentscope-core`）：消息 `Msg` 与 9 种内容块、模型契约 `Model`/`Formatter`、工具注册与调用 `Toolkit`、ReAct 循环与事件流、middleware 6 扩展点、state/session 层、权限（HITL 人工确认） |
| `agentscope-model-openai` | OpenAI Chat Completions 协议提供商（兼容 DashScope 兼容模式、DeepSeek、vLLM 等一切 OpenAI 兼容端点） |
| `agentscope-model-openai-official` | OpenAI Responses API 提供商 |
| `agentscope-model-anthropic` | Anthropic Messages API 提供商 |
| `agentscope-model-gemini` | Google Gemini generateContent 提供商 |
| `agentscope-model-ollama` | Ollama 原生 `/api/chat` 提供商 |
| `agentscope-model-dashscope` | 阿里 DashScope 原生协议提供商 |
| `agentscope-harness` | Harness 编排层（对应 Java `agentscope-harness`）：workspace / subagent / task / plan / 文件工具 / coordination / memory |
| `agentscope-browser` | 浏览器自动化工具：CDP 驱动本机 Chromium 系浏览器，导航、读页面、点击、输入、截图、执行 JS |
| `agentscope-db` | 数据库工具：`db_query`（只读 SQL，免确认）、`db_dml`（写操作，走人工确认）、连接档案目录 |

全部模型提供商支持 SSE 流式、多模态输入与 function calling；core 只定义契约，
提供商各自独立 crate，按需组合。

### 示例 `examples/`

| 目录 | 做什么 |
|-------|--------|
| `examples/quickstart` | 最小可运行示例（对应 Java 版 Quickstart）：消息 → 模型 → 工具 → ReAct 循环 → 事件流。`cargo run -p quickstart` |

### 产品层

| 目录 | 是什么 | 做什么 |
|------|--------|--------|
| `apps/agent-studio` | **Agent Studio** 桌面应用（Tauri + React） | 框架之上的参考产品：智能体定义与管理、会话对话、工具权限确认（HITL）、运维 / 协同 / 数据库 / 测试四类工作区、浏览器与数据库面板。详见 [apps/agent-studio/README.md](apps/agent-studio/README.md) |
| `ox-idea` | **OXidea** IDE（独立 Cargo workspace，39 个 crate） | 完整的 Rust 桌面 IDE：编辑器、代码洞察、调试器、Git、终端、数据库、HTTP 客户端，支持 60+ 语言；内置智能体 OX Hareness 通过 ACP 协议驱动，其框架层（`agentscope-acp`）直接依赖本仓库 `crates/` 的全部框架 crate。详见 [ox-idea/README.md](ox-idea/README.md) |

### 文档与其他

| 路径 | 做什么 |
|------|--------|
| `docs/ARCHITECTURE.md` | 架构设计：Java ↔ Rust 模块映射表、核心抽象对照、设计原则与进度 |
| `v2/zh` | 预留的中文文档 / 案例目录（当前为空骨架） |

## 快速开始

```bash
# 跑通最小示例（需设置 OPENAI_API_KEY，兼容端点见 examples/quickstart/src/main.rs 头注释）
cargo run -p quickstart

# 跑框架测试
cargo test --workspace

# Agent Studio 开发模式
cd apps/agent-studio && pnpm install && pnpm tauri dev

# OXidea 开发模式（独立 workspace）
cd ox-idea && pnpm install && pnpm tauri dev
```

## License

`MIT OR Apache-2.0`（框架层）。各产品目录下的 License 以其自身声明为准。
