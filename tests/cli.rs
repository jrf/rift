use std::fs;
use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use nix::pty::openpty;

static NEXT_TEST_ID: AtomicU64 = AtomicU64::new(1);

struct RiftTest {
    dir: PathBuf,
}

impl RiftTest {
    fn new() -> Self {
        let id = NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("rift-test-{}-{}", std::process::id(), id));
        fs::create_dir(&dir).expect("create isolated RIFT_DIR");
        Self { dir }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_rift"));
        command
            // Tests must not inherit rift's own session context or behavior
            // overrides from the shell running the suite. Individual tests can
            // opt back into a variable through `spawn_pty_env`.
            .env_remove("RIFT_SESSION")
            .env_remove("RIFT_SESSION_PREFIX")
            .env_remove("RIFT_TRACK_ENV")
            .env_remove("RIFT_NO_DETACH_KEY")
            .env_remove("RIFT_PICKER")
            .env_remove("RIFT_ON_ATTACH")
            .env_remove("RIFT_ON_DETACH")
            .env_remove("RIFT_ON_EXIT")
            .env_remove("RIFT_DIR_MODE")
            .env_remove("RIFT_LOG_MODE")
            .env("RIFT_DIR", &self.dir)
            .env("RIFT_SHELL", "/bin/sh")
            .env("RIFT_EMPTY_TIMEOUT", "30");
        command
    }

    fn output(&self, args: &[&str]) -> Output {
        self.command()
            .args(args)
            .output()
            .unwrap_or_else(|error| panic!("run rift {args:?}: {error}"))
    }

    fn spawn(&self, args: &[&str]) -> Child {
        self.command()
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|error| panic!("spawn rift {args:?}: {error}"))
    }

    fn spawn_pty(&self, args: &[&str]) -> (Child, File) {
        self.spawn_pty_env(args, &[])
    }

    fn spawn_pty_env(&self, args: &[&str], env: &[(&str, &str)]) -> (Child, File) {
        let pty = openpty(None, None).expect("open test PTY");
        let master = File::from(pty.master);
        let slave = File::from(pty.slave);
        let mut command = self.command();
        for (key, value) in env {
            command.env(key, value);
        }
        let child = command
            .args(args)
            .stdin(Stdio::from(
                slave.try_clone().expect("clone PTY slave for stdin"),
            ))
            .stdout(Stdio::from(
                slave.try_clone().expect("clone PTY slave for stdout"),
            ))
            .stderr(Stdio::from(slave))
            .spawn()
            .unwrap_or_else(|error| panic!("spawn PTY rift {args:?}: {error}"));
        (child, master)
    }

    fn wait_for_session(&self, name: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            let output = self.output(&["list", "--short"]);
            let sessions = String::from_utf8_lossy(&output.stdout);
            if sessions.lines().any(|session| session == name) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("session {name:?} did not appear");
    }
}

fn stop_pty_child(mut child: Child, _master: File) {
    assert!(
        child.try_wait().expect("poll PTY child").is_none(),
        "PTY client exited before the session could be inspected"
    );
    child.kill().expect("stop PTY client");
    child.wait().expect("reap PTY client");
}

impl Drop for RiftTest {
    fn drop(&mut self) {
        let output = self.output(&["list", "--short"]);
        for session in String::from_utf8_lossy(&output.stdout).lines() {
            let _ = self.output(&["kill", "--force", session]);
        }
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn send_frame(stream: &mut UnixStream, tag: u8, payload: &[u8]) {
    stream.write_all(&[tag]).expect("write frame tag");
    stream
        .write_all(&(payload.len() as u32).to_le_bytes())
        .expect("write frame length");
    stream.write_all(payload).expect("write frame payload");
}

fn read_frame(stream: &mut UnixStream) -> (u8, Vec<u8>) {
    fn read_exact_retry(stream: &mut UnixStream, mut buf: &mut [u8]) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !buf.is_empty() {
            match stream.read(buf) {
                Ok(0) => panic!("socket closed while reading frame"),
                Ok(n) => buf = &mut buf[n..],
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "timed out reading frame");
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("read frame: {error}"),
            }
        }
    }

