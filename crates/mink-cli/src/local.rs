//! Shared local-command help text and skill listing for REPL and TUI.
//!
//! Both terminal modes implement the same slash-command vocabulary and skill
//! report; keeping the copy here prevents the two surfaces drifting apart.

use anyhow::Result;

pub(crate) const COMMON_COMMAND_HELP: &[&str] = &[
    "Commands:",
    "  /flash          Switch to flash alias",
    "  /pro            Switch to pro alias",
    "  /model NAME     Switch to a model name or alias",
    "  /compact        Force context compaction",
    "  /skills         List available skills",
    "  /goal TEXT      Draft a goal, same as /goal create (agent_reviewed)",
    "  /goal refine TEXT     Refine the pending goal proposal",
    "  /goal confirm ID REV  Confirm an exact contract and start execution",
    "  /goal status    Show goal state, evidence level and remaining limits",
    "  /goal pause|resume|cancel|clear|approve|reconcile",
    "  /goal budget TURNS MINUTES [SOFT_TOKENS]  Adjust limits without resuming",
    "  /goal human TEXT | /goal verified TEXT  Select acceptance mode",
    "  /help           Show this help",
];

pub(crate) const REPL_EXIT_HELP: &[&str] = &[
    "  Ctrl+C          Interrupt current task",
    "  Ctrl+C again    Exit REPL",
    "  exit / quit     Exit REPL",
];

#[cfg(feature = "tui")]
pub(crate) const TUI_EXTRA_HELP: &[&str] = &[
    "  Enter           Send task / submit guidance while running",
    "  /inputs         Show pending and unapplied inputs",
    "  /resume ID      Explicitly use an unapplied input for a new turn",
    "  /withdraw ID    Withdraw a pending or unapplied input",
    "  /plan           Open current plan detail",
    "  /goals          Open current goal detail (also click the goal status marker)",
    "  Ctrl+G/Ctrl+O   Open goal detail / fold the newest goal boundary card",
    "  /todos          Open current todo detail",
    "  /artifact ID    Open a bounded artifact preview",
    "  /sub-agent ID   Open sub-agent detail",
    "  Ctrl+C          Interrupt current task",
    "  Ctrl+C again    Exit TUI",
    "  Ctrl+V          Paste clipboard image (macOS)",
    "  Shift+Enter     Newline when supported; Ctrl+J / Alt+Enter portable",
    "  /status         Open full statistics panel",
    "  /details        Select cards/details with the keyboard",
    "  Ctrl+L /latest  Follow latest output",
    "  ↑↓              Move within draft; browse history when empty",
    "  Home / End      Logical line start / end",
    "  Ctrl+A /Ctrl+E  Whole draft start / end",
    "  Ctrl+Z / Alt+Z  Undo / redo (paste is one edit)",
    "  PgUp / PgDn     Scroll one content viewport",
    "  Ctrl+D          Exit only with no draft, images or upload",
    "  Esc             Close overlay/panel or return to main",
    "  /exit  /quit    Exit TUI",
];

pub(crate) const SKILL_LOAD_HINT: &str = "Use --skill NAME or Read skill://NAME to load.";

/// Load the model-discoverable skill list using the CLI home resolution.
pub(crate) fn discoverable_skill_lines() -> Result<Vec<String>> {
    let cwd = std::env::current_dir().unwrap_or_default();
    let home = crate::config::default_home();
    let snapshot = crate::capabilities::CapabilitySnapshot::load_default(&cwd, &home, &[])?;
    Ok(snapshot
        .skills
        .discoverable
        .iter()
        .map(|skill| {
            format!(
                "  {} [{}] - {}",
                skill.skill.name,
                skill.source_label(),
                skill.skill.description
            )
        })
        .collect())
}

