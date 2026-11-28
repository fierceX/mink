use crate::config::{
    CliEarlyExit, apply_config_file, apply_provider_defaults, apply_sdk_request_options,
    parse_args, validate_runtime_config,
};
use crate::runtime::{
    AgentEventKind, AgentOptions, AgentRuntime, AgentRuntimeHandle, PresentedToolResultDisplay,
    RuntimeResult, ToolCallDisplay, ToolResultDisplay, TurnOutcome, exit_code_from_turn,
    final_from_outcome, runtime_skills_from_sdk_request, skill_discovery_policy_from_sdk_request,
};
use crate::sdk_protocol::{
    PROTOCOL_VERSION, SdkRequest, emit_failed_parse, parse_agent_jsonl_request,
    validate_sdk_request,
};
use crate::ui::engine::TerminalDisplay;
use crate::ui::replay::replay_last_turns;
use crate::ui::{Display, SubAgentStreamSink};
use anyhow::Result;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};

pub(crate) enum RuntimeCmd {
    Run {
        input: String,
        done: Option<oneshot::Sender<RuntimeResult<TurnOutcome>>>,
    },
    #[cfg(feature = "tui")]
    RenderTuiStream {
        stream: crate::runtime::AgentEventStream,
        signals: mpsc::UnboundedSender<crate::tui::TuiSignal>,
    },
    Compact,
    Goal(crate::local::GoalCommand),
    SetModel(String),
    Interrupt,
    Exit,
}

mod broker;
mod help;
mod repl;
mod sessions;

#[cfg(all(test, feature = "tui"))]
use broker::goal_boundary_signal;
pub(crate) use broker::start_runtime_broker;
use broker::{GoalTerminalSink, run_turn_rendered};
use help::{print_usage, version_line};
use repl::run_interactive;
use sessions::{auto_set_session_title, list_sessions, list_skills, read_session_alias};

pub struct CliExit {
    pub code: i32,
}

/// Shared binary entrypoint for both `mink` and `mink-core`: installs the
/// terminal-restoring panic hook, bootstraps OS sandboxing before any threads
/// spawn, then starts the Tokio runtime and executes `main_entry`.
pub fn run_cli_binary() -> ! {
    install_panic_hook();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let preparsed = match bootstrap_sandbox_with_preparse(&args) {
        Ok(preparsed) => preparsed,
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("Error: failed to start async runtime: {e}");
            std::process::exit(1);
        }
    };
    let code = match runtime.block_on(main_entry_with_preparsed(args, preparsed)) {
        Ok(exit) => exit.code,
        Err(e) => {
            eprintln!("Error: {e}");
            1
        }
    };
    std::process::exit(code);
}

pub fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        #[cfg(feature = "tui")]
        {
            // Restore all active TUI terminal modes *before* printing the panic
            // payload. Release builds use `panic = "abort"`, so Drop guards
            // never run; in debug builds restoring first also prevents leaving
            // the alternate screen from wiping the panic output.
            crate::tui::restore_terminal_on_exit();
        }
        eprintln!("{info}");
    }));
}

struct PreparsedCli {
    cfg: crate::config::CliConfig,
    provider_defaults_applied: bool,
}

pub async fn main_entry(args: Vec<String>) -> Result<CliExit> {
    main_entry_with_preparsed(args, None).await
}

