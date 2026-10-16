// Slash commands for the workbench input.
//
// The terminal parses these in `crates/mink-cli/src/tui/command.rs`. The two
// surfaces are kept honest by `crates/mink-cli/src/tui/slash_commands.json`,
// which both parsers are tested against: same inputs, same results.

export type SlashCommand =
  { kind: "goal"; goal: GoalCommand } |
  { kind: "model"; model: string } |
  { kind: "resume"; id: string } |
  { kind: "withdraw"; id: string } |
  { kind: "subagent"; id: string } |
  { kind: "artifact"; id: string } |
  { kind: SimpleCommand };

/** Commands that take no argument. */
export type SimpleCommand =
  "goals" | "help" | "skills" | "plan" | "todos" | "inputs" |
  "status" | "latest" | "details" | "compact" | "flash" | "pro" | "quit";

export type GoalCommand =
  { action: "status" } |
  { action: "create"; objective: string; acceptance: "agent_reviewed" | "human" | "verifier" } |
  { action: "confirm"; id: string; revision: number } |
  { action: "refine"; feedback: string } |
  { action: "pause" } | { action: "resume" } | { action: "cancel" } | { action: "clear" } |
  { action: "approve"; evidence: string } |
  { action: "reconcile"; evidence: string } |
  { action: "budget"; turns: number; minutes: number; tokens: number | null };

export class SlashCommandError extends Error {
  constructor(public readonly input: string, message: string) {
    super(message);
    this.name = "SlashCommandError";
  }
}

/**
 * Commands the workbench cannot honour.
 *
 * `/compact`, `/flash`, `/pro` and `/model` used to be here until the server
 * exposed the runtime controls behind them; only leaving the session still
 * has no equivalent.
 */
const TERMINAL_ONLY = new Set(["quit"]);
export function isTerminalOnly(command: SlashCommand): boolean {
  const kind = command.kind;
  if (kind === "model") return false;
  return TERMINAL_ONLY.has(kind as string);
}

/**
 * Whether this deployment configured a goal checker.
 *
 * Checker acceptance needs a verifier; the terminal reads `MINK_GOAL_CHECK` and
 * the server now does the same, reporting the capability on the goal endpoint.
 * Claiming parity without it would send a request that is guaranteed to fail.
 */
let verifierConfigured = false;
export function setVerifierAvailable(available: boolean): void {
  verifierConfigured = available;
}
export function isVerifierAvailable(): boolean {
  return verifierConfigured;
}

/** `/goal` subcommands whose misspellings must not become objectives. */
const GOAL_SUBCOMMANDS = ["confirm", "refine", "pause", "resume", "cancel", "clear", "approve", "reconcile", "human", "verified", "budget", "status", "create"] as const;

/** Whether `a` becomes `b` by exactly one insertion, deletion or replacement. */
function oneEditApart(a: string, b: string): boolean {
  const left = [...a];
  const right = [...b];
  const [long, short] = left.length >= right.length ? [left, right] : [right, left];
  if (long.length - short.length > 1) return false;
  let i = 0; let j = 0; let edits = 0;
  while (i < long.length && j < short.length) {
    if (long[i] === short[j]) { i += 1; j += 1; continue; }
    edits += 1;
    if (edits > 1) return false;
    if (long.length === short.length) j += 1;
    i += 1;
  }
  return edits + (long.length - i) + (short.length - j) === 1;
}

/** Whether `a` and `b` differ by one adjacent transposition (`cancle`/`cancel`). */
function adjacentTransposition(a: string, b: string): boolean {
  const left = [...a];
  const right = [...b];
  if (left.length !== right.length) return false;
  const differing = left.map((char, index) => (char === right[index] ? -1 : index)).filter(index => index >= 0);
  return differing.length === 2
    && differing[1] === differing[0] + 1
    && left[differing[0]!] === right[differing[1]!]
    && left[differing[1]!] === right[differing[0]!];
}

/**
 * The subcommand the first argument token looks like a typo of, if any.
 *
 * Only a one-edit (or one-transposition) miss counts: `/goal cancal` and
 * `/goal confrim g12` are mistyped subcommands, while `/goal fix the cancal
 * bug` is a sentence and a real objective. Exact subcommand words are left to
 * the branches above.
 */
function nearMissGoalSubcommand(args: string): string | undefined {
  const token = args.split(/\s+/)[0];
  if (token === undefined || token === "") return undefined;
  return GOAL_SUBCOMMANDS.find(known => oneEditApart(token, known) || adjacentTransposition(token, known));
}

