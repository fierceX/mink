#[test]
fn terminal_osc_component_removes_control_and_delimiters() {
    assert_eq!(super::terminal_osc_component("a;b\nc\x1b\x07d"), "a b c  d");
}

#[cfg(target_os = "macos")]
#[test]
fn native_mac_terminals_do_not_spawn_a_second_platform_notification() {
    for program in ["ghostty", "iTerm.app", "WezTerm", "wezterm"] {
        assert_eq!(
            super::mac_notification_activation(Some(program), None),
            None
        );
    }
    for bundle in [
        "com.mitchellh.ghostty",
        "com.googlecode.iterm2",
        "com.github.wez.wezterm",
    ] {
        assert_eq!(
            super::mac_notification_activation(Some("tmux"), Some(bundle)),
            None
        );
    }
    assert_eq!(super::mac_notification_activation(None, None), None);
    assert_eq!(super::mac_notification_activation(None, Some("")), None);
}

#[cfg(target_os = "macos")]
#[test]
fn platform_mac_notification_click_activates_the_terminal_without_a_script() {
    let activate = super::mac_notification_activation(Some("Apple_Terminal"), None).unwrap();
    assert_eq!(activate, "com.apple.Terminal");
    let notification = super::TaskNotification::new(super::TaskNotificationKind::Completed, "pro");
    let command = super::mac_notification_command(&notification, activate);
    assert_eq!(command.get_program(), "terminal-notifier");
    let args: Vec<_> = command.get_args().collect();
    assert_eq!(
        &args[args.len() - 2..],
        &["-activate", "com.apple.Terminal"]
    );
    assert!(!args.iter().any(|arg| *arg == "-execute" || *arg == "-open"));
    assert_eq!(
        super::mac_notification_activation(None, Some("custom.terminal")),
        Some("custom.terminal")
    );
}

#[test]
fn terminal_notification_sanitizes_payload_and_mac_emits_only_one_notification() {
    let notification = super::TaskNotification {
        kind: super::TaskNotificationKind::Completed,
        title: "mink;\x1btitle".into(),
        body: "body\x07\nline".into(),
    };
    let mut output = Vec::new();
    super::write_terminal_notification(&mut output, &notification).unwrap();
    let output = String::from_utf8(output).unwrap();
    assert!(output.ends_with('\x07'));
    #[cfg(target_os = "macos")]
    {
        assert!(output.starts_with("\x1b]9;mink  title: body  line\x07"));
        assert_eq!(output.matches("\x1b]").count(), 1);
        assert!(!output.contains("777;notify"));
    }
    #[cfg(not(target_os = "macos"))]
    assert!(output.starts_with("\x1b]9;body  line\x07"));
}
