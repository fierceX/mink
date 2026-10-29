//! Exercise the real process entry point, including its first positional arg.
use std::io::{BufRead, BufReader};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Output, Stdio};
use std::time::{Duration, Instant};

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        // Wall-clock nanoseconds collide between tests running in parallel:
        // the system clock is coarse enough that two threads read the same
        // value, and these tests share HOME and `server.toml` with their
        // neighbours. Pair the timestamp with a monotonic sequence.
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "mink-startup-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
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

static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The startup tests each reserve a free port and then release it; running them
/// in parallel lets two tests race for the same port. Serialize process-level
/// starts so the banner assertions are deterministic.
fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|error| error.into_inner())
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

fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

/// A server that answered its startup banner; killed when the test ends.
struct RunningServer {
    child: Child,
    _stdout: Option<ChildStdout>,
}

impl Drop for RunningServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Start a real server, read its first stdout line (the banner), and return the
/// bound port. The port itself is not a CLI option: it comes from the existing
/// environment variable.
fn start_server(
    fixture: &Fixture,
    args: &[&str],
    envs: &[(&str, &str)],
) -> (RunningServer, String, u16) {
    let port = free_port();
    let mut command = fixture.command();
    command.env("MINK_SERVER_PORT", port.to_string());
    for (key, value) in envs {
        command.env(key, value);
    }
    command.args(args);
    let mut child = command.spawn().unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);
    let mut banner = String::new();
    reader.read_line(&mut banner).unwrap();
    let server = RunningServer {
        child,
        _stdout: Some(reader.into_inner()),
    };
    (server, banner, port)
}

#[test]
fn positional_config_controls_the_listener() {
    let _serial = serial();
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
    let _serial = serial();
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
    let _serial = serial();
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
        vec!["--port", "8765"],
        vec!["--host"],
        vec!["--host", "not-an-ip"],
        vec!["--host=127.0.0.1", "--host=127.0.0.2"],
    ] {
        let mut command = fixture.command();
        command.args(args.clone());
        let output = finishes(command);
        assert!(!output.status.success(), "{args:?} must be rejected");
        let stderr = String::from_utf8_lossy(&output.stderr);
        if args[0] == "--port" {
            assert!(stderr.contains("unknown option: --port"), "{stderr}");
        }
        if args[0] == "--host" && args.len() == 1 {
            assert!(stderr.contains("--host requires an IP address"), "{stderr}");
        }
        if args[0] == "--host" && args.len() == 2 {
            assert!(stderr.contains("invalid --host"), "{stderr}");
        }
        if args.len() == 2 && args[1].starts_with("--host=") {
            assert!(stderr.contains("--host may only be given once"), "{stderr}");
        }
    }
}

#[test]
fn host_flag_wins_over_environment_and_config() {
    let _serial = serial();
    let fixture = Fixture::new();
    // Both fallbacks name TEST-NET addresses that cannot be bound here; the
    // server only starts when the CLI value wins.
    let path = fixture.config("[server]\nhost = \"192.0.2.10\"\n");
    let (server, banner, port) = start_server(
        &fixture,
        &[path.to_str().unwrap(), "--host=127.0.0.1"],
        &[("MINK_SERVER_HOST", "192.0.2.11")],
    );
    assert!(
        banner.contains(&format!("http://127.0.0.1:{port}")),
        "banner: {banner}"
    );
    TcpStream::connect(("127.0.0.1", port)).expect("server accepts loopback connections");
    drop(server);
}

#[test]
fn environment_host_wins_over_config_without_the_flag() {
    let _serial = serial();
    let fixture = Fixture::new();
    let path = fixture.config("[server]\nhost = \"192.0.2.10\"\n");
    let (server, banner, port) = start_server(
        &fixture,
        &[path.to_str().unwrap()],
        &[("MINK_SERVER_HOST", "127.0.0.1")],
    );
    assert!(
        banner.contains(&format!("http://127.0.0.1:{port}")),
        "banner: {banner}"
    );
    TcpStream::connect(("127.0.0.1", port)).expect("server accepts loopback connections");
    drop(server);
}

#[test]
fn default_listener_is_loopback_only() {
    let _serial = serial();
    let fixture = Fixture::new();
    let (server, banner, port) = start_server(&fixture, &[], &[]);
    assert!(
        banner.contains(&format!("http://127.0.0.1:{port}")),
        "banner: {banner}"
    );
    TcpStream::connect(("127.0.0.1", port)).expect("server accepts loopback connections");
    drop(server);
}

#[test]
fn ipv6_host_flag_is_bracketed_in_the_banner() {
    let _serial = serial();
    let fixture = Fixture::new();
    let Ok(listener) = TcpListener::bind("[::1]:0") else {
        return;
    };
    drop(listener);
    let (server, banner, port) = start_server(&fixture, &["--host", "::1"], &[]);
    assert!(
        banner.contains(&format!("http://[::1]:{port}")),
        "banner: {banner}"
    );
    drop(server);
}
