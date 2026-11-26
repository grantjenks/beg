use serde_json::{Value, json};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::fd::AsRawFd;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

const NATIVE_LOCK_FILE: &str = "native-command.lock";
const NATIVE_READY: &str = "beg-native-command-ready";

fn beg(cwd: &Path) -> Command {
    let home = cwd.join("codex-home");
    fs::create_dir_all(&home).unwrap();
    fs::write(
        home.join("config.toml"),
        "cli_auth_credentials_store = 'file'\n",
    )
    .unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_beg"));
    command
        .current_dir(cwd)
        .env("XDG_CACHE_HOME", cwd.join("cache"))
        .env("PATH", cwd)
        .env_remove("BEG_CODEX")
        .env("BEG_CODEX_HOME", home)
        .env_remove("BEG_DEBUG")
        .env_remove("BEG_TRACE")
        .env_remove("BEG_OPEN_TRACE")
        .env_remove("CODEX_EXEC_SERVER_URL")
        .env_remove("BEG_EXEC")
        .env_remove("CODEX_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .env_remove("OPENAI_BASE_URL")
        .args(["--timeout", "60s"]);
    command
}

#[test]
fn default_backend_starts_without_a_codex_executable() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("script"), "Exit 0.").unwrap();
    let output = beg(root.path())
        .args(["--trace", "trace.jsonl", "script"])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(69), "{output:?}");
    assert!(output.stdout.is_empty());
    let diagnostic = std::str::from_utf8(&output.stderr).unwrap();
    assert!(diagnostic.contains("Codex needs authentication"));
    let trace = fs::read_to_string(root.path().join("trace.jsonl")).unwrap();
    assert!(trace.contains(r#""backend":"codex-embedded""#));
}

#[test]
#[ignore = "requires a localhost listener for the synthetic Responses endpoint"]
fn embedded_model_uses_native_shell_and_beg_output_tools() {
    let root = tempfile::tempdir().unwrap();
    let script = "Write hello to stdout, then exit 7.";
    fs::write(root.path().join("script"), script).unwrap();
    fs::write(root.path().join("native-input"), "captured native output").unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
    let model = thread::spawn(move || {
        let (stream, request) = read_live_request(&listener);
        let input = request["input"].to_string();
        assert!(input.contains(script));
        assert!(input.contains("JSON.parse(await tools.process_context({}))"));
        let tools = request["tools"].as_array().unwrap();
        assert!(tools.iter().any(|tool| tool["name"] == "stream_write"));
        let (tool, args) = shell(&request, "/bin/cat native-input");
        send_call(stream, &request, 1, tool, args);

        let (mut stream, _) = listener.accept().unwrap();
        let request = read_request(&mut stream);
        let output = tool_output(&request, "call_1");
        assert!(output.contains("captured native output"), "{output}");
        send_call(
            stream,
            &request,
            2,
            "stream_write",
            json!({"stream": "stdout", "text": "hello"}),
        );

        let (mut stream, _) = listener.accept().unwrap();
        let request = read_request(&mut stream);
        assert!(request["input"].to_string().contains("written"));
        send_call(stream, &request, 3, "process_exit", json!({"code": 7}));
    });

    let output = beg(root.path())
        .env("PATH", "/usr/bin:/bin")
        .env("OPENAI_API_KEY", "beg-local-test-key")
        .env("OPENAI_BASE_URL", endpoint)
        .env("NO_PROXY", "127.0.0.1")
        .args([
            "--model",
            "gpt-5.4",
            "--debug",
            "--trace",
            "debug.jsonl",
            "script",
        ])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(7), "{output:?}");
    assert_eq!(output.stdout, b"hello");
    assert!(output.stderr.is_empty());
    model.join().unwrap();
    let trace = fs::read_to_string(root.path().join("debug.jsonl")).unwrap();
    let events: Vec<Value> = trace
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(events.iter().any(|event| {
        event["event"] == "codex_event"
            && event["detail"]["method"] == "item/completed"
            && event["detail"]["params"]["item"]["type"] == "commandExecution"
            && event["detail"]["params"]["item"]["aggregatedOutput"] == "captured native output"
    }));
    assert!(events.iter().any(|event| {
        event["event"] == "codex_event"
            && event["detail"]["method"] == "rawResponseItem/completed"
            && event["detail"]["params"]["item"]["type"] == "function_call"
            && event["detail"]["params"]["item"]["call_id"] == "call_1"
    }));
}

