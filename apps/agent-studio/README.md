# Agent Studio

**独立产品**（原创，非 fork）：基于本仓库 AgentScope Rust 框架构建的智能体
桌面应用（Tauri + React + TypeScript）。框架出处与 AI 能力总览见
[根 README](../../README.md)。

## 是什么

把框架能力产品化：定义智能体、和它对话、观察并控制它的每一步。
产品模型：**智能体 = 纯定义**（Settings 里管理模型/提示词/工具/权限），
**工作区 = 环境**（智能体被引用进工作区后获得真实能力）。

## AI 能力

- **会话对话**：token 级流式输出、工具调用卡片（参数与输出可展开、执行中
  自动展开、结束收起）、思考内容折叠、上下文压缩（长会话不越聊越笨）；
- **权限（HITL）**：只读工具自动放行，写操作逐次人工确认，可按工具永久授权
  （permissions.json）；
- **浏览器**：CDP 驱动本机 Chromium 系浏览器——导航、点击、输入、截图、
  执行 JS，screencast 帧流共享浏览器会话；
- **数据库**：连接档案、只读查询（免确认）与 DML/DDL（人工确认）；
- **运维**：remote_shell 远程服务器管理（审批 + 非交互有界命令）；
- **测试**：需求 ↔ Bug 闭环（绑定共用目录、需求工具四件套）；
- **通知**：任务完成 / 需要确认时弹系统 Toast。

## 工作区

| 类型 | 用途 |
|------|------|
| 协同（collab） | 与智能体结对干活：会话、文件编辑、终端 |
| 运维（ops） | 远程服务器：点击工作区即连接并加载其会话 |
| 数据库（db） | 数据源连接与查询 |
| 测试（test） | 需求 ↔ Bug 闭环 |

## 架构

```
apps/agent-studio/
├── src/            # React + TypeScript 前端（会话 UI、工作区面板、设置）
└── src-tauri/      # Tauri 后端（Rust）：组装框架 crate，AgentEvent 流桥接前端
```

后端 `src-tauri/` 以 path 依赖直接引用仓库根目录 `crates/` 的框架 crate。

## 开发

```bash
pnpm install
pnpm tauri dev      # 开发模式
pnpm build          # 前端构建（tsc && vite build）
pnpm tauri build    # 打包桌面应用
```