async fn main_entry_with_preparsed(
    args: Vec<String>,
    preparsed: Option<PreparsedCli>,
) -> Result<CliExit> {
    let cwd = std::env::current_dir()?;
    let home = crate::config::default_home();

    let (mut cfg, provider_defaults_applied, sandbox_checked) = match preparsed {
        Some(pre) => (pre.cfg, pre.provider_defaults_applied, true),
        None => {
            let mut cfg = match parse_args(args) {
                Ok(v) => v,
                Err(e) => match e.downcast_ref::<CliEarlyExit>() {
                    Some(CliEarlyExit::Help) => {
                        print_usage();
                        return Ok(CliExit { code: 0 });
                    }
                    Some(CliEarlyExit::Version) => {
                        println!("{}", version_line());
                        return Ok(CliExit { code: 0 });
                    }
                    None => return Err(e),
                },
            };

            if cfg.list_sessions {
                list_sessions(&home, &cwd).await?;
                return Ok(CliExit { code: 0 });
            }
            if cfg.list_skills {
                list_skills();
                return Ok(CliExit { code: 0 });
            }

            apply_config_file(&mut cfg)?;
            if let Some(message) =
                tui_stream_conflict(cfg.tui_mode.enabled(), cfg.agent_jsonl, cfg.output_format)
            {
                anyhow::bail!(message);
            }
            (cfg, false, false)
        }
    };

    if !cfg.agent_jsonl {
        if !provider_defaults_applied {
            apply_provider_defaults(&mut cfg)?;
        }
        if !sandbox_checked {
            reexec_if_sandbox(&cfg)?;
        }
    }

    // ═══ SDK protocol mode: early stdin parsing (before context creation) ═══
    let sdk_request: Option<SdkRequest> = if cfg.agent_jsonl {
        // Self-sandboxing must happen before reading SDK stdin so the
        // sandboxed child process inherits the original pipe with data intact.
        if !sandbox_checked {
            reexec_if_sandbox(&cfg)?;
        }
        let mut input = String::new();
        tokio::io::AsyncReadExt::read_to_string(&mut tokio::io::stdin(), &mut input).await?;
        let req = match parse_agent_jsonl_request(&input) {
            Ok(req) => req,
            Err(e) => {
                emit_failed_parse(&e);
                return Ok(CliExit { code: 1 });
            }
        };
        if let Some(version) = req.version
            && version != PROTOCOL_VERSION
        {
            emit_failed_parse(&format!(
                "unsupported SDK protocol version: {version}, expected {PROTOCOL_VERSION}"
            ));
            return Ok(CliExit { code: 1 });
        }
        if let Err(e) = validate_sdk_request(&req) {
            emit_failed_parse(&e);
            return Ok(CliExit { code: 1 });
        }
        apply_sdk_request_options(&mut cfg, &req);
        Some(req)
    } else {
        None
    };

    if cfg.agent_jsonl {
        apply_provider_defaults(&mut cfg)?;
    }

    if let Err(error) = validate_runtime_config(&cfg) {
        if cfg.agent_jsonl {
            emit_failed_parse(&format!("invalid SDK request: {error}"));
            return Ok(CliExit { code: 1 });
        }
        return Err(error);
    }
    let prompt_for_title = sdk_request
        .as_ref()
        .map(|r| r.prompt.clone())
        .or_else(|| (!cfg.prompt.trim().is_empty()).then(|| cfg.prompt.clone()));

    // Determine interactive mode early, before ctx creation.
    // StreamJson 输出（--print/--agent-jsonl）没有人类可读输出，
    // 不得落入 REPL（否则 REPL 渲染全部被抑制、只剩 stderr 提示符）。
    let is_stream_json_output =
        cfg.output_format == crate::config::OutputFormat::StreamJson || cfg.agent_jsonl;
    let is_interactive = cfg.interactive
        || (cfg.prompt.is_empty() && std::io::stdin().is_terminal() && !is_stream_json_output);
    cfg.interactive = is_interactive;
    let is_stream_json = is_stream_json_output;

    // TUI channels (if tui_mode). Created before display so signal_tx is available.
    #[cfg(feature = "tui")]
    let tui_tx = if cfg.tui_mode.enabled() {
        let (signal_tx, signal_rx) = mpsc::unbounded_channel::<crate::tui::TuiSignal>();
        Some((signal_tx, signal_rx))
    } else {
        None
    };
    #[cfg(not(feature = "tui"))]
    {
        if cfg.tui_mode.enabled() {
            anyhow::bail!("this mink binary was built without the `tui` feature");
        }
    }

    #[cfg(feature = "tui")]
    let display: Arc<dyn Display> = if let Some((ref tx, ..)) = tui_tx {
        Arc::new(crate::tui::TuiDisplay::new(tx.clone())) as Arc<dyn Display>
    } else {
        Arc::new(TerminalDisplay::new(is_interactive, is_stream_json))
    };
    #[cfg(not(feature = "tui"))]
    let display: Arc<dyn Display> = Arc::new(TerminalDisplay::new(is_interactive, is_stream_json));

    // TUI mode: store mpsc sender for sub-agent streaming
    #[cfg(feature = "tui")]
    let sub_stream_tx: Option<Arc<dyn SubAgentStreamSink>> = tui_tx.as_ref().map(|(tx, ..)| {
        Arc::new(crate::tui::TuiSubAgentStreamSink::new(tx.clone())) as Arc<dyn SubAgentStreamSink>
    });
    #[cfg(not(feature = "tui"))]
    let sub_stream_tx: Option<Arc<dyn SubAgentStreamSink>> = None;

    let mut runtime_options =
        assemble_runtime_options(&cfg, home.clone(), cwd.clone()).with_project_scoped_sessions();
    if let Some(prompt) = prompt_for_title {
        runtime_options = runtime_options.with_first_prompt(prompt);
    }
    if let Some(layout) = sdk_request
        .as_ref()
        .and_then(|request| request.options.session.session_layout)
    {
        runtime_options = runtime_options.with_session_layout(layout);
    }
    if let Some(request) = sdk_request.as_ref() {
        for skill in runtime_skills_from_sdk_request(request) {
            runtime_options = runtime_options.with_runtime_skill(skill);
        }
        if let Some(policy) = skill_discovery_policy_from_sdk_request(request) {
            runtime_options = runtime_options.with_skill_discovery_policy(policy);
        }
    }
    if let Some(sub_stream_tx) = sub_stream_tx {
        runtime_options = runtime_options.with_sub_stream_tx(sub_stream_tx);
    }
    let goal_plugin = if !cfg.agent_jsonl {
        let mut plugin = mink_goal::GoalPlugin::new();
        if let Ok(command) = std::env::var("MINK_GOAL_CHECK") {
            plugin =
                plugin.with_verifier(Arc::new(mink_goal::CommandGoalVerifier::new(command, 60)?));
        }
        plugin = plugin.with_event_sink(Arc::new(GoalTerminalSink {
            turns: true,
            display: display.clone(),
            #[cfg(feature = "tui")]
            signals: tui_tx.as_ref().map(|(tx, ..)| tx.clone()),
        }));
        plugin.install_into(&mut runtime_options);
        runtime_options = runtime_options.with_event_sink(Arc::new(GoalTerminalSink {
            turns: false,
            display: display.clone(),
            #[cfg(feature = "tui")]
            signals: tui_tx.as_ref().map(|(tx, ..)| tx.clone()),
        }));
        Some(plugin)
    } else {
        None
    };
    let runtime = AgentRuntime::start(runtime_options).await?;
    let runtime_handle = runtime.handle();
    let goals = if let Some(plugin) = goal_plugin {
        match mink_goal::GoalEngine::attach(runtime_handle.clone(), plugin) {
            Ok(goals) => Some(goals),
            Err(error) => {
                finish_turn(Err(error.into()), runtime.shutdown()).await?;
                unreachable!("finish_turn always propagates Err")
            }
        }
    } else {
        None
    };
    let session = runtime.session_info().clone();

    let mut process_exit_code = 0i32;
    let turn_result: anyhow::Result<()> = async {
        if cfg.agent_jsonl {
            let outcome = run_turn_rendered(
                &runtime_handle,
                sdk_request
                    .map(|request| request.prompt)
                    .unwrap_or_default(),
                display.as_ref(),
            )
            .await?;
            crate::sdk_protocol::emit_json_line(&final_from_outcome(&outcome));
            process_exit_code = exit_code_from_turn(outcome.status);
        } else if cfg.tui_mode.enabled() {
            #[cfg(feature = "tui")]
            {
                let cmd_tx =
                    start_runtime_broker(runtime_handle.clone(), goals.clone(), display.clone());
                if let Some((signal_tx, signal_rx)) = tui_tx {
                    let model_label = crate::config::resolve_model_label(
                        &cfg.provider.model,
                        &cfg.provider.model_aliases,
                    );
                    launch_tui_with(crate::tui::run_tui(
                        cfg.tui_mode,
                        signal_rx,
                        cmd_tx.clone(),
                        &session,
                        &model_label,
                        &cfg.sandbox,
                        crate::tui::TuiRuntime {
                            handle: runtime_handle.clone(),
                            goals: goals.clone(),
                            signals: signal_tx,
                        },
                    ))
                    .await?;
                }
            }
            #[cfg(not(feature = "tui"))]
            anyhow::bail!("this mink binary was built without the `tui` feature");
        } else if cfg.prompt == "/goal" || cfg.prompt.starts_with("/goal ") {
            let goals = goals
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("goal engine is unavailable in this mode"))?;
            // Parse before touching the driver: a status or control command must
            // neither start goal execution nor block waiting for it.
            let command = crate::local::parse_goal_command(&cfg.prompt)?;
            let drives = crate::local::goal_command_drives_execution(&command);
            if drives {
                // Arm only: the create/resume command itself starts the run,
                // so this path never schedules a second one.
                goals.start_driver()?;
            }
            display.render_info(&crate::local::execute_goal_command(goals, command).await?);
            if drives {
                let outcome = goals.wait().await?;
                process_exit_code = if outcome.status == mink_goal::GoalStatus::Completed {
                    0
                } else {
                    1
                };
            }
        } else if is_interactive {
            let cmd_tx =
                start_runtime_broker(runtime_handle.clone(), goals.clone(), display.clone());
            // UI and input wiring are ready: enable scheduling and pick up a
            // goal the previous session left active.
            if let Some(goals) = &goals {
                goals.start_driver_and_run()?;
            }
            display.render_info("mink interactive mode (type 'exit' or Ctrl+D to quit)");
            if !session.is_new {
                let events_path = session.events_path.clone();
                let _ = tokio::task::spawn_blocking(move || replay_last_turns(&events_path)).await;
            }
            run_interactive(cmd_tx, &home).await?;
        } else if !cfg.prompt.is_empty() {
            let outcome =
                run_turn_rendered(&runtime_handle, cfg.prompt.clone(), display.as_ref()).await?;
            emit_stream_json_final_if_needed(&cfg, &outcome);
            process_exit_code = exit_code_from_turn(outcome.status);
        } else {
            let mut input = String::new();
            tokio::io::AsyncReadExt::read_to_string(&mut tokio::io::stdin(), &mut input).await?;
            let outcome = run_turn_rendered(&runtime_handle, input, display.as_ref()).await?;
            emit_stream_json_final_if_needed(&cfg, &outcome);
            process_exit_code = exit_code_from_turn(outcome.status);
        }

        // Auto-generate session title from first user input if missing
        auto_set_session_title(&session).await;
        Ok(())
    }
    .await;

    finish_turn(turn_result, runtime.shutdown()).await?;

    if !session.session_id.is_empty()
        && !cfg.agent_jsonl
        && cfg.output_format != crate::config::OutputFormat::StreamJson
    {
        let alias_label = read_session_alias(&session).await;
        let label = alias_label.as_deref().unwrap_or(&session.session_ref);
        eprintln!(
            "\x1b[90mResume with: --session {}  or  --continue\x1b[0m",
            label
        );
    }

    Ok(CliExit {
        code: process_exit_code,
    })
}

