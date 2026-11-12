# OX agent 的 IDE 环境感知方案 —— 让 agent 如臂在握

> 状态：待评审。**定位：agent 是 IDEA 的大脑**——自己感知环境、操控 IDE 写代码、
> 运行、调试；IDE 的一切能力是它的手脚。
> 原则：复用现成机制（ide-run / ide-dap / ide-host 已有能力），只做接线与包装，不造新轮子。

## 〇、大脑的能力地图（三层）

| 层 | 内容 | 现状 |
|---|---|---|
| **眼（感知）** | 环境简报、编辑器状态、Problems、进程状态 | 改动 1/2/3 |
| **手（操控）** | 写代码 ✅、构建 ✅、运行 ✅（差进程管理）、**调试 ❌**、VCS 读 ✅ 写 ❌ | 改动 2/6/7 |
| **脑（自知）** | 能力地图、plan 模式、任务/进度呈现 | 改动 3/4/5 |


## 一、架构现状（数据流，全部已验证）

```
IDE Host (ide-host)
  └─ ox_tools.rs :: ox_tool_provider(host)        ← 宿主注入点（按任务 cwd 挑最深匹配项目）
       ├─ Registry::tools(&project)               ← 23 个 MCP 语义工具元数据
       ├─ IdeTool 适配（执行走 Registry::call，spawn_blocking 桥接）
       ├─ ox_db_tools::db_tools(host, cwd)        ← 数据库面板连接 → db_query/db_dml
       └─ OxHostContext { tools, memory_root }    ← 进程级 OnceLock 注入 agentscope-acp
            └─ session.rs :: assemble_agent       ← 工具注册 + sys_prompt 组装（含 datasource_section）
```

关键结论：**agent 的全部 IDE 能力都经 `OxHostContext` 这一个入口进来**。
环境感知的改动也全部落在这个入口上，crate 边界干净（agentscope-acp 不依赖 ide-run，
宿主算好文本/工具传进来）。

## 二、能力矩阵（已验证）

### 已接通（agent 现在就能用，23 个工具）

| 维度 | 工具 | 说明 |
|---|---|---|
| 编辑器状态 | `get_all_open_file_paths`、`get_current_file` | 后者含**用户选中的文本**与光标位置 |
| 代码质量 | `get_file_problems`、`get_project_problems` | Problems 视图（inspection + LSP） |
| 搜索/符号 | `search_text/regex/symbol/file`、`get_symbol_info`、`find_references` | |
| 编辑 | `apply_patch`、`create_new_file`、`rename_refactoring`（语义级）、`reformat_file`、`open_file_in_editor` | |
| 文件 | `read_file`、`list_directory_tree` | |
| 构建运行 | `build_project`、`get_run_configurations`、`execute_run_configuration` | 见下「运行机制」 |
| VCS | `get_repositories`、`git_status`、`git_diff` | |
| 数据库 | `db_query`（只读免确认）、`db_dml`（HITL 确认） | 面板有连接时 |
| 浏览器 | `browser_navigate/snapshot/click_at/...` | agentscope-browser |

**运行机制**（`mcp/run/configs.rs`，已验证）：`get_run_configurations` 返回自动检测
（Spring Boot 主类 / Cargo.toml / go.mod / package.json / Gradle / Maven / Tomcat / JUnit /
dotnet / cmake / ruby）+ IDEA `.idea/runConfigurations` 与 VSCode `launch.json` 导入 +
`.oxidea/run.toml` 用户自定义。`execute_run_configuration` 启动前必经用户确认（HITL），
返回**输出快照 + 可选 exitCode + `fullOutputPath`**（完整原始输出的临时文件，
进程活着就继续增长，一天后清理）。

→ **启动日志的获取路径其实已经闭环**：`waitForExit=true` + 短 timeout 拿快照与
`fullOutputPath`，进程转后台；后续用 `read_file` 轮询该文件看新日志。
缺的只是 agent 不知道这个打法（提示词没教）+ 停不掉进程。

### ide-run 层现成、但未暴露成工具（改动 2 的对象）

`ide-run/src/process.rs :: ProcessManager`（全部已验证存在）：
`stop(id)`、`stop_with_grace(id, grace)`、`list() -> Vec<RunInfo>`、`exit_code(id)`、
`write_stdin(id, data)`（可向运行中进程发输入）、`kill_project(root)`。

### ide-host 有、但未接的能力（路线图）

