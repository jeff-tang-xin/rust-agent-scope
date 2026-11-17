//! OXidea 专属内置技能：IDE 使用指南。
//!
//! 内容归宿主（agentscope-acp），不进框架级 `BUILTIN_SKILLS`——框架
//! （agentscope-harness）保持通用，产品专属技能经 `LoadSkillTool::with_extra_builtins`
//! 注入，检索语义与框架内置一致（仓库优先、内置兜底、不落盘、无附属文件）。

use agentscope_harness::skill::{AgentSkill, SkillUtil};

/// OXidea 使用指南（任意模式可加载：plan / code 都在 sys_prompt 引导）。
pub const OXIDEA_USAGE_SKILL: &str = r#"---
name: oxidea-usage
description: OXidea IDE 操作指南，覆盖五类工作场景的具体做法与工具调用顺序——调查（搜代码/追调用链/摸结构）、编码（改代码/重构/编译验证/问题检查）、调试（跑应用/断点调试/查进程/定位循环）、Git（状态/差异/提交/分支）、浏览器验证（页面操作/截图）。开始任何编码、调试或代码调查任务前加载本 skill，按对应场景的标准步骤操作。
---

# OXidea 使用指南

本 skill 指导你在 OXidea 中完成五类工作：**调查**（搞清代码现状）、**编码**（改代码并验证）、**调试**（跑起来并定位问题）、**Git**（查看与提交）、**浏览器验证**（页面操作与截图）。每个场景给出标准步骤与工具调用顺序，按场景照做，不要凭直觉乱试。文末「环境事实」是开局即知的 IDE 内在构造，不要靠失败去试探。

## 一、调查：搞清代码现状

标准顺序：

0. **先看用户现场**：`get_current_file` 拿当前聚焦文件与选区（用户大概率在说它）；`get_all_open_file_paths` 看打开了哪些文件；`get_project_problems` 看 Problems 面板现有问题。用户说「这个文件/这段代码」时先查现场，不要反问路径。
1. **定位入口**，按已知线索选工具：
   - 知道符号名（类/函数/字段）→ `search_symbol` 精确定位 → `get_symbol_info` 看签名与文档。
   - 只知道报错文本或日志片段 → `search_text`（精确子串）或 `search_regex`（模式匹配）。
   - 按文件名找文件 → `search_file`（IDE 索引）或 `glob_files`（模式匹配）。
   - 什么都不知 → `list_directory_tree` 看结构，找模块入口再深入。
2. **追调用链**：`find_references` 看谁在调用——评估改动影响面必做这一步。它只适合具体声明的影响面分析，不要对准高频名字（id/name 这类通用字段、多实现的接口方法）——会返回全工作区成千上万条引用；那种情况改用相关文件内 `search_text`。索引分析未完成时结果会标注不完整，等就绪再查，不要把不完整结果当全貌。
3. **读实现**：`read_file` 读关键路径，确认约束（参数校验、并发、错误处理套路）。IDE 版 `read_file` 支持 jar/zip 内读取与依赖源码根，比裸文件工具强。
4. **产出**：能一句话说清「问题在哪、涉及哪些文件、现有约定是什么」——说不清就继续查，不许动手改。

## 二、编码：改代码并验证

标准顺序：

1. **改**：`edit_file` 精确替换（old_string 从刚读过的真实内容取，不凭记忆拼）；新建文件用 `create_new_file`；一次多块改动用 `apply_patch`；语义级重命名用 `rename_refactoring`（自动连带所有引用，不要手工逐处改）；格式化用 `reformat_file`。改完用 `open_file_in_editor` 把关键文件打开给用户看——改动在编辑器里可见，不是黑盒。
2. **编译**：`build_project`（或 shell 构建），以 BUILD SUCCESS 为准。大型项目首次构建常见 5-10 分钟，编译期持续输出属正常，耐心等待，不要中途判定卡死而取消。
3. **查问题**：`get_file_problems` 对改动文件——改前先记基线，改后对比，新出现的问题就是你引入的，修掉。
4. **测试**：跑与改动相关的测试并核对通过数；全量测试在收尾阶段兜底。

## 三、调试：跑起来并定位问题

标准顺序：