    let mut header = [0; 5];
    read_exact_retry(stream, &mut header);
    let length = u32::from_le_bytes(header[1..].try_into().expect("frame length")) as usize;
    let mut payload = vec![0; length];
    read_exact_retry(stream, &mut payload);
    (header[0], payload)
}

fn resize_payload(rows: u16, cols: u16) -> [u8; 4] {
    let mut payload = [0; 4];
    payload[..2].copy_from_slice(&rows.to_le_bytes());
    payload[2..].copy_from_slice(&cols.to_le_bytes());
    payload
}

fn assert_socket_closed(stream: &mut UnixStream) {
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("set socket timeout");
    let mut byte = [0];
    assert_eq!(stream.read(&mut byte).expect("read socket EOF"), 0);
}

#[test]
fn malformed_clients_are_disconnected_without_stopping_daemon() {
    let test = RiftTest::new();
    assert!(test.output(&["new", "malformed-peer"]).status.success());
    test.wait_for_session("malformed-peer");
    let socket = test.dir.join("malformed-peer");

    let mut unknown = UnixStream::connect(&socket).expect("connect unknown-tag client");
    send_frame(&mut unknown, 13, &[]);
    assert_socket_closed(&mut unknown);

    let mut invalid_request = UnixStream::connect(&socket).expect("connect invalid-request client");
    send_frame(&mut invalid_request, 3, b"unexpected");
    assert_socket_closed(&mut invalid_request);

    let mut oversized = UnixStream::connect(&socket).expect("connect oversized-frame client");
    oversized.write_all(&[0]).expect("write oversized tag");
    oversized
        .write_all(&((16 * 1024 * 1024_u32) + 1).to_le_bytes())
        .expect("write oversized length");
    assert_socket_closed(&mut oversized);

    let listed = test.output(&["list", "--short"]);
    assert!(listed.status.success());
    assert_eq!(
        String::from_utf8_lossy(&listed.stdout).trim(),
        "malformed-peer"
    );
}

#[test]
fn rename_waits_for_daemon_result() {
    let test = RiftTest::new();
    assert!(test.output(&["new", "rename-source"]).status.success());
    test.wait_for_session("rename-source");

    let renamed = test.output(&["rename", "rename-source", "rename-success"]);
    assert!(
        renamed.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&renamed.stdout),
        String::from_utf8_lossy(&renamed.stderr)
    );
    test.wait_for_session("rename-success");

    let target = test.dir.join("rename-occupied");
    fs::write(&target, b"sentinel").expect("create occupied rename target");
    let rejected = test.output(&["rename", "rename-success", "rename-occupied"]);
    assert_eq!(
        rejected.status.code(),
        Some(1),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&rejected.stdout),
        String::from_utf8_lossy(&rejected.stderr)
    );
    assert!(
        String::from_utf8_lossy(&rejected.stderr).contains("target socket path already exists"),
        "{}",
        String::from_utf8_lossy(&rejected.stderr)
    );
    assert_eq!(fs::read(target).expect("read occupied target"), b"sentinel");
    test.wait_for_session("rename-success");
}

#[test]
fn control_clients_receive_output_only_after_subscribing() {
    let test = RiftTest::new();
    assert!(
        test.output(&["new", "output-subscription"])
            .status
            .success()
    );
    test.wait_for_session("output-subscription");

    let socket = test.dir.join("output-subscription");
    let mut control = UnixStream::connect(&socket).expect("connect control client");
    control
        .set_read_timeout(Some(Duration::from_millis(200)))
        .expect("set control client read timeout");

    assert!(
        test.output(&["print", "output-subscription", "before"])
            .status
            .success()
    );
    let mut byte = [0];
    let error = control
        .read(&mut byte)
        .expect_err("control client must not receive PTY output");
    assert!(
        matches!(
            error.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        ),
        "unexpected read error: {error}"
    );

    control
        .set_read_timeout(None)
        .expect("clear control client read timeout");
    send_frame(&mut control, 12, &[]);
    assert!(
        test.output(&["print", "output-subscription", "after"])
            .status
            .success()
    );
    let (tag, payload) = read_frame(&mut control);
    assert_eq!(tag, 1);
    assert_eq!(payload, b"after");
}