| 能力 | 位置 | 价值 |
|---|---|---|
| **断点调试** | `ide-dap`（完整 DAP 客户端：Session/Connection/DebugManager，断点行/函数/数据，Java/Kotlin 走 JDWP、Rust 走 cargo、Python 走 debugpy，attach 附加已有进程） | **大脑的核心缺口**，改动 6 |
| JDK/运行时检测 | `ide-host/src/java_runtime.rs` | 并入环境简报（本期做） |
| 子进程环境叠加 | `environment.rs` / `shell_env.rs`（登录 shell + devcontainer remoteEnv） | 环境简报可提一句；深挖后置 |
| 测试运行 | `ide-run/src/junit.rs`（JUnit 配置） | 已可经 `execute_run_configuration` 跑；结构化测试报告后置 |
| 插件 MCP 工具 | `plugins/mcp.rs :: mcp_tools` | **已自动可用**（`Registry::call` 有插件分支），无需开发 |
| 向进程发输入 | `ProcessManager::write_stdin` | 交互式程序应答，后置 |
| VCS 写操作 | `ide-host/src/mcp/vcs.rs`（现有 status/diff 只读） | commit/branch/stage，改动 7 |
| notebook / kotlin_bridge / phpstan / formatter | 各自模块 | 边缘，暂不接 |

## 三、改动清单（怎么改、在哪改、为什么）

### 改动 1：启动环境简报 —— 治「开局盲」

**在哪改**（3 个文件，各一处）：

1. `ox-idea/crates/agentscope-acp/src/lib.rs` — `OxHostContext` 结构体加字段：
   ```rust
   pub project_brief: Option<String>,
   ```
2. `ox-idea/crates/ide-host/src/ox_tools.rs` — `ox_tool_provider` 闭包内构造简报：
   - **直接复用 `ide_run::detect(&project_root)`**（`lib.rs:242`，pub）——多语言
     检测统一入口，返回 RunConfig（kind/name/command/cwd/env），覆盖
     npm/cargo/gradle/maven/go/python/pytest/make/tomcat/junit/dotnet/cmake/ruby/shell
     + IDEA/VSCode 配置导入。**不要自己写构建文件探测**——detect 已做且更全。
   - `java_runtime.rs` 的 JDK 检测（Java 项目时补充版本信息）；
   - 现有 `datasource_section` 的数据源画像并入。
   - 按语言分组拼成紧凑文本（目标 ≤ 40 行），塞进 `OxHostContext::project_brief`。
   - 为什么放宿主侧：ide-run / java_runtime 都是 ide-host 的依赖，agentscope-acp
     不该知道它们；宿主算好文本，agent 只管拼进提示词——crate 边界不破。
3. `ox-idea/crates/agentscope-acp/src/session.rs` — `assemble_agent` 的 sys_prompt：
   `{ide_section}` 处渲染 `project_brief`（有则注入「## 当前项目环境」节，无则维持现状）。

**多语言能力矩阵**（全部已验证存在，agent 大脑对每种语言拿到的完整牌）：

| 语言/栈 | 检测 | 运行 | 调试（改动 6） | 测试 |
|---|---|---|---|---|
| Java | detect（pom/gradle + Spring Boot 主类） | maven/gradle/tomcat | java-debug + JDWP attach | junit 配置 |
| Kotlin | detect | gradle | kotlin-debug-adapter | junit 配置 |
| Rust | Cargo.toml | cargo | codelldb | cargo test（shell） |
| Go | go.mod | go（build+run 两段计划） | delve | go test（含测试文件运行） |
| Node.js | package.json scripts | npm | js-debug | npm test（shell） |
| Python | python/pytest 配置 | python/pytest | debugpy | pytest 配置 |
| .NET | csproj/launchSettings | dotnet | netcoredbg | dotnet test |
| C/C++ | cmake | cmake build+launch | codelldb / lldb-dap | — |
| Ruby | ruby 配置 | ruby | rdbg | ruby_reporter（结构化测试报告） |

**为什么**：agent 开局即知「这是什么项目、可跑配置有哪些、数据源是什么」——
Phase 1 的定位成本大幅下降，也不会再用 shell 瞎试启动。多语言不是额外开发，
detect 与 ide-dap 本来就是全语言实现，简报与调试工具天然继承。

**验收**：打开一个 Spring Boot 项目发起会话，agent 第一轮就能说出项目类型与可跑配置；
单测断言 sys_prompt 含简报内容。

### 改动 2：进程管理三工具 —— 治「启动后失联」

**在哪改**：

1. 新文件 `ox-idea/crates/ide-host/src/mcp/run/processes.rs`（仿 `configs.rs` 的
   `Tool::new` 形态），注册进 `mcp/run/mod.rs` 的工具清单：
   - `get_running_processes` — `ProcessManager::list()`，返回 runId / 名称 / kind /
     命令 / 运行时长 / exit code。**只读**。
   - `stop_process` — `ProcessManager::stop_with_grace(id, grace)`，参数 runId +
     可选 grace 秒。**非只读**（走 HITL 确认，与 execute 启动对称）。
   - `get_process_output` — 按 runId 取输出快照（数据源与 execute 的快照同源）。
     若 RunInfo 不直接暴露缓冲，则本期不做此工具，提示词引导用 `fullOutputPath` +
     `read_file` 轮询（机制已闭环），避免为包装而改 ProcessManager。
