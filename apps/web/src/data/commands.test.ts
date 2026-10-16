// The workbench parser is tested against the terminal's own fixture, so a
// command added on one surface cannot be missing on the other.
import { describe, it, expect } from "vitest";
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { parseSlashCommand, setVerifierAvailable, SlashCommandError, inCommandMode, commandHintsFor } from "./commands.ts";

interface Case {
  input: string;
  kind?: string;
  goal?: string;
  text?: string;
  turns?: number;
  minutes?: number;
  tokens?: number | null;
  revision?: number;
  acceptance?: string;
  model?: string;
  id?: string;
  error?: boolean;
  not_command?: boolean;
}

// The fixture lives with the terminal parser; read it rather than duplicating,
// so there is exactly one command contract in the repository.
const fixture = resolve(process.cwd(), "../../crates/mink-cli/src/tui/slash_commands.json");
const slashCommands = JSON.parse(readFileSync(fixture, "utf-8")) as { commands: Case[] };

const cases = slashCommands.commands;

describe("slash command contract", () => {
  it("fixture covers every advertised hint", () => {
    expect(cases.length).toBeGreaterThan(20);
  });

  for (const testCase of cases) {
    it(`${testCase.error ? "rejects" : testCase.not_command ? "treats as prose" : "parses"} ${JSON.stringify(testCase.input)}`, () => {
      let parsed: ReturnType<typeof parseSlashCommand> | null = null;
      let threw = false;
      try {
        parsed = parseSlashCommand(testCase.input);
      } catch (error) {
        threw = error instanceof SlashCommandError;
      }
      if (testCase.not_command) {
        expect(threw).toBe(false);
        expect(parsed).toBeNull();
        return;
      }
      if (testCase.error) {
        expect(threw, `${testCase.input} should be rejected`).toBe(true);
        return;
      }
      expect(threw, `${testCase.input} should parse`).toBe(false);
      expect(parsed!.kind).toBe(testCase.kind);
      if (testCase.kind === "goal") {
        const goal = (parsed as { goal: Record<string, unknown> }).goal;
        expect(goal.action).toBe(testCase.goal);
        if (testCase.goal === "create") expect(goal.objective).toBe(testCase.text);
        if (testCase.goal === "confirm") {
          expect(goal.id).toBe(testCase.id);
          expect(goal.revision).toBe(testCase.revision);
        }
        if (testCase.acceptance) expect(goal.acceptance).toBe(testCase.acceptance);
        if (testCase.goal === "approve" || testCase.goal === "reconcile") {
          expect(goal.evidence).toBe(testCase.text);
        }
        if (testCase.goal === "budget") {
          expect(goal.turns).toBe(testCase.turns);
          expect(goal.minutes).toBe(testCase.minutes);
          expect(goal.tokens).toBe(testCase.tokens);
        }
      }
      if (testCase.kind === "model") expect((parsed as { model: string }).model).toBe(testCase.model);
      if (testCase.id && testCase.kind !== "goal") expect((parsed as { id: string }).id).toBe(testCase.id);
    });
  }
});

describe("command mode", () => {
  it("is entered by a leading slash and left by prose", () => {
    expect(inCommandMode("/")).toBe(true);
    expect(inCommandMode("/goal pa")).toBe(true);
    expect(inCommandMode("修复 /goal 那个 bug")).toBe(false);
    expect(inCommandMode("")).toBe(false);
  });

  it("suggests every command for a bare slash", () => {
    expect(commandHintsFor("/").length).toBeGreaterThan(10);
  });

  it("narrows to /goal subcommands", () => {
    const goalHints = commandHintsFor("/goal").map((hint) => hint.command);
    expect(goalHints).toContain("/goal pause");
    // Standard prefix matching keeps `/goals` available until the next
    // character rules it out; only a space ends completion.
    expect(goalHints).toContain("/goals");
    // A multi-word command cannot be completed one fragment at a time: its own
    // space already means "free-form text follows".
  });

  it("stops completing once the user types a space", () => {
    // A space means free-form text: the objective of a new goal, or the note
    // for an approval. Offering subcommands there is noise.
    expect(commandHintsFor("/goal ")).toEqual([]);
    expect(commandHintsFor("/goal pa")).toEqual([]);
    expect(commandHintsFor("/goal human ")).toEqual([]);
    expect(commandHintsFor("/goal approve 已确认 ")).toEqual([]);
    // Other commands behave the same way.
    expect(commandHintsFor("/model ")).toEqual([]);
  });

  it("marks commands the workbench cannot honour", () => {
    setVerifierAvailable(true);
    const hints = commandHintsFor("/");
    // Everything the server exposes is available once a checker is configured;
    // leaving the session still has no equivalent.
    expect(hints.filter((hint) => !hint.available)).toEqual([]);
    for (const command of ["/compact", "/flash", "/pro", "/model", "/goal verified"]) {
      expect(hints.some((hint) => hint.command === command && hint.available)).toBe(true);
    }
  });

  it("hides checker acceptance the deployment cannot honour", () => {
    // No MINK_GOAL_CHECK means the server refuses this acceptance mode, so the
    // workbench must not offer it as if the terminal could.
    setVerifierAvailable(false);
    const verified = commandHintsFor("/").find((hint) => hint.command === "/goal verified");
    expect(verified?.available).toBe(false);
    setVerifierAvailable(true);
  });

  it("keeps the error message actionable", () => {
    expect(() => parseSlashCommand("/nope")).toThrow(/未知命令 \/nope/);
    expect(() => parseSlashCommand("/goal budget 30")).toThrow(/用法/);
    expect(() => parseSlashCommand("/goal reconcile")).toThrow(/用法/);
  });
});