fn assemble_runtime_options(
    cfg: &crate::config::CliConfig,
    home: PathBuf,
    cwd: PathBuf,
) -> AgentOptions {
    let session = if !cfg.session_id.trim().is_empty() {
        crate::runtime::SessionPolicy::UseOrCreate(cfg.session_id.trim().to_string())
    } else if cfg.continue_session {
        crate::runtime::SessionPolicy::ContinueLatest
    } else {
        crate::runtime::SessionPolicy::New
    };
    let mut options = AgentOptions::new(home, cwd)
        .with_provider_options(cfg.provider.clone())
        .with_generation_options(cfg.generation.clone())
        .with_context_policy(cfg.context.clone())
        .with_tool_options(cfg.tools.clone())
        .with_signal_policy(cfg.signal_policy)
        .with_llm_recovery(cfg.llm_recovery)
        .with_session(session)
        .with_output_format(cfg.output_format)
        .with_verbose(cfg.verbose)
        .with_log_events(cfg.log_events)
        .with_selected_skills(cfg.skills.clone())
        .with_sandbox(cfg.sandbox.clone())
        .with_sandbox_python(cfg.sandbox_python.clone())
        .with_ptc(cfg.ptc.clone())
        .with_web_options(cfg.web)
        .with_interactive(cfg.interactive);
    if !cfg.prompt.trim().is_empty() {
        options = options.with_first_prompt(cfg.prompt.clone());
    }
    if let Some(path) = &cfg.mission_file {
        options = options.with_mission_file(path.clone());
    }
    if let Some(content) = &cfg.mission_content {
        options = options.with_mission_content(content.clone());
    }
    // Image capability wiring (v7 §3.1): an explicit override beats the
    // backend declaration; an explicit vision-model list replaces the
    // built-in default list (empty disables image capture).
    if let Some(capability) = cfg.image_input.clone() {
        options = options.with_image_input(capability);
    }
    if let Some(overrides) = &cfg.image_limits {
        options = options.with_image_limits(overrides.clone());
    }
    if let Some(models) = &cfg.vision_models {
        options = options.with_vision_models(models.clone());
    }
    options
}