2. `ox-idea/crates/agentscope-acp/src/session.rs` — sys_prompt「构建与运行」节补打法：
   长驻服务用 `waitForExit=true` + 短 timeout 拿快照与 `fullOutputPath`，后续读该文件
   看新日志；停止用 `stop_process`；不要用 shell 杀进程（绕过 IDE 进程簿记）。

**为什么**：`ProcessManager` 的 API 全部现成，这是纯包装；补完后
「启动 → 看日志确认起来 → 验证 → 停止」闭环，agent 不再留下杀不掉的后台进程。

**验收**：agent 启动一个 dev server → 读到日志确认监听端口 → `stop_process` 停止 →
`get_running_processes` 确认已退出。集成测试覆盖 list/stop 的工具注册与参数校验。

### 改动 3：能力地图 —— 治「自知残缺」

**在哪改**：`agentscope-acp/src/session.rs` 的 sys_prompt（纯文本，无代码逻辑）。

**内容**（新增一节，约 15 行）：
- 环境简报说明：系统提示词里的「当前项目环境」是开局事实，先读它再动手；
- 编辑器上下文引导：用户说「这个文件 / 我选中的」时先 `get_current_file` /
  `get_all_open_file_paths` 确认，不猜；
- 改动验证闭环已有规程（problems 基线对比）保持不变；
- 能力边界：调试（断点）暂不可用，需要时如实告知用户而不是用日志硬凑。

**为什么**：按既定工作原则——提示词改决策层成本最低且无回归风险；
agent 对「自己有什么牌」的自知决定它会不会用对工具。

**验收**：提示词评审 + 真实会话观察（用户说「我选中的这段」时 agent 正确调
`get_current_file`）。

### 改动 4：plan 模式接线（锦上添花部分，环境感知之后）

**在哪改**：

1. `agentscope-acp/src/session.rs` — `assemble_agent` 的 builder 链加
   `.enable_plan_mode(true)` + `.plan_file_dir("<memory_root>/plans")` +
   `.plan_prompt(<已设计的计划态提示词>)`（提示词设计已评审，见会话记录）。
2. `agentscope-acp/src/protocol.rs` — initialize / session/new 响应声明 mode
   configOption（`id: "mode"`, category `"mode"`, 选项 code/plan，currentValue code）。
3. `agentscope-acp/src/lib.rs` + `session.rs` — handle_request 加
   `session/set_config_option` 分支 → `hub.set_config` → value=="plan" 调
   `agent.enter_plan_mode(...)`，否则 `exit_plan_mode`（幂等）。
   plan 态随 AgentState 持久化（配合已完成的 session/load，重启不丢）。
4. 前端零改动：Composer 的 modeOption 渲染现成（`composerModel.ts:193` 连 plan
   图标映射都有）；plan 态写操作由 `PlanModeMiddleware` 确定性拦截（框架现成）；
   `plan_exit` 的 ask 审批走现有权限卡片。

**为什么**：框架的 plan 模式（Manager + Middleware + 工具 + 状态持久化）是完整实现，
OX agent 只差接线；「先计划后实施」的节奏由代码保证，计划内容由 plan_prompt 引导、
按任务轻重灵活——死的是节奏，活的是内容。

**验收**：切换 plan 后写工具被拦截（Denied 结果进上下文）；plan_write 写出计划；
plan_exit 触发审批；批准切回 code 后实施自由；重启后 plan 态恢复。

### 改动 6：调试接入 —— 大脑的核心缺口

**底层已验证**：`ide-dap` 是完整 DAP 客户端——`session.rs` 的 Session（连接/能力/
事件循环，DAP 事件转 `IdeEvent::Debug`）、断点三类（行 `Breakpoint` / 函数
`FunctionBreakpoint` / 数据 `DataBreakpoint`）、`adapter.rs` 的适配器解析与启动，
**九个内置适配器全语言覆盖**：java-debug（Java/jdtls）、kotlin-debug-adapter、
codelldb（Rust/C++）、delve（Go）、js-debug（Node/前端）、debugpy（Python）、
netcoredbg（.NET）、lldb-dap（C/C++ LLVM）、rdbg（Ruby），另有 `jdwp.rs` 支持
Java 远程 attach。前端调试 UI 在用同一套；**缺的只是 MCP 工具化**。

**在哪改**：新文件 `ox-idea/crates/ide-host/src/mcp/debug/`（仿 `run/` 的组织），
注册进 `mcp/mod.rs` 工具清单：

