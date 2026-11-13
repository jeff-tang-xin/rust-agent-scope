//! OXidea 专属内置技能：IDE 使用指南。
//!
//! 内容归宿主（agentscope-acp），不进框架级 `BUILTIN_SKILLS`——框架
//! （agentscope-harness）保持通用，产品专属技能经 `LoadSkillTool::with_extra_builtins`
//! 注入，检索语义与框架内置一致（仓库优先、内置兜底、不落盘、无附属文件）。

use agentscope_harness::skill::{AgentSkill, SkillUtil};

/// OXidea 使用指南（任意模式可加载：plan / code 都在 sys_prompt 引导）。
pub const OXIDEA_USAGE_SKILL: &str = r#"---
name: oxidea-usage
description: OXidea IDE 操作指南，覆盖三类工作场景的具体做法与工具调用顺序——调查（搜代码/追调用链/摸结构）、编码（改代码/重构/编译验证/问题检查）、调试（跑应用/断点调试/查进程/定位循环）。开始任何编码、调试或代码调查任务前加载本 skill，按对应场景的标准步骤操作。
---

# OXidea 使用指南

本 skill 指导你在 OXidea 中完成三类工作：**调查**（搞清代码现状）、**编码**（改代码并验证）、**调试**（跑起来并定位问题）。每个场景给出标准步骤与工具调用顺序，按场景照做，不要凭直觉乱试。

## 一、调查：搞清代码现状

标准顺序：

1. **定位入口**，按已知线索选工具：
   - 知道符号名（类/函数/字段）→ `search_symbol` 精确定位 → `get_symbol_info` 看签名与文档。
   - 只知道报错文本或日志片段 → `search_text`（精确子串）或 `search_regex`（模式匹配）。
   - 什么都不知 → `list_directory_tree` 看结构，找模块入口再深入。
2. **追调用链**：`find_references` 看谁在调用——评估改动影响面必做这一步。
3. **读实现**：`read_file` 读关键路径，确认约束（参数校验、并发、错误处理套路）。
4. **产出**：能一句话说清「问题在哪、涉及哪些文件、现有约定是什么」——说不清就继续查，不许动手改。

## 二、编码：改代码并验证

标准顺序：

1. **改**：`edit_file` 精确替换（old_string 从刚读过的真实内容取，不凭记忆拼）；语义级重命名用 `rename_refactoring`（自动连带所有引用，不要手工逐处改）；格式化用 `reformat_file`。
2. **编译**：`build_project`（或 shell 构建），以 BUILD SUCCESS 为准。大型项目首次构建常见 5-10 分钟，编译期持续输出属正常，耐心等待，不要中途判定卡死而取消。
3. **查问题**：`get_file_problems` 对改动文件——改前先记基线，改后对比，新出现的问题就是你引入的，修掉。
4. **测试**：跑与改动相关的测试并核对通过数；全量测试在收尾阶段兜底。

## 三、调试：跑起来并定位问题

标准顺序：

1. **找配置**：`get_run_configurations` 看可用的运行/调试配置（IDE 自动检测 + 用户自定义）。
2. **启动**：`execute_run_configuration`（`waitForExit=true` + 短 timeout 拿输出快照）；长驻服务转后台后 `read_file` 日志文件看新输出。
3. **确认在跑**：`get_running_processes` 现查进程状态，不要猜。
4. **断点调试**：走 IDE 的调试配置（以工具列表实际提供的 debug 工具为准）——断点、变量、调用栈在界面里可见；不要用 shell 起进程再 print 硬凑，那是纯黑盒。
5. **定位循环**：拿到报错文本 → `search_text` 定位 → `find_references` 追链路 → 读实现 → 修复 → 回到第 2 步重新验证。

## 工具边界（为什么这样分工）

- IDE 语义工具的操作在界面有 UI 反馈（搜索命中、引用图、构建面板、调试断点），用户看得见、跟得上；shell 是黑盒。**语义工具能做的不要绕 shell**——shell 只做 git、进程管理、语义工具没有的脚本。
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
