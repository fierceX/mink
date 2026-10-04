//! Exercise the real process entry point, including its first positional arg.
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "mink-startup-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_mink-server"));
        command
            .current_dir(&self.0)
            .env_remove("MINK_SERVER_HOST")
            .env_remove("MINK_SERVER_PORT")
            .env_remove("MINK_SERVER_MAX_RUNNING")
            .env_remove("MINK_HOME")
            .env_remove("MODEL")
            .env("HOME", &self.0)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    fn config(&self, text: &str) -> PathBuf {
        let path = self.0.join("server.toml");
        std::fs::write(&path, text).unwrap();
        path
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn finishes(mut command: Command) -> Output {
    let mut child = command.spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            child.kill().unwrap();
            let output = child.wait_with_output().unwrap();
            panic!("server did not exit: {output:?}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    child.wait_with_output().unwrap()
}

fn with_path(fixture: &Fixture, path: &Path) -> Command {
    let mut command = fixture.command();
    command.arg(path);
    command
}

#[test]
fn positional_config_controls_the_listener() {
    let fixture = Fixture::new();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let path = fixture.config(&format!(
        "[server]\nhost = \"127.0.0.1\"\nport = {port}\nmax_running = 1\n"
    ));
    let output = finishes(with_path(&fixture, &path));
    assert!(
        !output.status.success(),
        "reserved port must prevent startup"
    );
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(
        error.to_lowercase().contains("address already in use"),
        "{error}"
    );
}

#[test]
fn invalid_and_missing_config_files_fail_before_startup() {
    let fixture = Fixture::new();
    let path = fixture.config("[server]\nmax_running = 0\n");
    let output = finishes(with_path(&fixture, &path));
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("max_running must be >= 1"));
    let output = finishes(with_path(&fixture, &fixture.0.join("missing.toml")));
    assert!(!output.status.success());
    let path = fixture.config("not valid TOML !");
    assert!(!finishes(with_path(&fixture, &path)).status.success());
}

#[test]
fn help_version_and_invalid_arguments_are_handled_without_startup() {
    let fixture = Fixture::new();
    for flag in ["-h", "--help", "-V", "--version"] {
        let mut command = fixture.command();
        command.arg(flag);
        let output = finishes(command);
        assert!(output.status.success(), "{flag}: {output:?}");
        assert!(String::from_utf8_lossy(&output.stdout).contains("mink-server"));
    }
    for args in [
        vec!["--unknown"],
        vec!["one.toml", "two.toml"],
        vec!["--help", "extra"],
    ] {
        let mut command = fixture.command();
        command.args(args);
        assert!(!finishes(command).status.success());
    }
}