pub(crate) fn print_skills() {
    println!("SKILLS");
    println!("{}", "-".repeat(60));
    match discoverable_skill_lines() {
        Ok(lines) => {
            for line in lines {
                println!("{line}");
            }
        }
        Err(error) => println!("Error loading skills: {error}"),
    }
    println!("{SKILL_LOAD_HINT}");
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum GoalCommand {
    Status,
    Create(String),
    Human(String),
    Verified(String),
    Confirm {
        id: String,
        revision: u64,
    },
    Refine(String),
    Pause,
    Resume,
    Cancel,
    Clear,
    Approve(String),
    Reconcile(String),
    Budget {
        turns: u32,
        minutes: u64,
        tokens: Option<u64>,
    },
}
/// `/goal` subcommands whose misspellings must not become objectives.
///
/// Every word the parser branches on belongs here, including the two that take
/// arguments: a typo of `confirm`/`refine` used to fall through and create a
/// goal whose objective was the typo.
const GOAL_SUBCOMMANDS: [&str; 13] = [
    "confirm",
    "refine",
    "pause",
    "resume",
    "cancel",
    "clear",
    "approve",
    "reconcile",
    "human",
    "verified",
    "budget",
    "status",
    "create",
];

/// Whether `a` becomes `b` by exactly one insertion, deletion or replacement.
fn one_edit_apart(a: &str, b: &str) -> bool {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let (long, short) = if a.len() >= b.len() {
        (&a, &b)
    } else {
        (&b, &a)
    };
    if long.len() - short.len() > 1 {
        return false;
    }
    let (mut i, mut j, mut edits) = (0usize, 0usize, 0usize);
    while i < long.len() && j < short.len() {
        if long[i] == short[j] {
            i += 1;
            j += 1;
            continue;
        }
        edits += 1;
        if edits > 1 {
            return false;
        }
        if long.len() == short.len() {
            j += 1;
        }
        i += 1;
    }
    edits + (long.len() - i) + (short.len() - j) == 1
}

/// Whether `a` and `b` differ by one adjacent transposition (`cancle`/`cancel`).
fn adjacent_transposition(a: &str, b: &str) -> bool {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.len() != b.len() {
        return false;
    }
    let differing: Vec<usize> = (0..a.len()).filter(|i| a[*i] != b[*i]).collect();
    differing.len() == 2
        && differing[1] == differing[0] + 1
        && a[differing[0]] == b[differing[1]]
        && a[differing[1]] == b[differing[0]]
}

/// The subcommand the first argument token looks like a typo of, if any.
///
/// Only a one-edit (or one-transposition) miss counts: `/goal cancal` and
/// `/goal confrim g12` are mistyped subcommands, while `/goal fix the cancal
/// bug` is a sentence and a real objective. Exact subcommand words are left to
/// the parser's own branches.
fn near_miss_goal_subcommand(args: &str) -> Option<&'static str> {
    let token = args.split_whitespace().next()?;
    GOAL_SUBCOMMANDS
        .iter()
        .copied()
        .find(|known| one_edit_apart(token, known) || adjacent_transposition(token, known))
}

pub(crate) fn parse_goal_command(input: &str) -> Result<GoalCommand> {
    anyhow::ensure!(
        input == "/goal"
            || input
                .strip_prefix("/goal")
                .is_some_and(|s| s.starts_with(char::is_whitespace)),
        "expected /goal command"
    );
    let args = input
        .strip_prefix("/goal")
        .ok_or_else(|| anyhow::anyhow!("expected /goal"))?
        .trim();
    Ok(match args {
        "" => GoalCommand::Status,
        "confirm" | "refine" => {
            anyhow::bail!("usage: /goal confirm DRAFT_ID REVISION or /goal refine FEEDBACK")
        }
        _ if args.starts_with("confirm ") => {
            let parts: Vec<_> = args.split_whitespace().skip(1).collect();
            anyhow::ensure!(parts.len() == 2, "usage: /goal confirm DRAFT_ID REVISION");
            GoalCommand::Confirm {
                id: parts[0].into(),
                revision: parts[1].parse()?,
            }
        }
        _ if args.starts_with("refine ") => GoalCommand::Refine(args[7..].trim().into()),
        "pause" => GoalCommand::Pause,
        "resume" => GoalCommand::Resume,
        "cancel" => GoalCommand::Cancel,
        "clear" => GoalCommand::Clear,
        "approve" => GoalCommand::Approve("explicit terminal approval".into()),
        _ if args.starts_with("reconcile ") => GoalCommand::Reconcile(args[10..].trim().into()),
        _ if args.starts_with("approve ") => GoalCommand::Approve(args[8..].trim().into()),
        _ if args.starts_with("human ") => GoalCommand::Human(args[6..].trim().into()),
        _ if args.starts_with("verified ") => GoalCommand::Verified(args[9..].trim().into()),
        _ if args == "budget" || args.starts_with("budget ") => {
            let parts: Vec<_> = args.split_whitespace().skip(1).collect();
            anyhow::ensure!(
                (2..=3).contains(&parts.len()),
                "usage: /goal budget TURNS MINUTES [SOFT_TOKENS]"
            );
            let turns = parts[0].parse()?;
            let minutes = parts[1].parse()?;
            let tokens = parts.get(2).map(|s| s.parse()).transpose()?;
            anyhow::ensure!(
                turns > 0 && minutes > 0 && tokens != Some(0),
                "goal limits must be positive"
            );
            GoalCommand::Budget {
                turns,
                minutes,
                tokens,
            }
        }
        _ => {
            // A misspelled subcommand used to create a goal whose objective was
            // the typo, which then looked like "cancel did nothing".
            if let Some(suggestion) = near_miss_goal_subcommand(args) {
                anyhow::bail!(
                    "unknown goal subcommand {args:?}; did you mean `/goal {suggestion}`? An objective needs a sentence: `/goal <objective>`, `/goal human <objective>` or `/goal verified <objective>`"
                );
            }
            GoalCommand::Create(args.into())
        }
    })
}
/// Only creation and resume hand the session to the goal driver. Status and the
/// remaining control commands must never start execution or wait for it.
pub(crate) fn goal_command_drives_execution(command: &GoalCommand) -> bool {
    matches!(command, GoalCommand::Confirm { .. } | GoalCommand::Resume)
}

