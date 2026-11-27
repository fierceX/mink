use super::*;

pub(super) async fn run_interactive(
    cmd_tx: mpsc::UnboundedSender<RuntimeCmd>,
    home: &Path,
) -> Result<()> {
    #[cfg(feature = "repl")]
    let history_path = home.join(".mink/history");
    #[cfg(not(feature = "repl"))]
    let _ = home;
    let (exit_tx, exit_rx) = tokio::sync::mpsc::channel::<()>(1);
    let last_interrupt = Arc::new(std::sync::Mutex::new(None::<Instant>));
    let signal_task = {
        let sig_tx = cmd_tx.clone();
        let sig_exit_tx = exit_tx;
        let last_interrupt = last_interrupt.clone();
        tokio::spawn(async move {
            loop {
                if tokio::signal::ctrl_c().await.is_err() {
                    return;
                }
                let now = Instant::now();
                let should_exit = {
                    let mut guard = last_interrupt.lock().unwrap_or_else(|e| e.into_inner());
                    if guard.is_some_and(|prev| now.duration_since(prev) < Duration::from_secs(2)) {
                        true
                    } else {
                        *guard = Some(now);
                        false
                    }
                };
                if should_exit {
                    let _ = sig_tx.send(RuntimeCmd::Exit);
                    let _ = sig_exit_tx.send(()).await;
                    return;
                }
                let _ = sig_tx.send(RuntimeCmd::Interrupt);
            }
        })
    };
    let rt_handle = tokio::runtime::Handle::current();
    let repl_result = tokio::task::spawn_blocking(move || {
        #[cfg_attr(not(feature = "repl"), allow(unused_mut))]
        let mut exit_rx = exit_rx;
        #[cfg(feature = "repl")]
        {
            if let Some(parent) = history_path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let mut rl = match rustyline::DefaultEditor::new() {
                Ok(r) => r,
                Err(_) => {
                    eprintln!("Failed to initialize readline. Running in simple mode.");
                    simple_stdin_loop(&rt_handle, &cmd_tx, exit_rx);
                    return;
                }
            };

            let _ = rl.load_history(&history_path);
            let history_file = history_path.clone();

            loop {
                let line = match rl.readline("> ") {
                    Ok(s) => s.trim_end().to_string(),
                    Err(rustyline::error::ReadlineError::Eof) => break,
                    Err(rustyline::error::ReadlineError::Interrupted) => {
                        let now = Instant::now();
                        let should_exit = {
                            let mut guard =
                                last_interrupt.lock().unwrap_or_else(|e| e.into_inner());
                            if guard.is_some_and(|prev| {
                                now.duration_since(prev) < Duration::from_secs(2)
                            }) {
                                true
                            } else {
                                *guard = Some(now);
                                false
                            }
                        };
                        if should_exit {
                            let _ = cmd_tx.send(RuntimeCmd::Exit);
                            break;
                        }
                        let _ = cmd_tx.send(RuntimeCmd::Interrupt);
                        continue;
                    }
                    Err(_) => break,
                };
                if line.is_empty() {
                    continue;
                }
                match dispatch_local_command(&line, &cmd_tx) {
                    LocalCommandOutcome::Shutdown => break,
                    LocalCommandOutcome::Handled => continue,
                    LocalCommandOutcome::PassThrough => {}
                }
                if line == "exit" || line == "quit" {
                    break;
                }
                let _ = rl.add_history_entry(&line);
                let _ = rl.save_history(&history_file);
                let (done_tx, done_rx) = tokio::sync::oneshot::channel();
                if cmd_tx
                    .send(RuntimeCmd::Run {
                        input: line,
                        done: Some(done_tx),
                    })
                    .is_err()
                {
                    break;
                }
                if wait_for_turn_or_exit(&rt_handle, done_rx, &mut exit_rx) {
                    let _ = cmd_tx.send(RuntimeCmd::Exit);
                    break;
                }
            }
            let _ = rl.save_history(&history_file);
        }
        #[cfg(not(feature = "repl"))]
        {
            let _ = last_interrupt;
            eprintln!("Readline support is disabled; running in simple stdin mode.");
            simple_stdin_loop(&rt_handle, &cmd_tx, exit_rx);
        }
    })
    .await;
    signal_task.abort();
    repl_result?;

    Ok(())
}