/// Run the TUI launcher and surface failures with a stable context prefix.
///
/// Private seam for tests; deliberately not a long-term public launcher
/// trait. `shutdown` still runs in the caller after this returns.
#[cfg(feature = "tui")]
async fn launch_tui_with(
    run: impl std::future::Future<Output = anyhow::Result<()>>,
) -> anyhow::Result<()> {
    run.await
        .map_err(|error| anyhow::anyhow!("TUI error: {error}"))
}

/// Await the runtime shutdown even when `turn_result` failed, then combine
/// both outcomes (shutdown must run before the process decides the exit code).
async fn finish_turn(
    turn_result: anyhow::Result<()>,
    shutdown: impl std::future::Future<Output = crate::runtime::RuntimeResult<()>>,
) -> anyhow::Result<()> {
    let shutdown_result = shutdown.await;
    combine_turn_and_shutdown(turn_result, shutdown_result)
}

/// Combine the turn result with the shutdown result.
///
/// `shutdown` always runs (including when the turn/TUI failed); a failing
/// cleanup must not mask the primary error, and a primary error must not hide
/// that cleanup also failed.
fn combine_turn_and_shutdown(
    turn_result: anyhow::Result<()>,
    shutdown_result: crate::runtime::RuntimeResult<()>,
) -> anyhow::Result<()> {
    match (turn_result, shutdown_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(error)) => Err(error.into()),
        (Err(primary), Err(cleanup)) => Err(anyhow::anyhow!(
            "{primary:#}; cleanup also failed: {cleanup}"
        )),
    }
}