function parseGoal(args: string, input: string): GoalCommand {
  if (args === "") return { action: "status" };
  const [head, ...rest] = args.split(/\s+/);
  const tail = args.slice(head!.length).trim();
  switch (head) {
    case "pause":
    case "resume":
    case "cancel":
    case "clear":
      return { action: head };
    case "approve":
      // Bare `/goal approve` mirrors the terminal's own note rather than
      // inventing evidence.
      return { action: "approve", evidence: tail || "explicit terminal approval" };
    case "reconcile":
      if (!tail) throw new SlashCommandError(input, "用法：/goal reconcile 说明");
      return { action: "reconcile", evidence: tail };
    case "human":
    case "verified":
    case "verifier":
      if (!tail) throw new SlashCommandError(input, `用法：/goal ${head} 目标正文`);
      return { action: "create", objective: tail, acceptance: head === "human" ? "human" : "verifier" };
    case "confirm": {
      const parts = tail.split(/\s+/);
      const revision = Number(parts[1]);
      if (parts.length !== 2 || !Number.isSafeInteger(revision) || revision < 1) {
        throw new SlashCommandError(input, "用法：/goal confirm DRAFT_ID REVISION");
      }
      return { action: "confirm", id: parts[0]!, revision };
    }
    case "refine":
      if (!tail) throw new SlashCommandError(input, "用法：/goal refine 修改意见");
      return { action: "refine", feedback: tail };
    case "budget": {
      const parts = rest;
      if (parts.length < 2 || parts.length > 3) {
        throw new SlashCommandError(input, "用法：/goal budget 轮次 分钟 [软Token]");
      }
      const turns = Number(parts[0]);
      const minutes = Number(parts[1]);
      const tokens = parts.length === 3 ? Number(parts[2]) : null;
      if (!Number.isInteger(turns) || turns < 1) throw new SlashCommandError(input, "轮次必须是正整数");
      if (!Number.isFinite(minutes) || minutes < 1) throw new SlashCommandError(input, "时限必须是正数");
      if (parts.length === 3 && (!Number.isFinite(tokens!) || tokens! < 0)) {
        throw new SlashCommandError(input, "Token 软限额必须是非负数");
      }
      return { action: "budget", turns, minutes, tokens };
    }
    default: {
      // `create` and `verified` both create; bare text after /goal creates too,
      // matching the terminal's `/goal <objective>`.
      if (head!.startsWith("-")) throw new SlashCommandError(input, "未知的目标子命令");
      // A misspelled subcommand used to create a goal whose objective was the
      // typo, which then looked like "cancel did nothing".
      const suggestion = nearMissGoalSubcommand(args);
      if (suggestion !== undefined) {
        throw new SlashCommandError(input, `未知的目标子命令 ${JSON.stringify(args)}；是否想用 /goal ${suggestion}？目标正文请写成句子：/goal <正文>、/goal human <正文> 或 /goal verified <正文>`);
      }
      return { action: "create", objective: args, acceptance: "agent_reviewed" };
    }
  }
}

const SIMPLE: Record<string, SimpleCommand> = {
  "/goals": "goals",
  "/help": "help",
  "/skills": "skills",
  "/plan": "plan",
  "/todos": "todos",
  "/inputs": "inputs",
  "/status": "status",
  "/latest": "latest",
  "/details": "details",
  "/compact": "compact",
  "/flash": "flash",
  "/pro": "pro",
};

/**
 * Parse a draft that starts with `/`.
 *
 * Returns `null` for ordinary prose, so the caller can send it as a message.
 * Throws `SlashCommandError` for an unrecognised or malformed command; the
 * draft must survive that, because the user still has to fix it.
 */
export function parseSlashCommand(input: string): SlashCommand | null {
  if (!input.startsWith("/")) return null;
  const trimmed = input.trimEnd();
  if (trimmed === "/goal" || trimmed.startsWith("/goal ")) {
    const args = trimmed.slice("/goal".length).trim();
    return { kind: "goal", goal: parseGoal(args, trimmed) };
  }
  if (SIMPLE[trimmed]) return { kind: SIMPLE[trimmed] };
  if (trimmed === "/exit" || trimmed === "/quit" || trimmed === "/q") return { kind: "quit" };

  const prefixed = (name: string) => {
    if (!trimmed.startsWith(`${name} `)) return null;
    const id = trimmed.slice(name.length).trim();
    if (!id) throw new SlashCommandError(trimmed, `${name} 需要一个标识`);
    return id;
  };
  const model = prefixed("/model");
  if (model) return { kind: "model", model };
  const resume = prefixed("/resume");
  if (resume) return { kind: "resume", id: resume };
  const withdraw = prefixed("/withdraw");
  if (withdraw) return { kind: "withdraw", id: withdraw };
  const subagent = prefixed("/sub-agent");
  if (subagent) return { kind: "subagent", id: subagent };
  const artifact = prefixed("/artifact");
  if (artifact) return { kind: "artifact", id: artifact };

  throw new SlashCommandError(trimmed, `未知命令 ${trimmed.split(/\s+/)[0]}，输入 /help 查看可用命令`);
}