pub(crate) async fn execute_goal_command(
    handle: &crate::runtime::AgentRuntimeHandle,
    command: GoalCommand,
) -> Result<String> {
    use crate::runtime::{GoalAcceptance, GoalSpec};
    let created = match &command {
        GoalCommand::Create(text) | GoalCommand::Human(text) | GoalCommand::Verified(text) => {
            let mut spec = GoalSpec::new(text.clone());
            if matches!(command, GoalCommand::Human(_)) {
                spec.acceptance = GoalAcceptance::Human;
            }
            if matches!(command, GoalCommand::Verified(_)) {
                spec.acceptance = GoalAcceptance::Verifier {
                    id: handle
                        .goal_verifier_ids()
                        .into_iter()
                        .next()
                        .ok_or_else(|| {
                            anyhow::anyhow!("set MINK_GOAL_CHECK to configure a terminal checker")
                        })?,
                };
            }
            if let Some(id) = handle.goal_verifier_ids().into_iter().next() {
                spec.checker_id = Some(id);
            }
            handle.request_goal_draft(spec, 3600)?;
            let draft = handle.generate_goal_draft("").await?;
            return Ok(format!(
                "Goal draft {} revision {} awaiting confirmation.\n{}\nConfirm this exact version: /goal confirm {} {}",
                draft.id,
                draft.revision,
                serde_json::to_string_pretty(&draft.spec)?,
                draft.id,
                draft.revision
            ));
        }
        GoalCommand::Refine(feedback) => {
            let draft = handle.generate_goal_draft(feedback).await?;
            return Ok(format!(
                "Goal draft {} revision {}\n{}\nConfirm: /goal confirm {} {}",
                draft.id,
                draft.revision,
                serde_json::to_string_pretty(&draft.spec)?,
                draft.id,
                draft.revision
            ));
        }
        GoalCommand::Confirm { id, revision } => Some(handle.confirm_goal(id, *revision)?),
        _ => None,
    };
    let g = match created {
        Some(g) => g,
        None => {
            let Some(g) = handle.goal_snapshot() else {
                return Ok("No goal.".into());
            };
            match command {
                GoalCommand::Status if g.hidden => {
                    return Ok("No current goal; ended goal history retained.".into());
                }
                GoalCommand::Status => g,
                GoalCommand::Pause => handle.pause_goal(&g.id, g.revision)?,
                GoalCommand::Resume => handle.resume_goal(&g.id, g.revision)?,
                GoalCommand::Cancel => handle.cancel_goal(&g.id, g.revision)?,
                GoalCommand::Clear => {
                    handle.clear_goal(&g.id, g.revision)?;
                    return Ok("Goal display cleared; history retained.".into());
                }
                GoalCommand::Reconcile(evidence) => {
                    handle.reconcile_goal_effects(&g.id, g.revision, evidence)?
                }
                GoalCommand::Approve(evidence) => {
                    handle.approve_goal(&g.id, g.revision, vec![evidence])?
                }
                GoalCommand::Budget {
                    turns,
                    minutes,
                    tokens,
                } => {
                    let mut limits = g.spec.limits.clone();
                    limits.max_turns = turns;
                    limits.deadline_unix_secs =
                        (time::OffsetDateTime::now_utc().unix_timestamp().max(0) as u64)
                            .saturating_add(minutes.saturating_mul(60));
                    limits.soft_token_limit = tokens;
                    handle.update_goal_limits(&g.id, g.revision, limits)?
                }
                _ => unreachable!(),
            }
        }
    };
    Ok(format!(
        "Goal {} revision {}: {:?}\n{}\nAcceptance {:?}; completion basis {:?}\nRemaining {}/{} turns; absolute deadline {}; token soft limit {:?}; unknown requests {}\n{}",
        g.id,
        g.revision,
        g.status,
        g.spec.objective,
        g.spec.acceptance,
        g.completion_basis,
        g.remaining_turns(),
        g.spec.limits.max_turns,
        g.spec.limits.deadline_unix_secs,
        g.spec.limits.soft_token_limit,
        g.usage.unreported_request_count,
        g.reason.as_deref().unwrap_or("")
    ))
}