fn wait_for_turn_or_exit<T>(
    rt_handle: &tokio::runtime::Handle,
    done_rx: oneshot::Receiver<T>,
    exit_rx: &mut tokio::sync::mpsc::Receiver<()>,
) -> bool {
    rt_handle.block_on(async {
        tokio::select! {
            _ = done_rx => false,
            Some(()) = exit_rx.recv() => true,
        }
    })
}

fn simple_stdin_loop(
    rt_handle: &tokio::runtime::Handle,
    cmd_tx: &mpsc::UnboundedSender<RuntimeCmd>,
    mut exit_rx: tokio::sync::mpsc::Receiver<()>,
) {
    use std::io::BufRead;
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        if exit_rx.try_recv().is_ok() {
            break;
        }

        let line = match line {
            Ok(l) => l.trim().to_string(),
            Err(_) => break,
        };
        if line.is_empty() {
            continue;
        }
        if line.starts_with('/') && !mink::runtime::is_web_retrieval_command(&line) {
            match dispatch_local_command(&line, cmd_tx) {
                LocalCommandOutcome::Shutdown => break,
                LocalCommandOutcome::Handled | LocalCommandOutcome::PassThrough => {}
            }
            continue;
        }
        if line == "exit" || line == "quit" {
            break;
        }
        let (done_tx, done_rx) = oneshot::channel();
        if cmd_tx
            .send(RuntimeCmd::Run {
                input: line,
                done: Some(done_tx),
            })
            .is_err()
        {
            break;
        }
        if wait_for_turn_or_exit(rt_handle, done_rx, &mut exit_rx) {
            let _ = cmd_tx.send(RuntimeCmd::Exit);
            break;
        }
        if exit_rx.try_recv().is_ok() {
            break;
        }
    }
}
enum LocalCommandOutcome {
    /// The line was consumed by a local slash command.
    Handled,
    /// Not a local command; the caller decides how to treat it as input.
    PassThrough,
    /// A command-channel send failed; the caller should stop the loop.
    Shutdown,
}

/// Handle local slash commands shared by the readline and simple stdin
/// loops. Unknown `/...` lines are reported as `PassThrough`; the readline
/// loop forwards them as input while the simple loop consumes them.
fn dispatch_local_command(
    line: &str,
    cmd_tx: &mpsc::UnboundedSender<RuntimeCmd>,
) -> LocalCommandOutcome {
    match line {
        _ if line == "/goal" || line.starts_with("/goal ") => {
            match crate::local::parse_goal_command(line) {
                Ok(command) => {
                    if cmd_tx.send(RuntimeCmd::Goal(command)).is_err() {
                        return LocalCommandOutcome::Shutdown;
                    }
                }
                Err(error) => println!("{error}"),
            }
            LocalCommandOutcome::Handled
        }
        "/help" => {
            for line in crate::local::COMMON_COMMAND_HELP
                .iter()
                .chain(crate::local::REPL_EXIT_HELP)
            {
                println!("{line}");
            }
            LocalCommandOutcome::Handled
        }
        "/skills" => {
            list_skills();
            LocalCommandOutcome::Handled
        }
        "/compact" => {
            if cmd_tx.send(RuntimeCmd::Compact).is_err() {
                LocalCommandOutcome::Shutdown
            } else {
                LocalCommandOutcome::Handled
            }
        }
        "/flash" | "/pro" => {
            let model = line.trim_start_matches('/');
            if cmd_tx
                .send(RuntimeCmd::SetModel(model.to_string()))
                .is_err()
            {
                LocalCommandOutcome::Shutdown
            } else {
                LocalCommandOutcome::Handled
            }
        }
        _ if line.starts_with("/model ") => {
            let model = line["/model ".len()..].trim();
            if model.is_empty() {
                LocalCommandOutcome::Handled
            } else if cmd_tx
                .send(RuntimeCmd::SetModel(model.to_string()))
                .is_err()
            {
                LocalCommandOutcome::Shutdown
            } else {
                LocalCommandOutcome::Handled
            }
        }
        _ => LocalCommandOutcome::PassThrough,
    }
}