#[test]
fn info_counts_only_initialized_terminal_clients() {
    let test = RiftTest::new();
    let create = test.output(&["new", "client-count"]);
    assert!(create.status.success());
    test.wait_for_session("client-count");

    let socket = test.dir.join("client-count");
    let mut terminal = UnixStream::connect(&socket).expect("connect terminal");
    send_frame(&mut terminal, 7, &resize_payload(24, 80));
    std::thread::sleep(Duration::from_millis(50));

    let listed = test.output(&["list"]);
    let output = String::from_utf8_lossy(&listed.stdout);
    assert!(output.contains("clients=1"), "{output}");
}

#[test]
fn kill_then_immediate_recreate_same_name_succeeds() {
    let test = RiftTest::new();
    for _ in 0..3 {
        assert!(
            test.output(&["new", "race", "sleep", "30"])
                .status
                .success()
        );
        test.wait_for_session("race");
        assert!(test.output(&["kill", "race"]).status.success());
        let recreated = test.output(&["new", "race", "sleep", "30"]);
        assert!(
            recreated.status.success(),
            "{}",
            String::from_utf8_lossy(&recreated.stderr)
        );
        test.wait_for_session("race");
        assert!(test.output(&["kill", "race"]).status.success());
    }
}

#[test]
fn grouped_labels_are_accepted() {
    let test = RiftTest::new();
    assert!(test.output(&["new", "grouped-labels"]).status.success());
    test.wait_for_session("grouped-labels");
    assert!(
        test.output(&["set", "grouped-labels", "a=1 b=2"])
            .status
            .success()
    );
    let labels = test.output(&["get", "grouped-labels"]);
    let output = String::from_utf8_lossy(&labels.stdout);
    assert!(output.contains("a=1"), "{output}");
    assert!(output.contains("b=2"), "{output}");
}

#[test]
fn run_executes_piped_multiline_script_with_heredoc() {
    let test = RiftTest::new();
    let mut child = test
        .command()
        .args(["run", "stdin-script"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn piped run");

    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(
            b"printf 'line-one\\n'\ncat <<'EOF'\nliteral-$USER-$(whoami)\nEOF\nprintf 'line-three\\n'\n",
        )
        .expect("write script");

    let output = child.wait_with_output().expect("wait for piped run");
    assert!(
        output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("line-one"), "{stdout}");
    assert!(stdout.contains("literal-$USER-$(whoami)"), "{stdout}");
    assert!(stdout.contains("line-three"), "{stdout}");
}

#[test]
fn wait_reports_failed_task_history() {
    let test = RiftTest::new();
    let run = test.output(&[
        "run",
        "--detached",
        "failed-task",
        "sh",
        "-c",
        "printf 'wait-failure-marker\\n'; exit 7",
    ]);
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );

    let wait = test.output(&["wait", "failed-task"]);
    assert_eq!(wait.status.code(), Some(7));
    let stderr = String::from_utf8_lossy(&wait.stderr);
    assert!(stderr.contains("tasks failed!"), "{stderr}");
    assert!(
        stderr.contains("failed task=failed-task exit_status=7"),
        "{stderr}"
    );
    assert!(stderr.contains("wait-failure-marker"), "{stderr}");
}

#[test]
fn wait_fails_when_a_running_session_disappears() {
    let test = RiftTest::new();
    let run = test.output(&["run", "--detached", "disappearing-task", "sleep", "30"]);
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    test.wait_for_session("disappearing-task");

    let wait = test.spawn(&["wait", "disappearing-task"]);
    std::thread::sleep(Duration::from_millis(1200));

    let kill = test.output(&["kill", "--force", "disappearing-task"]);
    assert!(
        kill.status.success(),
        "{}",
        String::from_utf8_lossy(&kill.stderr)
    );

    let output = wait.wait_with_output().expect("wait for rift wait");
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("session(s) disappeared before completing"),
        "{stderr}"
    );
}