#[test]
fn only_goal_creation_and_resume_start_the_driver() {
    let drives = |c: GoalCommand| goal_command_drives_execution(&c);
    assert!(!drives(GoalCommand::Create("x".into())));
    assert!(!drives(GoalCommand::Human("x".into())));
    assert!(!drives(GoalCommand::Verified("x".into())));
    assert!(drives(GoalCommand::Confirm {
        id: "draft".into(),
        revision: 1
    }));
    assert!(drives(GoalCommand::Resume));
    for quiet in [
        GoalCommand::Status,
        GoalCommand::Pause,
        GoalCommand::Cancel,
        GoalCommand::Clear,
        GoalCommand::Approve("x".into()),
        GoalCommand::Reconcile("x".into()),
        GoalCommand::Budget {
            turns: 5,
            minutes: 10,
            tokens: None,
        },
    ] {
        assert!(
            !drives(quiet.clone()),
            "{quiet:?} must not start the driver"
        );
    }
}

#[cfg(test)]
mod goal_tests {
    use super::*;
    #[test]
    fn shared_goal_commands_parse_limits_and_preserve_objective() {
        assert_eq!(parse_goal_command("/goal").unwrap(), GoalCommand::Status);
        assert_eq!(
            parse_goal_command("/goal fix the build").unwrap(),
            GoalCommand::Create("fix the build".into())
        );
        assert_eq!(
            parse_goal_command("/goal pause").unwrap(),
            GoalCommand::Pause
        );
        assert_eq!(
            parse_goal_command("/goal budget 30 90 4000").unwrap(),
            GoalCommand::Budget {
                turns: 30,
                minutes: 90,
                tokens: Some(4000)
            }
        );
        assert!(parse_goal_command("/goal budget 0 10").is_err());
        assert!(parse_goal_command("/goal budget 1").is_err());
    }

    #[test]
    fn mistyped_subcommand_is_refused_with_a_suggestion() {
        // A near miss used to become a goal whose objective was the typo.
        let error = parse_goal_command("/goal cancal").expect_err("typo must be refused");
        let message = error.to_string();
        assert!(message.contains("cancel"), "suggestion missing: {message}");
        assert!(parse_goal_command("/goal cancle").is_err());
        assert!(parse_goal_command("/goal pasue").is_err());
        assert!(parse_goal_command("/goal cance").is_err());
        assert!(parse_goal_command("/goal cancell").is_err());
        // The two argument-taking subcommands are guarded too, with and
        // without their arguments.
        let error = parse_goal_command("/goal confrim").expect_err("confirm typo must be refused");
        assert!(error.to_string().contains("confirm"), "{error}");
        assert!(parse_goal_command("/goal refne").is_err());
        assert!(parse_goal_command("/goal confrim draft-13").is_err());
        assert!(parse_goal_command("/goal refne 增加证据要求").is_err());
        // Bare exact words keep their usage error instead of becoming goals.
        assert!(parse_goal_command("/goal confirm").is_err());
        assert!(parse_goal_command("/goal refine").is_err());
        // A word that is not close to any subcommand stays an objective.
        assert_eq!(
            parse_goal_command("/goal refactor").unwrap(),
            GoalCommand::Create("refactor".into())
        );
        // Real objectives stay objectives: multi-token text, longer text, and
        // non-latin scripts are not subcommand typos.
        assert_eq!(
            parse_goal_command("/goal fix the cancal bug").unwrap(),
            GoalCommand::Create("fix the cancal bug".into())
        );
        assert_eq!(
            parse_goal_command("/goal 修复构建").unwrap(),
            GoalCommand::Create("修复构建".into())
        );
        assert_eq!(
            parse_goal_command("/goal ship the release").unwrap(),
            GoalCommand::Create("ship the release".into())
        );
        // A first token that is not close to any subcommand stays an objective.
        assert_eq!(
            parse_goal_command("/goal refactor the parser").unwrap(),
            GoalCommand::Create("refactor the parser".into())
        );
        // An exact first token keeps its own branch even with arguments.
        assert_eq!(
            parse_goal_command("/goal refine the parser").unwrap(),
            GoalCommand::Refine("the parser".into())
        );
    }

    #[test]
    fn help_lists_every_goal_subcommand() {
        let help = COMMON_COMMAND_HELP.join("\n");
        for word in GOAL_SUBCOMMANDS {
            assert!(help.contains(word), "{word} missing from /goal help");
        }
        assert!(
            !help.contains("model-reported"),
            "retired acceptance mode still advertised: {help}"
        );
        assert!(help.contains("agent_reviewed"));
    }
}