| 工具 | 映射的 ide-dap 能力 | 只读 |
|---|---|---|
| `start_debug_session` | DebugManager 启动（debug 配置名或 filePath+line，复用 run 配置检测；attach 模式附加已有进程） | 否（HITL 确认） |
| `set_breakpoint` / `remove_breakpoint` | Breakpoint 增删（file+line） | 否 |
| `debug_control` | continue / step_over / step_into / step_out / pause（一个工具多 action，避免工具爆炸） | 否 |
| `get_debug_state` | stopped 后的栈帧 / 局部变量 / 断点命中位置（拉模式查询，DAP 事件仍推给前端 UI） | 是 |
| `evaluate_expression` | 栈帧上下文求值（watch） | 是 |
| `stop_debug_session` | 会话终止（连带 kill debuggee，对齐 `589c89be` 的语义） | 否（HITL） |

**为什么这样设计**：
- DAP 是推模式（事件流），MCP 工具是拉模式——agent 用 `get_debug_state` 轮询即可，
  不需要给 agent 接事件订阅；前端调试 UI 的事件流不受影响（同一会话双消费）。
- `debug_control` 合并五个控制动作：工具数量克制（用户工作原则：工具面精简，
  描述里写清 action 枚举）。
- 断点操作非只读但低危：走 HITL 确认与 execute/stop 对称。

**为什么值得做**：调试是工程师排障的核心手段——「在断点处看变量、单步走、
改了再跑」是 agent 现在完全做不到的事；底层全现成，这是纯工具化映射。

**验收**：agent 对一个 Java 项目：start_debug_session → set_breakpoint →
命中后 get_debug_state 读到局部变量 → evaluate_expression 求值 → debug_control
单步 → stop_debug_session。集成测试覆盖工具注册与参数校验；真实调试会话实测。

### 改动 7：VCS 写操作 —— 补全「改完提交」闭环

**在哪改**：`ox-idea/crates/ide-host/src/mcp/vcs.rs` 加两个工具（现有
git_status/git_diff/get_repositories 同文件）：
- `git_commit` — stage 指定文件 + commit（message 参数必填；HITL 确认）；
- `git_branch` — create/switch（HITL 确认）。
push 不做（不可逆外发动作，保持用户手动或明确授权后走 shell）。

**为什么**：大脑要能完成「改 → 验证 → 提交」闭环；现在只能看不能提交，
每次都要用户手动，断档。

**验收**：agent 改完代码 → git_commit（用户确认卡片）→ git_status 干净。

### 改动 5：任务/进度呈现 + Bug 统计约定

**在哪改**：

1. `agentscope-acp`（新工具，随改动 4 同文件）— `update_plan` 工具：模型更新任务
   清单（pending / in_progress / completed）→ acp 层转 ACP `plan_update` 帧 →
   ide-agent 渲染（`normalize.rs:137` 现成，前端零改动）。
2. sys_prompt（改动 3 同文件）— Bug 约定：测试失败必记
   `.oxidea/plan/<任务>/bugs.json`（id/描述/关联用例/状态 open→fixing→fixed→closed/
   fix_note）；`fixed` 必写 fix_note；回归通过才 `closed`；统计随 plan_update 播报。

**为什么**：任务与进度的呈现链路（plan_update）已通，只缺模型侧的更新入口；
Bug 统计是纯文件约定，studio 验证过的结构。

**验收**：MockModel 测试走一轮多任务流程，plan_update 帧序列正确；bugs.json
状态流转符合约定。

## 四、实施顺序与依赖

```
P1 环境感知（改动 1+2+3）        ← 本期主体：开局知环境 + 进程闭环 + 能力地图
P2 VCS 写操作（改动 7）          ← 小改动，补全「改→验证→提交」闭环
P3 plan 模式（改动 4）           ← 依赖 P1 的 sys_prompt 结构稳定
P4 呈现与统计（改动 5）          ← update_plan 依赖改动 4 的 ACP 帧通道
P5 调试接入（改动 6）            ← 最重的一块，单独阶段；依赖 P1 的进程管理经验
每步：cargo test -p agentscope-acp -p ide-host -p ide-agent 全绿 + 真实会话实测
```

## 五、明确不做的（本期）

- 结构化测试报告（测试面板）：IDE 自带测试 UI，agent 播报足够。
- `write_stdin` / 交互式进程应答：后置。
- studio 的 req_tool 四工具：单 agent 用文件约定足够。
- 代码级阶段状态机：plan 态拦截已由框架保证，流程内容靠提示词，实测遵循度
  不行再加兜底。
- git push：不可逆外发动作，保持用户手动或明确授权后走 shell。