/** True while the draft is being used to type a command rather than prose. */
export function inCommandMode(draft: string): boolean {
  return draft.startsWith("/") && !draft.slice(1).includes("\n");
}

export interface CommandHint {
  readonly command: string;
  readonly usage: string;
  readonly summary: string;
  readonly available: boolean;
}

/** Suggestion list for the current draft, mirroring the terminal's help. */
const ALL_COMMAND_HINTS: readonly CommandHint[] = [
  { command: "/goal", usage: "/goal 目标正文", summary: "创建目标（跨轮执行）", available: true },
  { command: "/goal confirm", usage: "/goal confirm 草案ID 修订号", summary: "确认目标草案并启动", available: true },
  { command: "/goal refine", usage: "/goal refine 修改意见", summary: "修改待确认的目标草案", available: true },
  { command: "/goal human", usage: "/goal human 目标正文", summary: "创建需人工验收的目标", available: true },
  { command: "/goal verified", usage: "/goal verified 目标正文", summary: "创建需检查器验收的目标", available: verifierConfigured },
  { command: "/goal pause", usage: "/goal pause", summary: "暂停当前目标", available: true },
  { command: "/goal resume", usage: "/goal resume", summary: "恢复当前目标", available: true },
  { command: "/goal cancel", usage: "/goal cancel", summary: "取消当前目标", available: true },
  { command: "/goal clear", usage: "/goal clear", summary: "清除目标记录", available: true },
  { command: "/goal budget", usage: "/goal budget 轮次 分钟 [软Token]", summary: "调整目标额度", available: true },
  { command: "/goal approve", usage: "/goal approve 说明", summary: "人工批准完成", available: true },
  { command: "/goal reconcile", usage: "/goal reconcile 说明", summary: "对账未知副作用", available: true },
  { command: "/goals", usage: "/goals", summary: "打开目标详情（等同状态栏标记）", available: true },
  { command: "/plan", usage: "/plan", summary: "查看当前计划", available: true },
  { command: "/todos", usage: "/todos", summary: "查看 Todo", available: true },
  { command: "/inputs", usage: "/inputs", summary: "查看待处理输入", available: true },
  { command: "/details", usage: "/details", summary: "打开详情面板", available: true },
  { command: "/latest", usage: "/latest", summary: "回到最新输出", available: true },
  { command: "/status", usage: "/status", summary: "查看会话状态", available: true },
  { command: "/skills", usage: "/skills", summary: "查看技能", available: true },
  { command: "/help", usage: "/help", summary: "列出可用命令", available: true },
  { command: "/compact", usage: "/compact", summary: "手动压缩上下文", available: true },
  { command: "/flash", usage: "/flash", summary: "切换到 flash 模型别名", available: true },
  { command: "/pro", usage: "/pro", summary: "切换到 pro 模型别名", available: true },
  { command: "/model", usage: "/model 名称", summary: "切换模型", available: true },
  { command: "/resume", usage: "/resume 输入ID", summary: "显式续发未应用输入", available: true },
  { command: "/withdraw", usage: "/withdraw 输入ID", summary: "撤回待处理输入", available: true },
  { command: "/artifact", usage: "/artifact ID", summary: "打开产物", available: true },
  { command: "/sub-agent", usage: "/sub-agent ID", summary: "查看子代理", available: true },
];

/**
 * Suggestions for the current draft.
 *
 * A space ends completion: from `/goal ` onward the user is typing free-form
 * text — the objective of a new goal, or the note for `/goal approve` — and
 * offering subcommands there is noise. Returning nothing for those drafts is
 * also why the caller must not treat an empty list as an error.
 */
/** Hints with the deployment's capabilities applied. */
export const COMMAND_HINTS: readonly CommandHint[] = ALL_COMMAND_HINTS.map((hint) =>
  hint.command === "/goal verified" ? { ...hint, available: verifierConfigured } : hint,
);

export function commandHintsFor(draft: string): CommandHint[] {
  if (!draft.startsWith("/")) return [];
  if (draft === "/") return commandHints();
  // Match on a word boundary: trimming first would make `/goal ` prefix-match
  // `/goals`.
  if (draft.includes(" ")) return [];
  // Plain prefix matching, as every shell does: `/goal pa` completes to
  // `/goal pause`, and `/goal` still offers `/goals` until the next character
  // rules it out.
  return commandHints().filter((hint) => hint.command.startsWith(draft));
}

/** Re-evaluated per render so a capability change reaches the list. */
/** Every command the composer offers, in the contract's order. */
export function allCommandHints(): CommandHint[] {
  return commandHints();
}

function commandHints(): CommandHint[] {
  return ALL_COMMAND_HINTS.map((hint) =>
    hint.command === "/goal verified" ? { ...hint, available: verifierConfigured } : hint,
  );
}
