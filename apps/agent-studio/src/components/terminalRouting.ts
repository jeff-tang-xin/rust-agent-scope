// 保守语法子集，不是 shell 安全沙箱：只放行明确的单条普通命令。
// 不先尝试执行；复合语法、未知命令、写操作统一交给 Agent 审批。
const SIMPLE_COMMANDS = new Set([
  "pwd", "ls", "cd", "whoami", "hostname", "uname", "date", "uptime",
  "df", "du", "free", "ps", "id", "groups", "cat", "head", "tail",
  "wc", "stat", "file", "which", "whereis", "echo", "printf", "printenv",
]);
export function routeTerminalInput(text: string): "pty" | "agent" {
  const value = text.trim();
  if (!value || /[\n\r\x00-\x1f\x7f;&|<>`$(){}\\]/.test(value)) return "agent";
  // 引号/通配符可能包含 shell 扩展；复杂形式宁可送 Agent，不猜测执行。
  if (/["'!*?\[\]~]/.test(value)) return "agent";
  const parts = value.split(/\s+/);
  if (!SIMPLE_COMMANDS.has(parts[0])) return "agent";
  // 防止自然语言被当作命令参数；非 ASCII 路径可通过 Agent 或 SH 使用。
  if (!/^[\x20-\x7e]+$/.test(value)) return "agent";
  if (parts[0] === "tail" && parts.some((part) => /^-.*[fF]/.test(part) || part === "--follow")) return "agent";
  if (parts[0] === "cat" && parts.length === 1) return "agent";
  return "pty";
}

export interface TerminalAgentLine {
  id: string;
  text: string;
  kind?: "input" | "thinking" | "reply" | "tool" | "confirm";
}
// LLM / 工具文本不能作为 ANSI 控制序列写入终端（OSC52 等）。
export function terminalPlainText(text: string): string {
  return text.replace(/\x1b\][^\x07]*(?:\x07|\x1b\\)/g, "")
    .replace(/\x1b\[[0-?]*[ -/]*[@-~]/g, "")
    .replace(/[\x00-\x08\x0b-\x1f\x7f]/g, "");
}
