use std::io::{self, Write};
use std::process::{Command, Stdio};
use std::thread;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TaskNotificationKind {
    Completed,
    Failed,
    Interrupted,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TaskNotification {
    pub kind: TaskNotificationKind,
    pub title: String,
    pub body: String,
}

impl TaskNotification {
    pub(crate) fn new(kind: TaskNotificationKind, model: &str) -> Self {
        let title = match kind {
            TaskNotificationKind::Completed => "mink 任务完成",
            TaskNotificationKind::Failed => "mink 任务失败",
            TaskNotificationKind::Interrupted => "mink 任务已停止",
        };
        let body = match kind {
            TaskNotificationKind::Completed => format!("模型 {model} 已完成当前 TUI 任务。"),
            TaskNotificationKind::Failed => format!("模型 {model} 的当前 TUI 任务已失败。"),
            TaskNotificationKind::Interrupted => {
                format!("模型 {model} 的当前 TUI 任务已停止，未应用引导仍保留。")
            }
        };
        Self {
            kind,
            title: title.into(),
            body,
        }
    }
}

pub(crate) fn send_task_notification(notification: &TaskNotification) {
    let _ = emit_terminal_notification(notification);
    if let Some(mut command) = platform_notification_command(notification) {
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        thread::spawn(move || {
            let _ = command.status();
        });
    }
}

fn emit_terminal_notification(notification: &TaskNotification) -> io::Result<()> {
    write_terminal_notification(&mut io::stdout().lock(), notification)
}

fn write_terminal_notification(
    out: &mut impl Write,
    notification: &TaskNotification,
) -> io::Result<()> {
    let title = terminal_osc_component(&notification.title);
    let body = terminal_osc_component(&notification.body);

    // Ghostty, iTerm2 and WezTerm all support OSC 9. On macOS send one
    // terminal-owned notification, whose click action can focus its surface.
    #[cfg(target_os = "macos")]
    write!(out, "\x1b]9;{title}: {body}\x07")?;
    #[cfg(not(target_os = "macos"))]
    // Retain the rxvt-style fallback on other platforms.
    {
        write!(out, "\x1b]9;{body}\x07")?;
        write!(out, "\x1b]777;notify;{title};{body}\x07")?;
    }
    // Terminal bell fallback. Many terminal apps can promote this to a notification.
    write!(out, "\x07")?;
    out.flush()
}

#[cfg(target_os = "macos")]
fn platform_notification_command(notification: &TaskNotification) -> Option<Command> {
    let program = std::env::var("TERM_PROGRAM").ok();
    let bundle = std::env::var("__CFBundleIdentifier").ok();
    let activate = mac_notification_activation(program.as_deref(), bundle.as_deref())?;
    Some(mac_notification_command(notification, activate))
}

#[cfg(target_os = "macos")]
fn mac_notification_activation<'a>(
    program: Option<&str>,
    bundle: Option<&'a str>,
) -> Option<&'a str> {
    match program {
        Some("ghostty" | "iTerm.app" | "WezTerm" | "wezterm") => None,
        Some("Apple_Terminal") => Some("com.apple.Terminal"),
        _ => match bundle {
            Some("com.mitchellh.ghostty" | "com.googlecode.iterm2" | "com.github.wez.wezterm") => {
                None
            }
            other => other.filter(|id| !id.is_empty()),
        },
    }
}

#[cfg(target_os = "macos")]
fn mac_notification_command(notification: &TaskNotification, activate: &str) -> Command {
    let mut command = Command::new("terminal-notifier");
    command.args([
        "-title",
        &notification.title,
        "-message",
        &notification.body,
        "-sound",
        "Glass",
        "-activate",
        activate,
    ]);
    // Never fall back to osascript: its notifications belong to Script Editor,
    // so clicking them opens the script app rather than the terminal.
    command
}

#[cfg(not(target_os = "macos"))]
fn platform_notification_command(notification: &TaskNotification) -> Option<Command> {
    let mut command = Command::new("notify-send");
    command.arg(&notification.title).arg(&notification.body);
    Some(command)
}

fn terminal_osc_component(value: &str) -> String {
    value
        .chars()
        .map(|ch| match ch {
            '\x1b' | '\x07' => ' ',
            ';' | '\n' | '\r' | '\t' => ' ',
            ch if ch.is_control() => ' ',
            ch => ch,
        })
        .collect()
}

#[cfg(test)]
#[path = "notify_tests.rs"]
mod tests;
