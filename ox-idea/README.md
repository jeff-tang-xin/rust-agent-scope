# OXidea

你熟悉的 IDE，用 Rust 重写——并且内置了编码智能体。

**项目出处**：OXidea 基于 OpenIDEA（Rust 复刻 IntelliJ IDEA 体验的开源 IDE）
发展而来，现已切断 upstream、纳入本仓库独立演进。仓库总览与框架出处见
[根 README](../README.md)。

OXidea 把 IntelliJ IDEA 的工作方式——keymap、工具窗口、search-everything 的
习惯——带进 Rust 内核的原生应用：编辑器、代码洞察、调试器、Git、终端、数据库
与 HTTP 客户端，支持 60+ 语言，构建于 macOS、Windows 与 Linux（Windows 另有
便携 zip，解压即用）。

## AI 能力

OXidea 不只是把 AI 塞进侧边栏——智能体与 IDE 共享同一套内核能力：

- **内置编码智能体 OX Hareness**：基于本仓库的 AgentScope Rust 框架
  （`crates/agentscope-acp` 以 ACP v1 协议接入 IDE），能读改代码、跑构建与测试、
  操作 Git、查数据库、驱动浏览器——工具就是 IDE 本身，不是拼凑的 CLI；
- **驱动外部智能体**：`ide-agent` 用同一套 ACP 协议会话化管理 Claude Code、
  Codex、Gemini CLI 等外部编码智能体（任务、会话、worktree）；
- **IDE 即 MCP server**：`ide-mcp` 在 127.0.0.1 上暴露 Streamable HTTP，
  IDE 之外的 AI（Claude Code、Codex…）可以直接调用 IDE 的符号索引、
  代码洞察与文件能力；
- **权限确认（HITL）**：智能体的每个写动作都经人工确认，只读操作自动放行；
- **上下文压缩**：长会话自动压缩，智能体不会越聊越笨。

智能体框架本身（ReAct 循环、6 个模型提供商、工具系统、Harness 编排）的
完整能力清单见根 README 的「AI 能力」一节。

## 基准测试

打开 [freqtrade](https://github.com/freqtrade/freqtrade)（Python，约 770 个文件），
热缓存，5 次取中位数：

|                             | OXidea          | IntelliJ IDEA 2026.2.3 |
| --------------------------- | ----------------- | ---------------------- |
| 出现项目窗口                | **0.9 s**         | 3.1 s                  |
| 就绪（CPU 落定）            | **1.1 s**         | 13.2 s                 |
| 内存（USS，全部进程）       | **169 MB**        | 2011 MB                |
| 就绪前 CPU 时间             | **2.4 s**         | 47.9 s                 |
| 空闲 CPU                    | **4 %** 单核      | 7 % 单核               |
| 退出                        | **0.3 s**         | 5.9 s                  |

`main` 分支 release 构建的 OXidea 对默认设置与插件的 IDEA，Windows 11、
32 核 AMD、62 GB 内存。IDEA 对 Python 的分析更深，两侧的「就绪」不是同一份
工作量。两者都从外部以相同方式测量，在你的机器上复现见 `perf/compare`。

## 架构

IDE 内核是 `crates/` 下的 **39 个 crate**（独立 Cargo workspace），按职责分
七组：内核与文本、语言与代码洞察、运行与调试、智能体、工具窗口与集成、
插件系统、应用壳；周边有 Tauri 应用壳（`src-tauri/`）、React 前端（`src/`）、
60+ 语言定义（`languages/`）、插件 WIT 接口（`wit/`）、官方插件（`plugins/`）、
插件开发 SDK（`sdk/rust`）、JVM 侧组件（`java/`）与启动基准（`perf/`）。

每个 crate 的逐条说明见 [crates/README.md](crates/README.md)。

## 插件

插件为 IDE 添加动作、补全、quick fix、工具窗口、自定义编辑器、主题、文件图标
与语言。它们以沙箱化的 WebAssembly 组件运行；主题、图标主题与语言完全不需要
写代码。从 Settings › Plugins 安装，或用 `oxidea-plugin new` 开始写自己的插件——
见 `plugins/README.md` 与 `plugins/official`。插件 API 仍处于预览阶段。

## License

Apache 2.0（见 `LICENSE`）。捆绑的第三方资源清单见 `THIRD_PARTY_LICENSES.md`。

与 JetBrains 无关联。IntelliJ IDEA 是 JetBrains 的商标。