#[test]
fn keyboard_input_transfers_resize_ownership_between_clients() {
    let test = RiftTest::new();
    let create = test.output(&["new", "leadership"]);
    assert!(
        create.status.success(),
        "{}",
        String::from_utf8_lossy(&create.stderr)
    );
    test.wait_for_session("leadership");

    let socket = test.dir.join("leadership");
    let mut first = UnixStream::connect(&socket).expect("connect first interactive client");
    let mut second = UnixStream::connect(&socket).expect("connect second interactive client");

    send_frame(&mut first, 7, &resize_payload(20, 80));
    send_frame(&mut second, 7, &resize_payload(40, 100));
    std::thread::sleep(Duration::from_millis(100));

    let before = test.output(&["run", "leadership", "stty", "size"]);
    assert!(
        String::from_utf8_lossy(&before.stdout).contains("20 80"),
        "{}",
        String::from_utf8_lossy(&before.stdout)
    );

    send_frame(&mut second, 0, b"\r");
    // Leadership transfer requests a fresh size; report the second terminal's
    // current dimensions as the real client does.
    send_frame(&mut second, 2, &resize_payload(40, 100));
    std::thread::sleep(Duration::from_millis(100));

    let after = test.output(&["run", "leadership", "stty", "size"]);
    assert!(
        String::from_utf8_lossy(&after.stdout).contains("40 100"),
        "{}",
        String::from_utf8_lossy(&after.stdout)
    );
}

#[test]
fn reattach_restores_active_alternate_screen_mode() {
    let test = RiftTest::new();
    let create = test.output(&[
        "new",
        "alternate-screen",
        "sh",
        "-c",
        "printf '\\033[?1049h\\033[2J\\033[3;10HALT_MARK'; sleep 30",
    ]);
    assert!(
        create.status.success(),
        "{}",
        String::from_utf8_lossy(&create.stderr)
    );
    test.wait_for_session("alternate-screen");

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let history = test.output(&["history", "--vt", "alternate-screen"]);
        if history
            .stdout
            .windows(8)
            .any(|window| window == b"ALT_MARK")
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    let socket = test.dir.join("alternate-screen");
    // First attachment establishes that subsequent terminal clients are
    // reattachments. Initial clients receive live output rather than replay.
    let mut initial = UnixStream::connect(&socket).expect("connect initial client");
    send_frame(&mut initial, 7, &resize_payload(24, 80));
    drop(initial);
    std::thread::sleep(Duration::from_millis(200));

    let mut client = UnixStream::connect(&socket).expect("connect reattaching client");
    client
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set read timeout");
    send_frame(&mut client, 7, &resize_payload(24, 80));
    let (mut tag, mut payload) = read_frame(&mut client);
    // Leadership establishment also asks the client for a fresh size.
    if tag == 2 && payload.is_empty() {
        (tag, payload) = read_frame(&mut client);
    }

    assert_eq!(tag, 7, "expected Init frame");
    assert!(
        payload
            .windows(b"\x1b[?1049h".len())
            .any(|window| window == b"\x1b[?1049h"),
        "Init frame did not enter alternate screen"
    );
    assert!(
        payload
            .windows(b"ALT_MARK".len())
            .any(|window| window == b"ALT_MARK"),
        "Init frame did not contain alternate-screen contents"
    );
}

#[test]
fn detached_custom_command_resolves_from_path_without_arguments() {
    let test = RiftTest::new();
    let create = test.output(&["new", "path-command", "cat"]);
    assert!(
        create.status.success(),
        "{}",
        String::from_utf8_lossy(&create.stderr)
    );

    test.wait_for_session("path-command");
}

#[test]
fn smart_commands_use_executable_basenames_and_allocate_suffixes() {
    let test = RiftTest::new();

    let (first, first_pty) = test.spawn_pty(&["--new", "cat"]);
    test.wait_for_session("cat");
    stop_pty_child(first, first_pty);

    let (second, second_pty) = test.spawn_pty(&["--new", "cat"]);
    test.wait_for_session("cat.1");
    stop_pty_child(second, second_pty);

    let sessions = String::from_utf8_lossy(&test.output(&["list", "--short"]).stdout).into_owned();
    assert_eq!(sessions, "cat\ncat.1\n");
}

