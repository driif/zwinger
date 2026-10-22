// No imports, so the node tests can load it.

export type AgentKind = "pi" | "claude" | "codex" | "gemini";

export const AGENT_NAMES: Record<AgentKind, string> = {
  pi: "pi",
  claude: "Claude Code",
  codex: "Codex",
  gemini: "Gemini CLI",
};