/// Run sandbox self-re-exec synchronously before spinning up the Tokio runtime
/// so we do not spawn worker threads solely to `execve()` the sandboxed child.
pub fn bootstrap_sandbox_if_needed(args: &[String]) -> Result<()> {
    bootstrap_sandbox_with_preparse(args).map(|_| ())
}

fn bootstrap_sandbox_with_preparse(args: &[String]) -> Result<Option<PreparsedCli>> {
    let mut cfg = match parse_args(args.to_vec()) {
        Ok(v) => v,
        Err(e) if e.downcast_ref::<CliEarlyExit>().is_some() => return Ok(None),
        Err(_) => return Ok(None), // let `main_entry` surface arg errors uniformly
    };
    if cfg.list_sessions || cfg.list_skills {
        return Ok(None);
    }
    apply_config_file(&mut cfg)?;
    if tui_stream_conflict(cfg.tui_mode.enabled(), cfg.agent_jsonl, cfg.output_format).is_some() {
        return Ok(None);
    }
    let mut provider_defaults_applied = false;
    if !cfg.agent_jsonl {
        if apply_provider_defaults(&mut cfg).is_err() {
            return Ok(Some(PreparsedCli {
                cfg,
                provider_defaults_applied: false,
            }));
        }
        provider_defaults_applied = true;
    }
    reexec_if_sandbox(&cfg)?;
    Ok(Some(PreparsedCli {
        cfg,
        provider_defaults_applied,
    }))
}

fn reexec_if_sandbox(cfg: &crate::config::CliConfig) -> Result<()> {
    if cfg.sandbox.is_active() {
        let current_exe = std::env::current_exe().map_err(|error| {
            anyhow::anyhow!("cannot determine current executable path for sandbox re-exec: {error}")
        })?;
        let args: Vec<String> = std::env::args().collect();
        crate::sandbox::reexec_in_sandbox(&cfg.sandbox, &current_exe, &args);
    }
    Ok(())
}

fn emit_stream_json_final_if_needed(cfg: &crate::config::CliConfig, outcome: &TurnOutcome) {
    if cfg.output_format != crate::config::OutputFormat::StreamJson || cfg.agent_jsonl {
        return;
    }
    crate::sdk_protocol::emit_json_line(&final_from_outcome(outcome));
}

/// `--tui` owns the terminal; stream-json output (`--print`, `--agent-jsonl`)
/// writes machine-readable lines to stdout. Combining them breaks both.
fn tui_stream_conflict(
    tui: bool,
    agent_jsonl: bool,
    output_format: crate::config::OutputFormat,
) -> Option<&'static str> {
    if !tui {
        return None;
    }
    if agent_jsonl {
        return Some("--tui cannot be combined with --agent-jsonl");
    }
    if output_format == crate::config::OutputFormat::StreamJson {
        return Some("--tui cannot be combined with --print/stream-json output");
    }
    None
}

#[cfg(test)]
#[path = "../cli_tests.rs"]
mod tests;