#[test]
fn smart_bare_name_prefers_an_existing_session_even_with_arguments() {
    let test = RiftTest::new();
    assert!(test.output(&["new", "not-a-real-command"]).status.success());
    test.wait_for_session("not-a-real-command");

    let (child, pty) = test.spawn_pty(&["not-a-real-command", "ignored-argument"]);
    stop_pty_child(child, pty);

    let sessions = String::from_utf8_lossy(&test.output(&["list", "--short"]).stdout).into_owned();
    assert_eq!(sessions, "not-a-real-command\n");
}

#[test]
fn smart_command_forwards_arguments_to_the_pty_process() {
    let test = RiftTest::new();
    let (child, pty) = test.spawn_pty(&["--new", "sleep", "30"]);
    test.wait_for_session("sleep");

    let listed = String::from_utf8_lossy(&test.output(&["list"]).stdout).into_owned();
    assert!(listed.contains("name=sleep\t"), "{listed}");
    assert!(listed.contains("sleep 30"), "{listed}");
    stop_pty_child(child, pty);
}

#[test]
fn forced_smart_command_rejects_an_unknown_executable() {
    let test = RiftTest::new();
    let output = test.output(&["--new", "rift-test-command-that-does-not-exist"]);

    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("command not found in PATH"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn labels_support_lifecycle_and_list_filtering() {
    let test = RiftTest::new();
    let create = test.output(&["new", "labeled"]);
    assert!(create.status.success());
    test.wait_for_session("labeled");

    let set = test.output(&["set", "labeled", "project=rift", "env=dev"]);
    assert!(
        set.status.success(),
        "{}",
        String::from_utf8_lossy(&set.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&test.output(&["get", "labeled"]).stdout),
        "env=dev project=rift"
    );
    assert_eq!(
        String::from_utf8_lossy(&test.output(&["get", "labeled", "project"]).stdout),
        "rift"
    );

    let matching = test.output(&["list", "--short", "--where", "project=rift"]);
    assert_eq!(String::from_utf8_lossy(&matching.stdout), "labeled\n");
    let not_matching = test.output(&["list", "--short", "--where", "project=other"]);
    assert!(not_matching.stdout.is_empty());

    let listed = String::from_utf8_lossy(&test.output(&["list"]).stdout).into_owned();
    assert!(listed.contains("\tenv=dev\tproject=rift"), "{listed}");

    assert!(test.output(&["unset", "labeled", "env"]).status.success());
    assert_eq!(
        String::from_utf8_lossy(&test.output(&["get", "labeled"]).stdout),
        "project=rift"
    );
    assert!(test.output(&["clear", "labeled"]).status.success());
    assert!(test.output(&["get", "labeled"]).stdout.is_empty());
}

#[test]
fn invalid_or_reserved_labels_are_rejected_without_mutation() {
    let test = RiftTest::new();
    assert!(test.output(&["new", "label-validation"]).status.success());
    test.wait_for_session("label-validation");

    let invalid = test.output(&["set", "label-validation", "bad=value/with/slash"]);
    assert_eq!(invalid.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("label value may only contain"));

    let reserved = test.output(&["set", "label-validation", "name=other"]);
    assert_eq!(reserved.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&reserved.stderr).contains("read-only built-in field"));
    assert!(test.output(&["get", "label-validation"]).stdout.is_empty());
}

#[test]
fn attach_labels_flag_sets_labels_at_creation() {
    let test = RiftTest::new();

    // `new --labels` (detached create) applies labels atomically.
    let create = test.output(&["new", "--labels", "project=rift env=prod", "prelabeled"]);
    assert!(
        create.status.success(),
        "{}",
        String::from_utf8_lossy(&create.stderr)
    );
    test.wait_for_session("prelabeled");
    assert_eq!(
        String::from_utf8_lossy(&test.output(&["get", "prelabeled"]).stdout),
        "env=prod project=rift"
    );

    // The `--labels=<value>` form is also accepted.
    let create_eq = test.output(&["new", "--labels=team=core", "prelabeled2"]);
    assert!(create_eq.status.success());
    test.wait_for_session("prelabeled2");
    assert_eq!(
        String::from_utf8_lossy(&test.output(&["get", "prelabeled2"]).stdout),
        "team=core"
    );

    // An invalid pair is rejected before the session is created.
    let invalid = test.output(&["new", "--labels", "bad key", "should-not-exist"]);
    assert_eq!(invalid.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("key=value"));
    let listed = String::from_utf8_lossy(&test.output(&["list", "--short"]).stdout).into_owned();
    assert!(!listed.contains("should-not-exist"), "{listed}");

    // A missing --labels value is a clear error (flag before name, no value).
    let missing = test.output(&["new", "--labels"]);
    assert_eq!(missing.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&missing.stderr).contains("--labels requires a value"));
}

