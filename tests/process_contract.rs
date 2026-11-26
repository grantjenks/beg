use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::fd::FromRawFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;

struct TestRun {
    root: TempDir,
    script: PathBuf,
}

impl TestRun {
    fn new(script: &str) -> Self {
        let root = tempfile::tempdir().unwrap();
        let script_path = root.path().join("script");
        fs::write(&script_path, format!("#!/usr/bin/env beg\n{script}")).unwrap();
        let fixture = root.path().join("codex-fixture");
        let binary = env::current_exe().unwrap();
        let quote = |text: &OsStr| format!("'{}'", text.to_str().unwrap().replace('\'', "'\\''"));
        fs::write(
            &fixture,
            format!(
                "#!/bin/sh\nBEG_TEST_FIXTURE=1 exec {} --exact fixture_app_server --ignored --nocapture 3>&1 > {} 2>&1\n",
                quote(binary.as_os_str()),
                quote(root.path().join("fixture.log").as_os_str()),
            ),
        )
        .unwrap();
        fs::set_permissions(fixture, fs::Permissions::from_mode(0o700)).unwrap();
        Self {
            root,
            script: script_path,
        }
    }

    fn interpreter(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_beg"));
        command
            .current_dir(self.root.path())
            .env("XDG_CACHE_HOME", self.root.path().join("cache"))
            .env("BEG_CODEX", self.root.path().join("codex-fixture"))
            .env("BEG_CODEX_HOME", self.root.path())
            .env_remove("BEG_DEBUG")
            .env_remove("BEG_TRACE")
            .env_remove("BEG_OPEN_TRACE")
            .env_remove("BEG_ENV")
            .env_remove("BEG_EXEC")
            .env_remove("CODEX_API_KEY")
            .env_remove("OPENAI_API_KEY")
            .env_remove("OPENAI_BASE_URL")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    fn command(&self) -> Command {
        let mut command = self.interpreter();
        command.args(["--timeout", "8s"]).arg(&self.script);
        command
    }

    fn check_code(&self, output: &Output, code: i32) {
        let backend = fs::read_to_string(self.root.path().join("fixture.log")).unwrap_or_default();
        assert_eq!(output.status.code(), Some(code), "{output:?}\n{backend}");
    }
}

fn run_input(command: &mut Command, bytes: &[u8]) -> Output {
    let mut process = command.spawn().unwrap();
    let mut input = process.stdin.take().unwrap();
    thread::scope(|scope| {
        scope.spawn(move || input.write_all(bytes).unwrap());
        process.wait_with_output().unwrap()
    })
}

fn run_with_input_open(command: &mut Command) -> Output {
    let mut process = command.spawn().unwrap();
    let input = process.stdin.take().unwrap();
    let output = process.wait_with_output().unwrap();
    drop(input);
    output
}

fn signal(process: &Child, signal: i32) {
    assert_eq!(unsafe { libc::kill(process.id() as i32, signal) }, 0);
}

fn ready(process: &mut Child) {
    let mut message = [0; 6];
    process
        .stdout
        .as_mut()
        .unwrap()
        .read_exact(&mut message)
        .unwrap();
    assert_eq!(&message, b"ready\n");
}

const SCRIPT_PATH_BYTES: &[u8] = if cfg!(target_os = "linux") {
    b"-script-\xff"
} else {
    b"-script"
};

const FILTER_SCRIPT: &str =
    "Copy bytes from stdin to stdout after EOF and report the argument on stderr.";

const FILTER_CODE: &str = r#"
function checkArgs(argv) {
  return argv.length < 3 && argv[1]?.length !== 0 ? null : { stderr: "usage\n", code: 2 };
}
async function run(argv, io) {
  const parts = [];
  for (let part = await io.read(); part.length; part = await io.read()) parts.push(part);
  for (const part of parts) await io.write("stdout", part);
  await io.write("stderr", argv[1] ?? new Uint8Array([0, 255]));
  return 7;
}
"#;

const REQUEST_SCRIPT: &str = "Respond to a request using the method and query string.";

const REQUEST_CODE: &str = r#"
async function checkArgs(argv, io) {
  if ((await io.env("BEG_TEST_REQUEST_CHECK"))?.text === "reject") {
    return { stderr: "Rejected\n", code: 2 };
  }
  return null;
}
async function run(argv, io) {
  const method = (await io.env("REQUEST_METHOD"))?.text;
  if (method !== "GET") {
    await io.write("stdout", "Status: 405 Method Not Allowed\r\nAllow: GET\r\n\r\n");
    return 0;
  }
  const query = argv.length === 1
    ? await io.env("QUERY_STRING")
    : (await io.env()).find((entry) => entry.name.text === "QUERY_STRING")?.value;
  await io.write("stdout", "Status: 200 OK\r\n\r\n");
  await io.write("stdout", query ? Uint8Array.fromBase64(query.base64) : "unset");
  return 0;
}
"#;

#[test]
#[ignore = "requires the matching codex-code-mode-host beside beg"]
fn reuses_a_prepared_filter_and_replays_exact_output() {
    let run = TestRun::new(FILTER_SCRIPT);
    let original = fs::read(&run.script).unwrap();
    let cache = run.root.path().join("cache");
    let first_trace = run.root.path().join("first.log");
    let data = b"hello\0\xff\n";
    let output = run_input(
        run.command()
            .env("XDG_CACHE_HOME", &cache)
            .env("BEG_TRACE", &first_trace),
        data,
    );
    run.check_code(&output, 7);
    assert_eq!(output.stdout, data);
    assert_eq!(output.stderr, [0, 255]);
    wait_for_trace(&first_trace, r#""action":"prepared_program""#);
    assert_eq!(fs::read(&run.script).unwrap(), original);

    let offline = |trace: &str, timeout: &str| {
        let mut command = run.interpreter();
        command
            .env("XDG_CACHE_HOME", &cache)
            .env("BEG_CODEX_HOME", run.root.path().join("absent-home"))
            .env("BEG_CODEX", run.root.path().join("absent-backend"))
            .env("BEG_TRACE", run.root.path().join(trace))
            .args(["--timeout", timeout])
            .arg(&run.script);
        command
    };

    let replay = run_input(&mut offline("replay.log", "8s"), data);
    run.check_code(&replay, 7);
    assert_eq!(replay.stdout, output.stdout);
    assert_eq!(replay.stderr, output.stderr);
    wait_for_trace(
        &run.root.path().join("replay.log"),
        r#""action":"replayed_result""#,
    );

    let changed = run_input(&mut offline("changed.log", "8s"), b"different");
    run.check_code(&changed, 7);
    assert_eq!(changed.stdout, b"different");
    wait_for_trace(
        &run.root.path().join("changed.log"),
        r#""action":"ran_program""#,
    );
    let binary_arg = OsString::from_vec(vec![0xfe, b'x']);
    let with_arg = run_input(offline("arg.log", "8s").arg(&binary_arg), data);
    run.check_code(&with_arg, 7);
    assert_eq!(with_arg.stderr, binary_arg.as_bytes());
    assert!(
        !fs::read_to_string(run.root.path().join("arg.log"))
            .unwrap()
            .contains("replayed_result")
    );

    let usage = run_with_input_open(offline("usage.log", "8s").arg(""));
    run.check_code(&usage, 2);
    assert_eq!(usage.stderr, b"usage\n");

    let waiting = run_with_input_open(&mut offline("deadline.log", "500ms"));
    run.check_code(&waiting, 124);

    let large: Vec<u8> = (0..=255).cycle().take(1024 * 1024 + 513).collect();
    let streamed = run_input(&mut offline("stream.log", "8s"), &large);
    run.check_code(&streamed, 7);
    assert_eq!(streamed.stdout, large);
    wait_for_trace(
        &run.root.path().join("stream.log"),
        r#""action":"streaming_input""#,
    );

    let mut broken = offline("pipe.log", "8s").spawn().unwrap();
    drop(broken.stdout.take());
    broken.stdin.take().unwrap().write_all(data).unwrap();
    assert_eq!(
        broken.wait_with_output().unwrap().status.signal(),
        Some(libc::SIGPIPE)
    );
}

#[test]
#[ignore = "requires the matching codex-code-mode-host beside beg"]
fn prepares_cgi_with_unrestricted_environment_and_replays_matching_requests() {
    let run = TestRun::new(REQUEST_SCRIPT);
    let first_trace = run.root.path().join("first.log");
    let get = b"Status: 200 OK\r\n\r\n";
    let output = run_with_input_open(
        run.command()
            .env("REQUEST_METHOD", "GET")
            .env("QUERY_STRING", "name=Ada")
            .env("REMOTE_PORT", "1")
            .env("BEG_TRACE", &first_trace),
    );
    run.check_code(&output, 0);
    assert_eq!(output.stdout, [get.as_slice(), b"name=Ada"].concat());
    assert!(output.stderr.is_empty());
    wait_for_trace(&first_trace, r#""action":"prepared_program""#);

    let offline = |trace: &str| {
        let mut command = run.command();
        command
            .env("BEG_CODEX_HOME", run.root.path().join("absent-home"))
            .env("BEG_CODEX", run.root.path().join("absent-backend"))
            .env("BEG_TRACE", run.root.path().join(trace))
            .env("REQUEST_METHOD", "GET")
            .env("QUERY_STRING", "name=Ada")
            .env("REMOTE_PORT", "2");
        command
    };
    let replay = run_with_input_open(&mut offline("replay.log"));
    run.check_code(&replay, 0);
    assert_eq!(replay.stdout, output.stdout);
    wait_for_trace(
        &run.root.path().join("replay.log"),
        r#""action":"replayed_result""#,
    );

    let changed = run_with_input_open(offline("changed.log").env("QUERY_STRING", "name=Grace"));
    run.check_code(&changed, 0);
    assert_eq!(changed.stdout, [get.as_slice(), b"name=Grace"].concat());
    wait_for_trace(
        &run.root.path().join("changed.log"),
        r#""action":"ran_program""#,
    );

    let post = run_with_input_open(offline("post.log").env("REQUEST_METHOD", "POST"));
    run.check_code(&post, 0);
    assert_eq!(
        post.stdout,
        b"Status: 405 Method Not Allowed\r\nAllow: GET\r\n\r\n"
    );

    let unset = run_with_input_open(offline("unset.log").env_remove("QUERY_STRING"));
    run.check_code(&unset, 0);
    assert_eq!(unset.stdout, [get.as_slice(), b"unset"].concat());
    let empty = run_with_input_open(offline("empty.log").env("QUERY_STRING", ""));
    run.check_code(&empty, 0);
    assert_eq!(empty.stdout, get);

    let binary = OsString::from_vec(vec![b'x', 0xff]);
    let all = run_with_input_open(offline("all.log").env("QUERY_STRING", &binary).arg("all"));
    run.check_code(&all, 0);
    assert_eq!(all.stdout, [get.as_slice(), binary.as_bytes()].concat());
    let all_changed = run_with_input_open(
        offline("all-changed.log")
            .env("QUERY_STRING", &binary)
            .env("REMOTE_PORT", "3")
            .arg("all"),
    );
    run.check_code(&all_changed, 0);
    assert_eq!(all_changed.stdout, all.stdout);
    wait_for_trace(
        &run.root.path().join("all-changed.log"),
        r#""action":"ran_program""#,
    );

    let rejected =
        run_with_input_open(offline("argument-check.log").env("BEG_TEST_REQUEST_CHECK", "reject"));
    run.check_code(&rejected, 2);
    assert_eq!(rejected.stderr, b"Rejected\n");
    assert!(rejected.stdout.is_empty());
}

#[test]
#[ignore = "requires the matching codex-code-mode-host beside beg"]
fn embeds_storage_and_runs_a_read_only_copy_without_codex() {
    let run = TestRun::new(FILTER_SCRIPT);
    fs::set_permissions(&run.script, fs::Permissions::from_mode(0o755)).unwrap();
    let original = fs::read_to_string(&run.script).unwrap();
    let output = run
        .interpreter()
        .env("XDG_CACHE_HOME", run.root.path().join("first-cache"))
        .arg("--embed")
        .arg(&run.script)
        .output()
        .unwrap();
    run.check_code(&output, 0);
    assert!(output.stdout.is_empty() && output.stderr.is_empty());
    let stored = fs::read_to_string(&run.script).unwrap();
    let yaml = stored
        .strip_prefix(&format!("{original}\n\n---\n"))
        .unwrap();
    let storage: serde_yaml::Value = serde_yaml::from_str(yaml).unwrap();
    assert_eq!(storage["format_version"].as_u64(), Some(1));
    assert_eq!(storage["program"]["code"].as_str(), Some(FILTER_CODE));
    assert_eq!(
        fs::metadata(&run.script).unwrap().permissions().mode() & 0o777,
        0o755
    );

    fs::set_permissions(&run.script, fs::Permissions::from_mode(0o555)).unwrap();
    let trace = run.root.path().join("embedded.log");
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_beg"));
    let path = env::join_paths(
        std::iter::once(binary.parent().unwrap().to_path_buf())
            .chain(env::split_paths(&env::var_os("PATH").unwrap())),
    )
    .unwrap();
    let mut command = Command::new(&run.script);
    command
        .env("PATH", path)
        .env("XDG_CACHE_HOME", run.root.path().join("second-cache"))
        .env("BEG_CODEX_HOME", run.root.path().join("absent-home"))
        .env("BEG_CODEX", run.root.path().join("absent-backend"))
        .env("BEG_TRACE", &trace)
        .env_remove("BEG_OPEN_TRACE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = run_input(&mut command, b"embedded");
    run.check_code(&output, 7);
    assert_eq!(output.stdout, b"embedded");
    wait_for_trace(&trace, r#""action":"embedded_program""#);
    assert_eq!(fs::read_to_string(&run.script).unwrap(), stored);

    fs::set_permissions(&run.script, fs::Permissions::from_mode(0o755)).unwrap();
    let mut legacy = storage;
    let program = legacy["program"].as_mapping_mut().unwrap();
    program.remove("stdin");
    program.insert("preparation_version".into(), 1.into());
    fs::write(
        &run.script,
        format!(
            "{original}\n\n---\n{}",
            serde_yaml::to_string(&legacy).unwrap()
        ),
    )
    .unwrap();
    let output = run_input(&mut command, b"legacy");
    run.check_code(&output, 7);
    assert_eq!(output.stdout, b"legacy");

    fs::write(&run.script, stored.replace("Copy bytes", "Copy the bytes")).unwrap();
    let output = run_input(&mut command, b"stale");
    run.check_code(&output, 64);
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("BEG_CODEX_HOME"));
}

fn wait_for_trace(path: &std::path::Path, message: &str) {
    let end = Instant::now() + Duration::from_secs(4);
    while Instant::now() < end {
        if fs::read_to_string(path).is_ok_and(|text| text.contains(message)) {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("trace did not contain {message:?}");
}

#[test]
fn cli_and_script_errors() {
    let run = TestRun::new("missing result");
    let output = run.interpreter().arg("--help").output().unwrap();
    assert!(output.status.success() && output.stderr.is_empty());
    assert!(output.stdout.starts_with(b"Run English scripts"));
    let version = run.interpreter().arg("--version").output().unwrap();
    assert!(version.status.success() && version.stderr.is_empty());
    assert!(version.stdout.starts_with(b"beg "));
    for args in [
        vec![],
        vec!["--unknown"],
        vec!["--timeout", "invalid", "script"],
    ] {
        let output = run.interpreter().args(args).output().unwrap();
        run.check_code(&output, 64);
        assert!(output.stdout.is_empty() && output.stderr.starts_with(b"beg: "));
    }
    let missing = run.interpreter().arg("absent").output().unwrap();
    run.check_code(&missing, 66);
    fs::write(&run.script, [0xff]).unwrap();
    let invalid_utf8 = run.command().output().unwrap();
    run.check_code(&invalid_utf8, 66);
    assert!(!run.root.path().join("fixture.log").exists());
}

#[test]
fn preserves_native_argv_environment_and_working_directory() {
    let run = TestRun::new("native values");
    let script_path = OsStr::from_bytes(SCRIPT_PATH_BYTES);
    fs::rename(&run.script, run.root.path().join(script_path)).unwrap();
    let output = run
        .interpreter()
        .args(["--timeout=8s", "--model", "test-model", "--"])
        .arg(script_path)
        .args(["two words", "", "--help", "*"])
        .arg(OsStr::from_bytes(b"arg-\xff"))
        .env("BEG_EXEC", "inherit")
        .env("BEG_ENV", "BEG_NATIVE")
        .env("BEG_NATIVE", OsString::from_vec(b"env-\xfe".to_vec()))
        .output()
        .unwrap();
    run.check_code(&output, 37);
    let mut expected = run
        .root
        .path()
        .canonicalize()
        .unwrap()
        .into_os_string()
        .into_vec();
    expected.extend_from_slice(b"\narg-\xff\0env-\xfe\0\n");
    assert_eq!(output.stdout, expected);
    assert_eq!(output.stderr, b"diagnostic");
}

#[test]
fn streams_binary_bytes_and_decodes_utf8_across_short_reads() {
    let run = TestRun::new("binary");
    let bytes = b"\xe2\x82\xac\0\xffZ";
    let output = run_input(&mut run.command(), bytes);
    run.check_code(&output, 0);
    assert_eq!(output.stdout, bytes);
    assert!(output.stderr.is_empty());
}

#[test]
fn returns_a_short_read_while_the_producer_is_still_open() {
    let run = TestRun::new("short read");
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_beg"));
    let has_runner = binary.with_file_name("codex-code-mode-host").is_file();
    for (name, action) in [
        ("first", "preparation_declined"),
        ("second", "cached_decline"),
    ] {
        let trace = run.root.path().join(name);
        let mut process = run.command().env("BEG_TRACE", &trace).spawn().unwrap();
        let mut input = process.stdin.take().unwrap();
        input.write_all(b"x").unwrap();
        ready(&mut process);
        drop(input);
        let output = process.wait_with_output().unwrap();
        run.check_code(&output, 0);
        assert!(output.stderr.is_empty());
        if has_runner {
            let trace = fs::read_to_string(trace).unwrap();
            assert!(
                trace.contains(&format!("\"action\":\"{action}\"")),
                "{trace}"
            );
            assert_eq!(
                trace.contains("\"action\":\"preparing_program\""),
                name == "first"
            );
        }
    }
}

#[test]
fn runs_directly_through_a_shebang() {
    let run = TestRun::new("binary");
    let trace = run.root.path().join("trace.jsonl");
    let opened = run.root.path().join("opened");
    let opener = run.root.path().join(if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    });
    fs::write(
        &opener,
        "#!/bin/sh\nprintf 'opener output\n'\nprintf 'opener diagnostic\n' >&2\nprintf '%s\n' \"$1\" > \"$BEG_TEST_OPENED\"\n",
    )
    .unwrap();
    fs::set_permissions(opener, fs::Permissions::from_mode(0o700)).unwrap();
    let prior_line = json!({
        "pid": 0, "elapsed_ms": 0, "event": "prior",
        "detail": "</script><script>unexpected markup</script>",
    });
    fs::write(&trace, format!("{prior_line}\n")).unwrap();
    fs::set_permissions(&run.script, fs::Permissions::from_mode(0o700)).unwrap();
    let bin = std::path::Path::new(env!("CARGO_BIN_EXE_beg"))
        .parent()
        .unwrap();
    let original_path = env::var_os("PATH").unwrap();
    let path = env::join_paths(
        [run.root.path().to_path_buf(), bin.to_path_buf()]
            .into_iter()
            .chain(env::split_paths(&original_path)),
    )
    .unwrap();
    let mut command = Command::new(&run.script);
    command
        .env("PATH", path)
        .env("XDG_CACHE_HOME", run.root.path().join("cache"))
        .env("BEG_CODEX", run.root.path().join("codex-fixture"))
        .env("BEG_CODEX_HOME", run.root.path())
        .env("BEG_DEBUG", "1")
        .env("BEG_TRACE", &trace)
        .env("BEG_OPEN_TRACE", "1")
        .env("BEG_TEST_OPENED", &opened)
        .env("TMPDIR", run.root.path())
        .env_remove("BEG_ENV")
        .env_remove("BEG_EXEC")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let bytes = b"\xe2\x82\xac\0\xffZ";
    let output = run_input(&mut command, bytes);
    run.check_code(&output, 0);
    assert_eq!(output.stdout, bytes);
    assert!(output.stderr.is_empty());
    let trace = fs::read_to_string(trace).unwrap();
    assert!(trace.contains(r#""method":"item/agentMessage/delta""#));
    wait_for_trace(&opened, ".html");
    let page = fs::read_to_string(opened).unwrap();
    let page = fs::read_to_string(page.trim()).unwrap();
    let (_, payload) = page
        .split_once(r#"<script type="application/json" id="initial-trace">"#)
        .unwrap();
    let data: Value = serde_json::from_str(payload.split_once("</script>").unwrap().0).unwrap();
    assert_eq!(data["text"].as_str(), Some(trace.as_str()));
    let finish: Value = serde_json::from_str(trace.lines().last().unwrap()).unwrap();
    assert_eq!(finish["event"], "finish");
    assert_eq!(finish["pid"], data["pid"]);
}

#[test]
fn stdin_is_exclusive_while_connected_to_a_child() {
    let run = TestRun::new("stdin lease");
    let mut process = run.command().env("BEG_EXEC", "inherit").spawn().unwrap();
    ready(&mut process);
    drop(process.stdin.take());
    let output = process.wait_with_output().unwrap();
    run.check_code(&output, 0);
    assert!(output.stderr.is_empty());
}

#[test]
fn child_streaming_does_not_send_input_to_the_agent() {
    let run = TestRun::new("transfer");
    let input = [b"pfx".as_slice(), &b"\0example-data\xff".repeat(512 * 1024)].concat();
    let trace = run.root.path().join("trace.jsonl");
    let output = run_input(
        run.interpreter()
            .args(["--timeout", "8s", "--trace"])
            .arg(&trace)
            .arg(&run.script)
            .env("BEG_EXEC", "inherit"),
        &input,
    );
    run.check_code(&output, 0);
    assert_eq!(output.stdout, input[3..]);
    assert!(output.stderr.is_empty());
    let trace = fs::read_to_string(trace).unwrap();
    assert_eq!(trace.matches("bytes_to_agent").count(), 1);
    assert!(trace.contains(r#""bytes_to_agent":3"#));
    assert!(trace.contains("script_sha256") && trace.contains("child_finished"));
    assert!(!trace.contains("example-data") && !trace.contains("transfer"));
}

#[test]
fn distinguishes_child_exit_and_signal() {
    let run = TestRun::new("child status");
    let output = run.command().env("BEG_EXEC", "inherit").output().unwrap();
    run.check_code(&output, 0);
    assert!(output.stdout.is_empty() && output.stderr.is_empty());
}

#[test]
fn interpreter_errors_keep_protocol_and_conversation_off_stdout() {
    for (scenario, code, diagnostic) in [
        ("missing result", 70, "without calling process_exit"),
        ("bad protocol", 70, "invalid protocol message"),
        ("unavailable", 69, "model service is unavailable"),
        ("missing Code Mode helper", 70, "codex-code-mode-host"),
    ] {
        let run = TestRun::new(scenario);
        let output = run.command().output().unwrap();
        run.check_code(&output, code);
        assert!(output.stdout.is_empty());
        let stderr = std::str::from_utf8(&output.stderr).unwrap();
        assert!(stderr.starts_with("beg: ") && stderr.lines().count() == 1);
        assert!(stderr.contains(diagnostic), "{stderr}");
    }
}

#[test]
fn debug_captures_backend_events_in_a_private_trace() {
    let run = TestRun::new("missing result");
    let output = run
        .interpreter()
        .env("TMPDIR", run.root.path())
        .args(["--timeout", "8s", "--debug"])
        .arg(&run.script)
        .output()
        .unwrap();
    run.check_code(&output, 70);
    assert!(output.stdout.is_empty());
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let path = stderr
        .lines()
        .next()
        .unwrap()
        .strip_prefix("beg: debug trace at ")
        .unwrap();
    assert!(stderr.contains("without calling process_exit"));
    assert_eq!(fs::metadata(path).unwrap().permissions().mode() & 0o077, 0);
    let trace = fs::read_to_string(path).unwrap();
    let events: Vec<Value> = trace
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(events.iter().any(|event| {
        event["event"] == "codex_event"
            && event["detail"]["method"] == "item/agentMessage/delta"
            && event["detail"]["params"]["delta"] == "Ignore assistant chatter"
    }));
    assert!(events.iter().any(|event| {
        event["event"] == "codex_event" && event["detail"]["method"] == "turn/completed"
    }));
}

#[test]
fn refuses_execution_and_environment_lookup_by_default() {
    let run = TestRun::new("policy");
    let output = run.command().output().unwrap();
    run.check_code(&output, 0);
    assert!(output.stdout.is_empty() && output.stderr.is_empty());
}

#[test]
fn preserves_configured_native_tools_and_instructions() {
    let run = TestRun::new("policy");
    fs::write(run.root.path().join("native-config"), "").unwrap();
    let output = run.command().output().unwrap();
    run.check_code(&output, 0);
    assert!(output.stdout.is_empty() && output.stderr.is_empty());
}

#[test]
fn preserves_sigpipe_when_the_reader_closes() {
    for scenario in ["write to pipe", "child pipe"] {
        let run = TestRun::new(scenario);
        let mut process = run.command().env("BEG_EXEC", "inherit").spawn().unwrap();
        drop(process.stdout.take());
        let output = process.wait_with_output().unwrap();
        assert_eq!(
            output.status.signal(),
            Some(libc::SIGPIPE),
            "{scenario}: {output:?}"
        );
        assert!(output.stderr.is_empty());
    }
}

#[test]
fn closes_stdout_immediately_when_requested() {
    let run = TestRun::new("close stdout");
    let mut process = run.command().spawn().unwrap();
    let mut bytes = Vec::new();
    process
        .stdout
        .take()
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    assert_eq!(bytes, b"EOF");
    assert!(process.try_wait().unwrap().is_none());
    signal(&process, libc::SIGTERM);
    assert_eq!(process.wait().unwrap().signal(), Some(libc::SIGTERM));
}

#[test]
fn handles_sigint_and_sigterm_with_stdin_held_open() {
    for (scenario, sig) in [
        ("blocked read", libc::SIGINT),
        ("blocked read", libc::SIGTERM),
        ("model wait", libc::SIGTERM),
    ] {
        let run = TestRun::new(scenario);
        let mut process = run.command().spawn().unwrap();
        ready(&mut process);
        let input = process.stdin.take().unwrap();
        signal(&process, sig);
        let output = process.wait_with_output().unwrap();
        assert_eq!(output.status.signal(), Some(sig), "{output:?}");
        assert!(output.stderr.is_empty());
        drop(input);
    }
}

#[test]
fn startup_signal_and_deadline_work_on_a_blocked_script_open() {
    let run = TestRun::new("unused");
    fs::remove_file(&run.script).unwrap();
    let path = std::ffi::CString::new(run.script.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
    let timed_out = run
        .interpreter()
        .args(["--timeout", "100ms"])
        .arg(&run.script)
        .output()
        .unwrap();
    run.check_code(&timed_out, 124);
    let mut process = run.command().spawn().unwrap();
    thread::sleep(Duration::from_millis(100));
    signal(&process, libc::SIGINT);
    assert_eq!(process.wait().unwrap().signal(), Some(libc::SIGINT));
}

#[test]
fn subscribed_signals_cancel_reads_and_allow_cleanup() {
    let run = TestRun::new("read cleanup");
    let trace = run.root.path().join("trace.jsonl");
    let mut process = run
        .interpreter()
        .args(["--timeout", "8s", "--trace"])
        .arg(&trace)
        .arg(&run.script)
        .spawn()
        .unwrap();
    ready(&mut process);
    let input = process.stdin.take().unwrap();
    wait_for_trace(&trace, r#""name":"stream_read""#);
    signal(&process, libc::SIGTERM);
    let output = process.wait_with_output().unwrap();
    run.check_code(&output, 23);
    assert_eq!(output.stderr, b"cleaned");
    drop(input);
}

#[test]
fn a_second_signal_forces_termination() {
    let run = TestRun::new("repeat signal");
    let mut process = run.command().spawn().unwrap();
    ready(&mut process);
    signal(&process, libc::SIGINT);
    let mut handling = [0; 8];
    process
        .stdout
        .as_mut()
        .unwrap()
        .read_exact(&mut handling)
        .unwrap();
    assert_eq!(&handling, b"handling");
    signal(&process, libc::SIGINT);
    assert_eq!(process.wait().unwrap().signal(), Some(libc::SIGINT));
}

#[test]
fn deadline_reaps_a_child_even_if_it_ignores_sigterm() {
    let run = TestRun::new("long child");
    let output = run
        .interpreter()
        .args(["--timeout", "5s"])
        .arg(&run.script)
        .env("BEG_EXEC", "inherit")
        .output()
        .unwrap();
    run.check_code(&output, 124);
    let child_pid: i32 = std::str::from_utf8(&output.stdout)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_eq!(unsafe { libc::kill(child_pid, 0) }, -1);
    assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
}

#[test]
fn deadline_keeps_working_with_a_full_output_pipe() {
    let run = TestRun::new("slow output");
    let trace = run.root.path().join("trace.jsonl");
    let mut process = run
        .interpreter()
        .args(["--timeout", "5s", "--trace"])
        .arg(&trace)
        .arg(&run.script)
        .spawn()
        .unwrap();
    let start = Instant::now();
    let status = process.wait().unwrap();
    assert_eq!(status.code(), Some(124));
    assert!(start.elapsed() < Duration::from_secs(8));
    wait_for_trace(&trace, "stream_written");
}

struct Fixture {
    input: BufReader<io::Stdin>,
    output: File,
    next_id: u64,
}

impl Fixture {
    fn send(&mut self, value: Value) {
        writeln!(self.output, "{value}").unwrap();
    }

    fn receive(&mut self) -> Option<Value> {
        let mut line = String::new();
        (self.input.read_line(&mut line).unwrap() != 0)
            .then(|| serde_json::from_str(&line).unwrap())
    }

    fn request(&mut self, method: &str) -> Value {
        let value = self.receive().unwrap();
        assert_eq!(value["method"], method);
        value
    }

    fn response(&mut self, request: &Value, result: Value) {
        self.send(json!({"id": request["id"], "result": result}));
    }

    fn send_tool(&mut self, tool: &str, args: Value, call_id: Option<&str>) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({
            "id": id, "method": "item/tool/call",
            "params": {"tool": tool, "arguments": args, "callId": call_id.map_or_else(|| format!("call-{id}"), str::to_owned),
                "threadId": "test-thread", "turnId": "test-turn"},
        }));
        id
    }

    fn tool_result(&mut self, id: u64) -> Option<(bool, Value)> {
        while let Some(value) = self.receive() {
            if value["method"] == "turn/steer" {
                self.response(&value, json!({"turnId": "test-turn"}));
                continue;
            }
            if value["method"] == "turn/interrupt" {
                self.response(&value, json!({}));
                return None;
            }
            assert_eq!(value["id"], id, "{value}");
            let result = &value["result"];
            let text = result["contentItems"][0]["text"].as_str().unwrap();
            return Some((
                result["success"].as_bool().unwrap(),
                serde_json::from_str(text).unwrap(),
            ));
        }
        None
    }

    fn call(&mut self, tool: &str, args: Value) -> Value {
        let id = self.send_tool(tool, args, None);
        let (success, body) = self.tool_result(id).unwrap();
        assert!(success, "{tool}: {body}");
        body
    }

    fn denied(&mut self, tool: &str, args: Value) -> Value {
        let id = self.send_tool(tool, args, None);
        let (success, body) = self.tool_result(id).unwrap();
        assert!(!success && body["error"].is_string(), "{tool}: {body}");
        body
    }

    fn write(&mut self, stream: &str, text: &str) {
        self.call("stream_write", json!({"stream": stream, "text": text}));
    }

    fn exit(&mut self, code: u8) {
        let id = self.send_tool("process_exit", json!({"code": code}), None);
        let _ = self.tool_result(id);
        if let Some(value) = self.receive() {
            assert_eq!(value["method"], "turn/interrupt", "{value}");
        }
    }
}

#[test]
#[ignore = "run by the integration tests as an app-server subprocess"]
fn fixture_app_server() {
    if env::var_os("BEG_TEST_FIXTURE").is_none() {
        return;
    }
    let mut rpc = Fixture {
        input: BufReader::new(io::stdin()),
        // The wrapper puts the private protocol pipe on fd 3.
        output: unsafe { File::from_raw_fd(3) },
        next_id: 100,
    };
    let init = rpc.request("initialize");
    assert_eq!(init["params"]["capabilities"]["experimentalApi"], true);
    rpc.response(
        &init,
        json!({"userAgent": "codex/0.171.0-alpha.13 fixture"}),
    );
    rpc.request("initialized");
    let config = rpc.request("config/read");
    let home = PathBuf::from(env::var_os("CODEX_HOME").unwrap());
    let native_config = home.join("native-config").exists();
    let settings = if native_config {
        json!({"mcp_servers": {"probe": {"enabled": true}}, "developer_instructions":"native instructions"})
    } else {
        json!({})
    };
    rpc.response(&config, json!({"config": settings}));
    let account = rpc.request("account/read");
    rpc.response(
        &account,
        json!({"account": {"type":"apiKey"}, "requiresOpenaiAuth": true}),
    );
    let start = rpc.request("thread/start");
    let params = &start["params"];
    assert_eq!(params["ephemeral"], true);
    assert_eq!(params["approvalPolicy"], "never");
    let preparing = params["dynamicTools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|tool| tool["name"] == "publish_filter");
    if preparing {
        assert_eq!(params["environments"], json!([]));
        assert_eq!(params["dynamicTools"].as_array().unwrap().len(), 1);
        assert!(
            params["developerInstructions"]
                .as_str()
                .unwrap()
                .contains("may replay")
        );
    } else {
        assert!(params.get("environments").is_none());
    }
    assert!(params.get("baseInstructions").is_none());
    assert!(params.get("sandbox").is_none());
    assert!(
        params["dynamicTools"]
            .as_array()
            .unwrap()
            .iter()
            .all(|tool| tool["description"]
                .as_str()
                .unwrap()
                .contains("result is a JSON string"))
    );
    if native_config && !preparing {
        let instructions = params["developerInstructions"].as_str().unwrap();
        assert!(instructions.starts_with("native instructions\n\n"));
        assert!(instructions.contains("process_exit"));
    }
    let has_spawn = params["dynamicTools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|tool| tool["name"] == "spawn");
    rpc.response(
        &start,
        json!({
            "thread": {"id":"test-thread"}, "model":"fixture",
            "approvalPolicy":"never", "sandbox":{"type":"readOnly"}, "instructionSources": [],
        }),
    );
    let turn = rpc.request("turn/start");
    let source = turn["params"]["input"][0]["text"].as_str().unwrap();
    assert!(!source.starts_with("#!"));
    rpc.response(&turn, json!({"turn": {"id":"test-turn"}}));
    if preparing {
        let prepared = match source.trim() {
            FILTER_SCRIPT => Some((FILTER_CODE, "eof")),
            REQUEST_SCRIPT => Some((REQUEST_CODE, "unused")),
            _ => None,
        };
        if let Some((code, stdin)) = prepared {
            assert_eq!(
                rpc.call("publish_filter", json!({"source": code, "stdin": stdin}))["received"],
                true
            );
        } else {
            rpc.send(json!({
                "method": "turn/completed",
                "params": {"threadId": "test-thread", "turn": {"id": "test-turn", "status": "completed"}},
            }));
        }
        return;
    }
    rpc.send(json!({
        "method": "item/agentMessage/delta",
        "params": {"threadId":"test-thread", "turnId":"test-turn", "delta":"Ignore assistant chatter"},
    }));
    match source {
        "native values" => {
            assert_eq!(params["model"], "test-model");
            let context = rpc.call("process_context", json!({}));
            let argv = context["argv"].as_array().unwrap();
            assert_eq!(argv.len(), 6);
            assert_eq!(
                STANDARD
                    .decode(argv[0]["base64"].as_str().unwrap())
                    .unwrap(),
                SCRIPT_PATH_BYTES
            );
            for (item, expected) in argv[1..5].iter().zip(["two words", "", "--help", "*"]) {
                assert_eq!(item["text"], expected);
            }
            assert!(argv[5]["text"].is_null());
            assert_eq!(context["cwd"]["handle"], "cwd");
            let value = rpc.call("env_get", json!({"name":"BEG_NATIVE"}));
            assert_eq!(value["value"]["handle"], "env:BEG_NATIVE");
            assert!(value["value"]["text"].is_null());
            let child = rpc.call("spawn", json!({"program":"/bin/pwd", "stdout":"stdout"}));
            assert_eq!(
                rpc.call("wait", json!({"child":child["child"]})),
                json!({"kind":"exit","code":0})
            );
            let child = rpc.call("spawn", json!({
                "program":"/usr/bin/printf", "stdout":"stdout",
                "args":["%s\\000%s\\000%s\\n", {"handle":"argv:5"}, {"handle":"env:BEG_NATIVE"}, {"handle":"argv:2"}],
            }));
            rpc.call("wait", json!({"child":child["child"]}));
            let id = rpc.send_tool(
                "stream_write",
                json!({"stream":"stderr","text":"diagnostic"}),
                Some("write-once"),
            );
            assert!(rpc.tool_result(id).unwrap().0);
            let id = rpc.send_tool(
                "stream_write",
                json!({"stream":"stderr","text":"duplicate"}),
                Some("write-once"),
            );
            assert!(!rpc.tool_result(id).unwrap().0);
            rpc.denied("process_exit", json!({"code":256}));
            rpc.exit(37);
        }
        "binary" => {
            for expected in ["", "", "€", "\0", "�", "Z"] {
                let read = rpc.call("stream_read", json!({"max_bytes":1}));
                assert_eq!(read["text"], expected);
                assert_eq!(read["text_lossy"], expected == "�");
                assert_eq!(read["byte_count"], 1);
                assert_eq!(read["eof"], false);
                rpc.call(
                    "stream_write",
                    json!({"stream":"stdout","base64":read["base64"]}),
                );
            }
            let eof = rpc.call("stream_read", json!({"max_bytes":1}));
            assert_eq!(eof["eof"], true);
            assert_eq!(eof["byte_count"], 0);
            rpc.call("stream_flush", json!({"stream":"stdout"}));
            rpc.exit(0);
        }
        "short read" => {
            let data = rpc.call("stream_read", json!({"max_bytes":65536}));
            assert_eq!(
                (data["byte_count"].as_u64(), data["eof"].as_bool()),
                (Some(1), Some(false))
            );
            rpc.write("stdout", "ready\n");
            assert_eq!(
                rpc.call("stream_read", json!({"max_bytes":4096}))["eof"],
                true
            );
            rpc.exit(0);
        }
        "transfer" => {
            let prefix = rpc.call("stream_read", json!({"max_bytes":3}));
            assert_eq!(prefix["text"], "pfx");
            let child = rpc.call(
                "spawn",
                json!({"program":"/bin/cat","stdin":"stdin","stdout":"stdout"}),
            );
            assert_eq!(
                rpc.call("wait", json!({"child":child["child"]})),
                json!({"kind":"exit","code":0})
            );
            rpc.exit(0);
        }
        "stdin lease" => {
            let child = rpc.call("spawn", json!({"program":"/bin/cat","stdin":"stdin"}));
            let read = rpc.denied("stream_read", json!({"max_bytes":1}));
            assert!(
                read["error"]
                    .as_str()
                    .unwrap()
                    .contains("connected to child")
            );
            rpc.denied("spawn", json!({"program":"/bin/cat","stdin":"stdin"}));
            rpc.denied("stream_close", json!({"stream":"stdin"}));
            rpc.write("stdout", "ready\n");
            assert_eq!(
                rpc.call("wait", json!({"child":child["child"]})),
                json!({"kind":"exit","code":0})
            );
            assert_eq!(rpc.call("stream_read", json!({"max_bytes":1}))["eof"], true);
            rpc.exit(0);
        }
        "child status" => {
            let child = rpc.call("spawn", json!({"program":"/bin/sh","args":["-c","exit 9"]}));
            let result = rpc.call("wait", json!({"child":child["child"]}));
            assert_eq!(result, json!({"kind":"exit","code":9}));
            assert_eq!(result, rpc.call("wait", json!({"child":child["child"]})));
            let child = rpc.call("spawn", json!({"program":"/bin/sleep","args":["30"]}));
            rpc.call(
                "signal_send",
                json!({"child":child["child"],"signal":"SIGUSR1"}),
            );
            assert_eq!(
                rpc.call("wait", json!({"child":child["child"]})),
                json!({"kind":"signal","signal":libc::SIGUSR1})
            );
            rpc.exit(0);
        }
        "policy" => {
            assert!(!has_spawn);
            rpc.send(json!({
                "id": "interactive",
                "method": "item/tool/requestUserInput",
                "params": {},
            }));
            assert_eq!(rpc.receive().unwrap()["error"]["code"], -32601);
            rpc.denied("spawn", json!({"program":"/bin/true"}));
            rpc.denied("env_get", json!({"name":"PATH"}));
            rpc.denied(
                "stream_write",
                json!({"stream":"stdin","text":"disallowed"}),
            );
            rpc.exit(0);
        }
        "missing result" | "unavailable" => {
            let error = if source == "unavailable" {
                json!({"codexErrorInfo":"unauthorized", "message":"backend detail stays private"})
            } else {
                Value::Null
            };
            rpc.send(json!({
                "method":"turn/completed",
                "params":{"threadId":"test-thread","turn":{"id":"test-turn","error":error}},
            }));
        }
        "bad protocol" => writeln!(rpc.output, "invalid JSON").unwrap(),
        "missing Code Mode helper" => rpc.send(json!({
            "method": "warning",
            "params": {
                "threadId": "test-thread",
                "message": "Code Mode is unavailable because failed to spawn code-mode host codex-code-mode-host: host executable was not found. Code mode will fail closed;",
            },
        })),
        "close stdout" => {
            rpc.write("stdout", "EOF");
            rpc.call("stream_close", json!({"stream":"stdout"}));
            let _ = rpc.receive();
        }
        "write to pipe" => {
            let id = rpc.send_tool(
                "stream_write",
                json!({"stream":"stdout","text":"bytes"}),
                None,
            );
            let _ = rpc.tool_result(id);
        }
        "child pipe" => {
            let child = rpc.call(
                "spawn",
                json!({"program":"/usr/bin/printf","args":["data"],"stdout":"stdout"}),
            );
            let id = rpc.send_tool("wait", json!({"child":child["child"]}), None);
            let _ = rpc.tool_result(id);
        }
        "blocked read" => {
            rpc.write("stdout", "ready\n");
            let id = rpc.send_tool("stream_read", json!({"max_bytes":50}), None);
            let _ = rpc.tool_result(id);
        }
        "model wait" => {
            rpc.write("stdout", "ready\n");
            let _ = rpc.receive();
        }
        "read cleanup" => {
            rpc.call("signal_subscribe", json!({"signals":["SIGTERM"]}));
            rpc.write("stdout", "ready\n");
            rpc.denied("stream_read", json!({"max_bytes":50}));
            assert_eq!(
                rpc.call("event_next", json!({})),
                json!({"event":{"kind":"signal","signal":"SIGTERM"}})
            );
            rpc.write("stderr", "cleaned");
            rpc.exit(23);
        }
        "repeat signal" => {
            rpc.call("signal_subscribe", json!({"signals":["SIGINT"]}));
            rpc.write("stdout", "ready\n");
            assert_eq!(
                rpc.call("event_next", json!({}))["event"]["signal"],
                "SIGINT"
            );
            rpc.write("stdout", "handling");
            let _ = rpc.receive();
        }
        "long child" => {
            let child = rpc.call("spawn", json!({
                "program":"/bin/sh","args":["-c","trap '' TERM; echo $$; /bin/sleep 30"], "stdout":"stdout",
            }));
            let id = rpc.send_tool("wait", json!({"child":child["child"]}), None);
            let _ = rpc.tool_result(id);
        }
        "slow output" => {
            rpc.write("stdout", "ready");
            let text = "a".repeat(16384);
            loop {
                let id =
                    rpc.send_tool("stream_write", json!({"stream":"stdout","text":text}), None);
                if rpc.tool_result(id).is_none() {
                    break;
                }
            }
        }
        _ => panic!("unknown fixture source {source:?}"),
    }
}