1. **找配置**：`get_run_configurations` 看可用的运行/调试配置（IDE 自动检测 + 用户自定义）。
2. **启动**：`execute_run_configuration`（`waitForExit=true` + 短 timeout 拿输出快照）；长驻服务转后台后 `read_file` 其 `fullOutputPath` 日志文件看新输出。Java/agent 类项目优先走运行配置——没有配置时按第四节用 mvn 启动，不要手拼 classpath。
3. **确认在跑**：`get_running_processes` 现查进程状态（只列运行配置启动的进程），不要猜；停掉用 `stop_process`（传 runId，优雅终止，UI 可见）。
4. **断点调试**（DAP 工具链——断点/变量/调用栈在界面可见，不要 shell 起进程再 print 硬凑，那是纯黑盒）：
   1. `set_breakpoint` 在目标文件行设断点（line 1-based，可带 condition；会话启动前后都能设，断点与编辑器 UI 共享）。
   2. `start_debug_session` 启动调试会话：传 adapter（Java=java-debug、Rust/C++=codelldb、Node=js-debug、Python=debugpy、Go=delve、.NET=netcoredbg）+ program/args；适配器选项走 extraJson（如 Rust `{"cargo":{"args":["build"]}}` 构建并启动产物、Java 远程 attach 用 request=attach + `{"debugServer":"host:port"}`）。
   3. `get_debug_state` 轮询：线程 stopped=true 即命中断点，返回栈顶帧（文件+行）、作用域与顶层变量；传 variablesReference 逐层展开变量（一次一层）。
   4. `evaluate_expression` 在停帧上下文求值验证假设（watch/repl 语义；会话必须处于 stopped，先 poll get_debug_state）。
   5. `debug_control` 继续/单步（continue / step_over / step_into / step_out / pause），每次动作后回到 `get_debug_state` 看停在哪。
   6. 定位后 `stop_debug_session` 收尾（终止会话并杀调试进程树）；删断点用 `remove_breakpoint`。
5. **定位循环**：拿到报错文本 → `search_text` 定位 → `find_references` 追链路 → 读实现 → 修复 → 回到第 2 步重新验证。

## 四、Java agent 启动与进程管理

Java 类 agent（mvn 工程）启动失败，绝大多数是 **classpath 问题，不是代码问题**。`ClassNotFoundException` 几乎总是「手拼 classpath 漏了依赖」，不是「类不存在」。

### 启动：用构建工具，禁止手拼 classpath

**错误**（经典错误——漏掉所有依赖 jar，必报 ClassNotFoundException）：

```bash
java -cp target/classes com.example.AgentMain     # ❌ ClassNotFoundException
```

**正确**，按优先级：

1. **`mvn exec:java`**——Maven 自动构建完整运行时 classpath：

   ```bash
   mvn -q compile exec:java -Dexec.mainClass=com.example.AgentMain
   # 传参：-Dexec.args="--port 8080"
   ```

2. **Spring Boot 项目**：`mvn -q spring-boot:run`。

3. **长驻/生产式启动**——先打包再带依赖 classpath 运行：

   ```bash
   mvn -q package dependency:copy-dependencies -DoutputDirectory=target/dependency
   java -cp "target/classes;target/dependency/*" com.example.AgentMain   # Windows（分隔符 ; ，通配符必须加引号）
   java -cp "target/classes:target/dependency/*" com.example.AgentMain   # macOS/Linux（分隔符 : ）
   ```

纪律：

- 多模块工程先在聚合根 `mvn -q install -DskipTests` 一次——单模块的 `target/classes` 缺兄弟模块产物。
- 不要 `cd` 进 `target/` 里运行——相对资源路径会断。
- 首次启动失败先看 `mvn -v` 的 `Java version:` 行——JDK 版本不符（如默认 JDK 8 编译 Java 17 工程）会报出与真实原因无关的错。

### 排查：三种报错，三种根因

- `ClassNotFoundException: com.example.X`（启动即报）→ 运行时 classpath 缺类。用上面的 exec:java 或 copy-dependencies 修，不要拷 jar 补丁。
- `NoClassDefFoundError: com/example/X`（斜杠形式，运行期报）→ 编译期有、运行期缺，或静态初始化失败。先 `mvn -q compile` 确认改过代码后重新编译了；再查依赖是否 `scope=provided`。
- `NoSuchMethodError` / `AbstractMethodError` → **同一依赖的两个版本同时在场**。用 `mvn dependency:tree -Dincludes=groupId:artifactId` 定位冲突并 exclude 旧版本——不要靠调整 `-cp` 顺序碰运气。
- 想确认运行时 classpath 到底有什么：`mvn dependency:build-classpath -Dmdep.outputFile=cp.txt`，然后读 `cp.txt`。

### 停止：干净收尾，不留孤儿

先分清进程归谁管：**运行配置启动的进程在 IDE 簿记里，用 `stop_process` 停**（优雅终止，UI 可见）；**你经 shell 启动的进程（mvn/node）不在 IDE 簿记里，`stop_process` 管不到，由你负责用 shell 停**。

- `mvn exec:java` 启动的 JVM 跑在 Maven 进程里，杀 mvn 即杀 agent。优先温和终止让 shutdown hook 跑完（释放端口、落盘文件）。
- `taskkill /PID <pid> /F`（Windows）/ `kill -9`（macOS/Linux）跳过 shutdown hook——只在进程卡死时用；强杀后确认端口已释放：`netstat -ano | findstr :<port>`（Windows）/ `lsof -i :<port>`（macOS/Linux）。
- **任务收尾时停止你启动的长驻进程**（JVM/node）——不要留后台进程占着端口和锁，下一个任务或下一次启动会被它卡住（残留进程还会占住应用的单实例锁，导致应用「点了图标打不开」）。