#[test]
fn print_env_reports_attached_clients_tracked_environment() {
    use std::io::Read;

    let test = RiftTest::new();

    // Attach an interactive PTY client with a couple of tracked vars set and
    // one deliberately unset. Use RIFT_TRACK_ENV to keep the set small and
    // deterministic regardless of the ambient environment.
    let (child, mut master) = test.spawn_pty_env(
        &["attach", "envsess"],
        &[
            ("RIFT_TRACK_ENV", "DISPLAY,SSH_AUTH_SOCK,RIFT_ENV_ABSENT"),
            ("DISPLAY", ":7"),
            ("SSH_AUTH_SOCK", "/tmp/ssh-test"),
            // RIFT_ENV_ABSENT intentionally left unset.
        ],
    );
    test.wait_for_session("envsess");

    // Drain PTY output on a background thread so the client never stalls on
    // backpressure before it sends its env snapshot.
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop_reader = stop.clone();
    let drain = std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        while !stop_reader.load(Ordering::Relaxed) {
            match master.read(&mut buf) {
                Ok(0) => break,
                Ok(_) => {}
                Err(_) => break,
            }
        }
        master
    });
    // Give the client a moment to send its EnvSet frame.
    std::thread::sleep(Duration::from_millis(500));

    let all = test.output(&["print-env", "envsess"]);
    let text = String::from_utf8_lossy(&all.stdout);
    assert!(text.contains("DISPLAY=:7"), "{text}");
    assert!(text.contains("SSH_AUTH_SOCK=/tmp/ssh-test"), "{text}");
    assert!(text.contains("-RIFT_ENV_ABSENT"), "{text}");

    // Single-key lookup of a set variable.
    let one = test.output(&["print-env", "envsess", "DISPLAY"]);
    assert_eq!(String::from_utf8_lossy(&one.stdout).trim(), ":7");

    // Single-key lookup of an unset variable fails.
    let missing = test.output(&["print-env", "envsess", "RIFT_ENV_ABSENT"]);
    assert_eq!(missing.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&missing.stderr).contains("not found"));

    // Shell mode emits export/unset statements.
    let shell = test.output(&["print-env", "-s", "envsess"]);
    let shell_text = String::from_utf8_lossy(&shell.stdout);
    assert!(shell_text.contains("export DISPLAY=':7';"), "{shell_text}");
    assert!(
        shell_text.contains("unset RIFT_ENV_ABSENT;"),
        "{shell_text}"
    );

    // Stop the client; killing it closes the PTY slave and unblocks the
    // drain thread's blocking read with EOF.
    stop.store(true, Ordering::Relaxed);
    let _ = test.output(&["kill", "--force", "envsess"]);
    let mut child = child;
    let _ = child.kill();
    let _ = child.wait();
    let _master = drain.join().expect("join PTY drain");
}

