# Agent Studio

基于 [AgentScope Rust 框架](../../README.md)构建的智能体桌面应用（Tauri + React + TypeScript）。

> **框架出处**：本应用的全部智能体能力来自本仓库的框架层
> `crates/agentscope-core`（消息 / 模型 / 工具 / ReAct 循环 / 权限）与
> `agentscope-harness`、`agentscope-browser`、`agentscope-db`、六个模型提供商 crate。
> 框架是对 **AgentScope Java 2.0.4**
> （[agentscope-ai/agentscope-java](https://github.com/agentscope-ai/agentscope-java)）
> 的 Rust 复刻，架构映射见 [docs/ARCHITECTURE.md](../../docs/ARCHITECTURE.md)。

## 是什么

Agent Studio 把框架能力产品化：定义智能体、和它对话、观察并控制它的每一步。

- **智能体 = 纯定义**：在 Settings 里管理模型、提示词、工具与权限配置；
- **工作区 = 环境**：智能体被引用进工作区后获得文件、终端、数据库等真实环境，
  工作区分协同（collab）、运维（ops）、数据库（db）、测试（test）四类；
- **会话**：流式对话、工具调用卡片（参数与输出可展开）、上下文压缩；
- **权限（HITL）**：只读工具自动放行，写操作逐次人工确认，可按工具永久授权；
- **运维**：远程服务器管理（remote_shell，非交互有界命令）；
- **数据库**：连接档案、只读查询与写操作确认；
- **测试**：需求 ↔ Bug 闭环（绑定目录、需求工具四件套）。

## 开发

```bash
pnpm install
pnpm tauri dev      # 开发模式
pnpm build          # 前端构建（tsc && vite build）
pnpm tauri build    # 打包桌面应用
```

前端源码在 `src/`（React + TypeScript），Tauri 后端在 `src-tauri/`（Rust，
依赖本仓库 `crates/` 的框架 crate）。