#[test]
#[ignore = "requires a localhost listener and the matching codex-code-mode-host"]
fn embedded_codex_prepares_a_filter_before_receiving_input() {
    let root = tempfile::tempdir().unwrap();
    let description = "Count stdin bytes and print the total.";
    fs::write(root.path().join("script"), description).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
    let model = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let request = read_request(&mut stream);
        let input = request["input"].to_string();
        assert!(input.contains("Count stdin bytes"));
        assert!(input.contains("may replay the exact stdout"));
        assert!(!input.contains("stream data only"));
        let tools = request["tools"].as_array().unwrap();
        assert!(tools.iter().any(|tool| tool["name"] == "publish_filter"));
        assert!(!tools.iter().any(|tool| {
            matches!(
                tool["name"].as_str(),
                Some("stream_read" | "spawn" | "exec_command")
            )
        }));
        send_call(
            stream,
            &request,
            1,
            "publish_filter",
            json!({"stdin": "eof", "source": r#"
function checkArgs(argv) { return null; }
async function run(argv, io) {
  let size = 0;
  for (let part = await io.read(); part.length; part = await io.read()) size += part.length;
  await io.write("stdout", String(size) + "\n");
  return 0;
}
"#}),
        );
    });
    let mut process = beg(root.path())
        .env("XDG_CACHE_HOME", root.path().join("cache"))
        .env("OPENAI_API_KEY", "beg-local-test-key")
        .env("OPENAI_BASE_URL", endpoint)
        .env("NO_PROXY", "127.0.0.1")
        .args(["--model", "gpt-5.4", "--trace", "cache.jsonl", "script"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    process
        .stdin
        .take()
        .unwrap()
        .write_all(b"stream data only")
        .unwrap();
    let output = process.wait_with_output().unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"16\n");
    assert!(output.stderr.is_empty());
    model.join().unwrap();
    let trace = fs::read_to_string(root.path().join("cache.jsonl")).unwrap();
    assert!(trace.contains(r#""action":"prepared_program""#));
    assert!(trace.contains(r#""action":"ran_program""#));
}

#[test]
#[ignore = "requires a localhost listener for the synthetic Responses endpoint"]
fn interrupt_stops_a_native_command_with_the_embedded_turn() {
    let root = tempfile::tempdir().unwrap();
    let mut command = beg(root.path());
    fs::write(root.path().join("script"), "Run a native command.").unwrap();
    let lock_file = File::create(root.path().join(NATIVE_LOCK_FILE)).unwrap();
    fs::write(
        root.path().join("codex-home/config.toml"),
        "cli_auth_credentials_store = 'file'\n[features]\nunified_exec = true\n",
    )
    .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
    let test_exe = std::env::current_exe().unwrap();
    let quoted_exe = test_exe.to_str().unwrap().replace('\'', "'\\''");
    let hold_lock = format!(
        "BEG_NATIVE_LOCK_FIXTURE=1 exec '{quoted_exe}' --exact fixture_native_command --ignored --nocapture"
    );
    let (running, command_started) = mpsc::channel();
    let model = thread::spawn(move || {
        let (stream, request) = read_live_request(&listener);
        send_call(
            stream,
            &request,
            1,
            "exec_command",
            json!({
                "cmd": hold_lock,
                "login": false,
                "yield_time_ms": 100,
            }),
        );

        let (mut stream, _) = listener.accept().unwrap();
        let request = read_request(&mut stream);
        let output = tool_output(&request, "call_1");
        assert!(output.contains(NATIVE_READY), "{output}");
        running.send(stream).unwrap();
    });
    let child = command
        .env("PATH", "/usr/bin:/bin")
        .env("OPENAI_API_KEY", "beg-local-test-key")
        .env("OPENAI_BASE_URL", endpoint)
        .env("NO_PROXY", "127.0.0.1")
        .args(["--model", "gpt-5.4", "script"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let native_command = command_started.recv_timeout(Duration::from_secs(50));
    let held_lock = !try_lock(&lock_file);
    assert_eq!(
        unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) },
        0
    );
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.signal(), Some(libc::SIGTERM), "{output:?}");
    assert!(output.stdout.is_empty() && output.stderr.is_empty());
    let _request = native_command.expect("native command did not start");
    assert!(held_lock, "native command did not hold the file lock");
    let deadline = Instant::now() + Duration::from_secs(2);
    // The file lock is visible even from outside the Linux sandbox's PID namespace.
    while !try_lock(&lock_file) {
        assert!(
            Instant::now() < deadline,
            "native command still held the file lock after interruption"
        );
        thread::sleep(Duration::from_millis(10));
    }
    model.join().unwrap();
}

#[test]
#[ignore = "run by the embedded interruption test as a native command"]
fn fixture_native_command() {
    if std::env::var_os("BEG_NATIVE_LOCK_FIXTURE").is_some() {
        let file = File::open(NATIVE_LOCK_FILE).unwrap();
        assert!(try_lock(&file));
        println!("{NATIVE_READY}");
        thread::sleep(Duration::from_secs(60));
        drop(file);
    }
}

fn try_lock(file: &File) -> bool {
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return true;
    }
    let error = std::io::Error::last_os_error();
    assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
    false
}

fn tool_output(request: &Value, call_id: &str) -> String {
    let output = &request["input"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["type"] == "function_call_output" && item["call_id"] == call_id)
        .expect("tool result")["output"];
    if let Some(text) = output.as_str() {
        return text.to_owned();
    }
    output
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|item| item["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

fn shell(request: &Value, command: &str) -> (&'static str, Value) {
    let names: Vec<_> = request["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    if names.contains(&"exec_command") {
        ("exec_command", json!({"cmd": command, "login": false}))
    } else if names.contains(&"shell_command") {
        ("shell_command", json!({"command": command, "login": false}))
    } else if names.contains(&"shell") {
        ("shell", json!({"command":["/bin/sh", "-c", command]}))
    } else {
        panic!("no native shell tool in {names:?}");
    }
}

fn read_request(stream: &mut TcpStream) -> Value {
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    assert!(line.starts_with("POST /v1/responses "), "{line}");
    let mut content_length = None;
    loop {
        line.clear();
        assert_ne!(reader.read_line(&mut line).unwrap(), 0);
        if line == "\r\n" {
            break;
        }
        let (name, value) = line.split_once(':').unwrap();
        if name.eq_ignore_ascii_case("content-length") {
            content_length = Some(value.trim().parse().unwrap());
        }
    }
    let mut body = vec![0; content_length.unwrap()];
    reader.read_exact(&mut body).unwrap();
    serde_json::from_slice(&body).unwrap()
}

fn read_live_request(listener: &TcpListener) -> (TcpStream, Value) {
    let (mut stream, _) = listener.accept().unwrap();
    let request = read_request(&mut stream);
    if request["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|tool| tool["name"] == "publish_filter")
    {
        send_response(stream, &request, 0, None);
        stream = listener.accept().unwrap().0;
        let request = read_request(&mut stream);
        return (stream, request);
    }
    (stream, request)
}

fn send_call(stream: TcpStream, request: &Value, sequence: u8, name: &str, args: Value) {
    let item = json!({
        "id": format!("fc_{sequence}"),
        "type":"function_call",
        "call_id": format!("call_{sequence}"),
        "name": name,
        "arguments": args.to_string(),
        "status": "completed",
    });
    send_response(stream, request, sequence, Some(item));
}

fn send_response(mut stream: TcpStream, request: &Value, sequence: u8, item: Option<Value>) {
    let mut response = json!({
        "id": format!("resp_{sequence}"),
        "object": "response",
        "created_at": 1,
        "model": request["model"],
        "status": "in_progress",
        "output": [],
    });
    let created = json!({"type": "response.created", "response": response});
    response["status"] = json!("completed");
    let mut events = vec![created];
    if let Some(item) = item {
        response["output"] = json!([item]);
        let mut start_item = item.clone();
        start_item["status"] = json!("in_progress");
        start_item["arguments"] = json!("");
        events.push(
            json!({"type": "response.output_item.added", "output_index": 0, "item": start_item}),
        );
        events.push(json!({"type": "response.output_item.done", "output_index": 0, "item": item}));
    }
    response["usage"] = json!({
        "input_tokens": 25,
        "input_tokens_details": {"cached_tokens": 0},
        "output_tokens": 5,
        "output_tokens_details": {"reasoning_tokens": 0},
        "total_tokens": 30,
    });
    events.push(json!({"type": "response.completed", "response": response}));
    let mut body = String::new();
    for (index, mut event) in events.into_iter().enumerate() {
        event["sequence_number"] = json!(index);
        let kind = event["type"].as_str().unwrap();
        body.push_str(&format!("event: {kind}\ndata: {event}\n\n"));
    }
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body,
    ).unwrap();
}
