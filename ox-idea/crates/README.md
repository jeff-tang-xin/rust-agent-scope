# OXidea crates

OXidea（**基于 OpenIDEA** 发展而来，见 [上级 README](../README.md)）的 IDE 内核
由 **39 个 crate** 组成（`ox-idea/crates/`），全部在 `ox-idea` 这个独立 Cargo
workspace 内。按职责分组如下。

> 其中 `agentscope-acp` 直接依赖仓库根目录 `crates/` 的 AgentScope Rust 框架
> （框架出处与 AI 能力总览见[根 README](../../README.md)）。

## 内核与文本

| crate | 做什么 |
|-------|--------|
| `ide-core` | 全 IDE 共享词汇：ids、文本坐标、事件、错误、设置 |
| `ide-text` | Rope 文档、变更集、UTF-16/字节坐标转换 |
| `ide-vfs` | 文件系统访问（目录列表、编码探测读写）——UI 不直接碰磁盘 |
| `ide-workspace` | 项目模型：打开的文档、设置分层、最近项目、会话状态 |

## 语言与代码洞察

| crate | 做什么 |
|-------|--------|
| `ide-language` | 语言注册表（`languages/*.toml`）、语言检测、tree-sitter 结构 |
| `ide-syntax` | tree-sitter 结构服务：节点/表达式查找、语法错误、作用域 |
| `ide-analysis` | 项目级分析：重复代码检测等 |
| `ide-index` | 后台项目索引（文件 + 符号），redb 持久化、模糊查询 |
| `ide-search` | Find/Replace in Files（ripgrep 引擎） |
| `ide-spell` | 拼写检查（注释/字符串/标识符取词） |
| `ide-lsp` | 语言服务器管理：生命周期、文档同步、LSP→IDE 语义映射 |
| `ide-jsonrpc` | Content-Length 帧传输 + JSON-RPC 2.0（LSP/DAP 共享） |
| `ide-tools` | 语言服务器/调试适配器安装器：注册表、检测、下载 |

## 运行与调试

| crate | 做什么 |
|-------|--------|
| `ide-run` | 运行配置：检测、持久化、进程生命周期、输出流 |
| `ide-dap` | DAP 调试客户端：会话、断点、线程、栈帧、变量 |
| `ide-build` | Maven/Gradle 工具窗口模型与构建动作 |
| `ide-coverage` | Run with Coverage（cargo-llvm-cov、vitest/jest/pytest…） |
| `ide-profiler` | Run with Profiler（Node `--cpu-prof`、py-spy…） |
| `ide-sdk` | SDK/工具链发现（Python、Node、Rust、Go、TypeScript 解释器） |

## 智能体

| crate | 做什么 |
|-------|--------|
| `agentscope-acp` | OX Hareness：把 AgentScope agent（本仓库框架）以 ACP v1 暴露给 IDE |
| `ide-agent` | ACP 驱动外部编码智能体（Claude Code、Codex、Gemini CLI…）：任务、会话、worktree |
| `ide-mcp` | IDE 的 MCP server：IDE 外的 AI agent（Streamable HTTP）调用 IDE 能力 |

## 工具窗口与集成

| crate | 做什么 |
|-------|--------|
| `ide-terminal` | 集成终端（原生 PTY，xterm.js 渲染） |
| `ide-vcs` | Git 集成：gitoxide 进程内读（status/diff/blame/log），git CLI 写 |
| `ide-github` | GitHub PR（`gh` CLI） |
| `ide-docker` | Docker（Services 工具窗口，`docker` CLI） |
| `ide-android` | Android 设备与 logcat（`adb` CLI） |
| `ide-ssh` | SSH 连接：配置、host key、认证、远程命令、SFTP |
| `ide-remote` | SSH 远程开发：SFTP 部署（mappings、上传下载同步）与远程运行 |
| `ide-db` | 数据库工具：数据源、连接、内省、查询执行 |
| `ide-http` | HTTP Client：IntelliJ 风格 `.http` 请求文件、环境、历史 |
| `ide-notebook` | 原生 notebook 文档 + 自有 Jupyter kernel 会话 |
| `ide-web` | 内置静态 web server（127.0.0.1）与浏览器集成 |
| `ide-clipboard` | OS 剪贴板文件列表（Explorer ↔ 项目视图） |
| `ide-fonts` | 本机字体发现（CoreText/DirectWrite/fontconfig），供字体选择器 |

## 插件系统

| crate | 做什么 |
|-------|--------|
| `ide-extension` | 插件内核：manifest、扩展点、贡献注册、能力模型 |
| `ide-plugin` | 插件 Wasm 运行时（wasmtime component model + WASI preview 2） |

## 应用壳

| crate | 做什么 |
|-------|--------|
| `ide-host` | 无 UI 的 IDE 后端：每窗口项目、终端、进程、调试管理器 |
| `ide-cli` | `oxidea` 命令行：参数解析、向运行中的实例发请求 |

## 周边

- `../src-tauri/` —— Tauri 应用壳（窗口、事件、命令）；
- `../src/` —— React 前端；
- `../languages/` —— 60+ 语言定义（`*.toml`）；
- `../wit/` —— 插件 WIT 接口定义；
- `../plugins/` —— 官方插件（blame-annotations、container-runner、harbor-night、test-runner、todo-finder）；
- `../sdk/rust` —— `oxidea-plugin` 插件开发 SDK；
- `../java/` —— JVM 侧组件（java-usages、kotlin-bridge、lombok）；
- `../docs/` —— 用户文档；`../perf/` —— 与 IDEA 的启动基准；`../e2e/`、`../smoke/`、`../fixtures/` —— 测试。