#[test]
fn switch_request_relays_to_leader_client_with_cwd() {
    let test = RiftTest::new();
    let create = test.output(&["new", "switch-src"]);
    assert!(
        create.status.success(),
        "{}",
        String::from_utf8_lossy(&create.stderr)
    );
    test.wait_for_session("switch-src");

    let socket = test.dir.join("switch-src");

    // Leader client: an interactive client that claims resize leadership.
    let mut leader = UnixStream::connect(&socket).expect("connect leader client");
    leader
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set read timeout");
    send_frame(&mut leader, 7, &resize_payload(24, 80)); // Tag::Init == 7
    std::thread::sleep(Duration::from_millis(100));

    // A separate transient client asks the daemon to switch to "switch-dst".
    let mut requester = UnixStream::connect(&socket).expect("connect switch requester");
    send_frame(&mut requester, 11, b"switch-dst"); // Tag::Switch == 11

    // The leader must receive a Switch frame carrying "switch-dst\n<cwd>".
    // Other frames (Init, Output) may arrive first; scan past them.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut got = None;
    while Instant::now() < deadline {
        let (tag, payload) = read_frame(&mut leader);
        if tag == 11 {
            got = Some(payload);
            break;
        }
    }
    let payload = got.expect("leader did not receive Switch frame");
    let text = String::from_utf8_lossy(&payload);
    let (name, cwd) = text.split_once('\n').expect("payload is name\\ncwd");
    assert_eq!(name, "switch-dst");
    assert!(
        cwd.starts_with('/'),
        "cwd should be an absolute path, got {cwd:?}"
    );
}

#[test]
fn subcommand_help_does_not_create_sessions() {
    let test = RiftTest::new();

    for command in ["attach", "run", "send", "kill", "history", "--new"] {
        let output = test.output(&[command, "--help"]);
        assert!(
            output.status.success(),
            "rift {command} --help failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("Usage:"),
            "rift {command} --help did not print help"
        );
    }

    assert!(test.output(&["list", "--short"]).stdout.is_empty());
}

#[test]
fn detached_run_uses_sane_size_without_a_terminal() {
    let test = RiftTest::new();
    let run = test.output(&[
        "run",
        "--detached",
        "headless-size",
        "sh",
        "-c",
        "stty size",
    ]);
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );

    let wait = test.output(&["wait", "headless-size"]);
    assert!(
        wait.status.success(),
        "{}",
        String::from_utf8_lossy(&wait.stderr)
    );
    let history = test.output(&["history", "headless-size"]);
    assert!(
        String::from_utf8_lossy(&history.stdout).contains("24 120"),
        "{}",
        String::from_utf8_lossy(&history.stdout)
    );
}

#[test]
fn empty_write_creates_an_empty_file() {
    let test = RiftTest::new();
    assert!(test.output(&["new", "empty-write"]).status.success());
    test.wait_for_session("empty-write");
    let path = test.dir.join("empty.txt");
    let mut child = test
        .command()
        .args(["write", "empty-write", path.to_str().unwrap()])
        .stdin(Stdio::piped())
        .spawn()
        .expect("spawn empty write");
    drop(child.stdin.take());
    assert!(child.wait().expect("wait empty write").success());
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && !path.exists() {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(fs::read(path).expect("read empty file"), b"");
}

#[test]
fn print_injects_exact_bytes_without_a_newline() {
    let test = RiftTest::new();
    assert!(test.output(&["new", "exact-print"]).status.success());
    test.wait_for_session("exact-print");
    assert!(
        test.output(&["print", "exact-print", "EXACT_MARK"])
            .status
            .success()
    );
    std::thread::sleep(Duration::from_millis(50));
    let history = test.output(&["history", "--vt", "exact-print"]);
    assert!(history.stdout.windows(10).any(|w| w == b"EXACT_MARK"));
}

#[test]
fn dumb_term_is_replaced_for_session_commands() {
    let test = RiftTest::new();
    let output = test
        .command()
        .env("TERM", "dumb")
        .args([
            "run",
            "--detached",
            "term-fallback",
            "sh",
            "-c",
            "printf 'TERM=%s\\n' \"$TERM\"",
        ])
        .output()
        .expect("run TERM fallback command");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let wait = test.output(&["wait", "term-fallback"]);
    assert!(
        wait.status.success(),
        "{}",
        String::from_utf8_lossy(&wait.stderr)
    );
    let history = test.output(&["history", "term-fallback"]);
    assert!(
        String::from_utf8_lossy(&history.stdout).contains("TERM=xterm-256color"),
        "{}",
        String::from_utf8_lossy(&history.stdout)
    );
}