## 五、Git：查看与提交走 IDE 工具

标准顺序：

1. `get_repositories` 看项目有几个仓库根（多仓库项目必做）、当前分支、HEAD 与进行中的 merge/rebase/cherry-pick。
2. `git_status` 看工作区状态（porcelain 码 + 计数）；`git_diff` 看具体差异（staged=true 比 index 与 HEAD，否则比磁盘与 index；未保存的编辑器文本不在 diff 里）。
3. `git_commit` 提交：传 files 会先暂存再提交；不传 files 提交已暂存内容——先 `git_status` 确认那是什么。提交走用户确认；**此工具不支持 push**。
4. `git_branch` 切分支/建分支（create=true），走用户确认。
5. push/pull 等远端操作没有语义工具，才用 shell。

## 六、浏览器验证：页面操作与截图

工具族：`browser_navigate` / `browser_snapshot` / `browser_click` / `browser_type` / `browser_hover` / `browser_scroll_at` / `browser_drag` / `browser_evaluate` / `browser_screenshot` / `browser_close`。

标准顺序：

1. `browser_navigate` 打开页面 → `browser_snapshot` 拿可交互元素的无障碍树快照（每行 `[ref=eN] role "name"`）。
2. **快照是主感知通道**：click/type/hover 一律用 ref 句柄寻址（ref="eN"），不要像素坐标；CSS selector 与比例坐标只作兜底。
3. ref 是短命句柄，每次快照重新分配：导航后、动作返回 stale_ref 时、页面变化后都要重新 snapshot。
4. **表单逐控件操作**：联动表单（选省份才加载城市这类）每步操作后重新 snapshot 再动下一个控件——批量设值基于过期页面状态，越快错得越一致。
5. 验证结果用 `browser_screenshot`（存 `.oxidea/screenshots/`）或 snapshot 文本断言；`browser_evaluate` 用于读页面状态/断言，不用它绕过正常交互。
6. **会话与用户的浏览器直播面板共享**：你操作的页面就是用户面板里看到的页面。不要随意 `browser_close`——会断掉用户正在看的现场；任务结束保留页面。

## 七、环境事实（开局即知，不要靠失败试探）

- **信任模型**：未信任的项目只暴露只读工具——写/执行类工具「消失」不是故障，是 IDE 的信任门禁；需要时提示用户在 IDE 里信任项目。
- **索引预热**：符号/引用/问题检查依赖项目索引与语言服务，刚打开的大项目分析未完成时结果会标注不完整——等就绪再查。
- **权限分层**：只读工具免确认直接执行；写/执行类工具走用户确认（HITL）；`db_dml` 变更确认是设计不是故障。确认被拒就停下说明，不要换路径绕过。
- **`.oxidea/` 目录域**：`screenshots/` 浏览器截图、`evicted/` 超大工具结果卸载落盘（占位里有文件地址，`read_file` 可读回）、`agent/` 记忆+黑板+specs。这些是系统数据域，不要用文件工具直接改其中的记忆/黑板文件。
- **上下文会被压缩**：长会话历史自动压缩、超大结果卸载落盘——关键中间结论随手 `blackboard_append`，压缩后仍可读回。

## 工具边界（为什么这样分工）

- IDE 语义工具的操作在界面有 UI 反馈（搜索命中、引用图、构建面板、调试断点、Git 面板），用户看得见、跟得上；shell 是黑盒。**语义工具能做的不要绕 shell**——shell 只做语义工具覆盖不到的事：git push 等远端操作、shell 启动的进程收尾、语义工具没有的脚本。
- 数据库：`db_query` 只读免确认 / `db_dml` 变更走用户确认（这是设计不是故障）——不要 shell 直连绕过确认。
- shell 纪律：只跑非交互、有界、能自行结束的命令；长命令输出重定向到日志文件再读日志（两步，不用 && 串联）；判定成功看 exit code 与 Tests run 计数，不看输出「看起来正常」。

## 现场记录（跨打断保连续）

- 开工先 `blackboard_read` 恢复现场；重要发现随手 `blackboard_append`（发现即写，不要攒到收尾）——被打断时已登记的不丢。
- 收尾 `blackboard_append` 登记结论与风险，`memory_save` 沉淀跨会话有价值的结论（用户偏好、架构决策、踩坑）。
- 用户提到「之前/刚才/一直在」而你接不上上下文时，先查黑板与记忆恢复现场，不要让用户重新交代背景。
"#;

/// 构造 OXidea 使用指南技能（source 标记 builtin，与框架内置同语义）。
pub fn oxidea_usage_skill() -> Option<AgentSkill> {
    SkillUtil::create_from(OXIDEA_USAGE_SKILL, None, "builtin").ok()
}
