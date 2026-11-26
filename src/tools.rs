use crate::streams::{MAX_BYTES, Stream};
use crate::unix::Signal;
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;

#[derive(Deserialize)]
#[serde(tag = "tool", content = "arguments", rename_all = "snake_case")]
pub enum Operation {
    ProcessContext(Empty),
    EnvGet(EnvGet),
    StreamRead(StreamRead),
    StreamWrite(StreamWrite),
    StreamFlush(StreamArg),
    StreamClose(StreamArg),
    Spawn(Spawn),
    Wait(ChildArg),
    SignalSubscribe(Subscription),
    SignalSend(SignalSend),
    EventNext(EventNext),
    ProcessExit(ProcessExit),
}

impl Operation {
    pub fn name(&self) -> &'static str {
        match self {
            Self::ProcessContext(_) => "process_context",
            Self::EnvGet(_) => "env_get",
            Self::StreamRead(_) => "stream_read",
            Self::StreamWrite(_) => "stream_write",
            Self::StreamFlush(_) => "stream_flush",
            Self::StreamClose(_) => "stream_close",
            Self::Spawn(_) => "spawn",
            Self::Wait(_) => "wait",
            Self::SignalSubscribe(_) => "signal_subscribe",
            Self::SignalSend(_) => "signal_send",
            Self::EventNext(_) => "event_next",
            Self::ProcessExit(_) => "process_exit",
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Empty {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvGet {
    pub name: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamRead {
    pub max_bytes: usize,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamWrite {
    pub stream: Stream,
    pub text: Option<String>,
    pub base64: Option<String>,
}

impl StreamWrite {
    pub fn bytes(self) -> Result<Vec<u8>, String> {
        match (self.text, self.base64) {
            (Some(text), None) => Ok(text.into_bytes()),
            (None, Some(data)) => decode_base64(&data),
            _ => Err("supply exactly one of text and base64".into()),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamArg {
    pub stream: Stream,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Binding {
    #[default]
    Null,
    Stdin,
    Stdout,
    Stderr,
}

impl Binding {
    pub fn stream(&self) -> Option<Stream> {
        match self {
            Self::Null => None,
            Self::Stdin => Some(Stream::Stdin),
            Self::Stdout => Some(Stream::Stdout),
            Self::Stderr => Some(Stream::Stderr),
        }
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
pub enum Native {
    Text(String),
    Handle(Handle),
    Bytes(Bytes),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Handle {
    pub handle: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Bytes {
    pub base64: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Spawn {
    pub program: Native,
    #[serde(default)]
    pub args: Vec<Native>,
    pub cwd: Option<Native>,
    #[serde(default)]
    pub env: BTreeMap<String, Native>,
    #[serde(default = "inherit")]
    pub inherit_env: bool,
    #[serde(default)]
    pub stdin: Binding,
    #[serde(default)]
    pub stdout: Binding,
    #[serde(default)]
    pub stderr: Binding,
}

fn inherit() -> bool {
    true
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChildArg {
    pub child: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Subscription {
    pub signals: Vec<Signal>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignalSend {
    pub child: u64,
    pub signal: Signal,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventNext {
    pub timeout_ms: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessExit {
    pub code: u8,
}

pub fn decode_base64(text: &str) -> Result<Vec<u8>, String> {
    STANDARD
        .decode(text)
        .map_err(|_| "invalid standard base64".into())
}

fn function(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "function",
        "name": name,
        "description": format!("{description} In Code Mode, the result is a JSON string. Parse it with JSON.parse."),
        "deferLoading": false,
        "inputSchema": {
            "type": "object",
            "properties": properties,
            "required": required,
            "additionalProperties": false,
        },
    })
}

pub fn publish_filter() -> Value {
    function(
        "publish_filter",
        "Submit a reusable JavaScript filter and specify whether it reads stdin through EOF.",
        json!({
            "source": {"type": "string"},
            "stdin": {"type": "string", "enum": ["unused", "eof"]},
        }),
        &["source", "stdin"],
    )
}

pub fn specifications(spawn_allowed: bool) -> Vec<Value> {
    let text = json!({"type": "string"});
    let streams = json!({"type": "string", "enum": ["stdin", "stdout", "stderr"]});
    let terminating = Signal::TERMINATING.map(Signal::name);
    let child = json!({"type": "integer", "minimum": 1});
    let mut tools = vec![
        function(
            "process_context",
            "Get script argv, working directory, process IDs, streams, and allowed operations.",
            json!({}),
            &[],
        ),
        function(
            "env_get",
            "Look up one permitted environment variable. Returns a native value or null.",
            json!({"name": text}),
            &["name"],
        ),
        function(
            "stream_read",
            &format!(
                "Read up to max_bytes from stdin, with a {MAX_BYTES}-byte limit per read. Read again if more input is needed and eof is false. base64 holds the exact bytes read. text decodes UTF-8 across reads and may contain replacement characters when text_lossy is true."
            ),
            json!({"max_bytes": {"type": "integer", "minimum": 1}}),
            &["max_bytes"],
        ),
        function(
            "stream_write",
            "Write exact bytes to stdout or stderr. Supply one of text (UTF-8) or base64 (binary). No newline is added.",
            json!({"stream": {"type": "string", "enum": ["stdout", "stderr"]}, "text": text, "base64": text}),
            &["stream"],
        ),
        function(
            "stream_flush",
            "Confirm no write is pending on the selected output stream. Completed writes already went to the OS.",
            json!({"stream": {"type": "string", "enum": ["stdout", "stderr"]}}),
            &["stream"],
        ),
        function(
            "stream_close",
            "Close one available program stream.",
            json!({"stream": streams}),
            &["stream"],
        ),
        function(
            "signal_subscribe",
            "Replace the set of signals that trigger script cleanup. Use event_next to receive signals. The first subscribed signal starts a five-second grace period.",
            json!({"signals": {"type": "array", "items": {"type": "string", "enum": terminating}}}),
            &["signals"],
        ),
        function(
            "event_next",
            "Wait for a signal or child exit event. A timeout_ms of zero checks without waiting. Returns an event or null on timeout.",
            json!({"timeout_ms": {"type": "integer", "minimum": 0}}),
            &[],
        ),
        function(
            "process_exit",
            "Finish this invocation by committing a Unix exit status. This is the last operation.",
            json!({"code": {"type": "integer", "minimum": 0, "maximum": 255}}),
            &["code"],
        ),
    ];
    if spawn_allowed {
        let native = json!({
            "oneOf": [
                text,
                {"type": "object", "properties": {"handle": text}, "required": ["handle"], "additionalProperties": false},
                {"type": "object", "properties": {"base64": text}, "required": ["base64"], "additionalProperties": false},
            ],
        });
        tools.extend([
            function("spawn",
                "Run an executable with an argument array. Use an explicit shell executable for a pipeline. Bind stdin to stdin and outputs to stdout or stderr to stream directly. Unspecified bindings use /dev/null. Returns a child handle immediately.",
                json!({
                    "program": native,
                    "args": {"type": "array", "items": native},
                    "cwd": native,
                    "env": {"type": "object", "additionalProperties": native},
                    "inherit_env": {"type": "boolean"},
                    "stdin": {"type": "string", "enum": ["stdin", "null"]},
                    "stdout": {"type": "string", "enum": ["stdout", "stderr", "null"]},
                    "stderr": {"type": "string", "enum": ["stdout", "stderr", "null"]},
                }), &["program"]),
            function("wait",
                "Get a managed child's final result. The result has kind exit with code or kind signal with the OS signal number.",
                json!({"child": child}), &["child"]),
            function("signal_send",
                "Send a signal to a managed child's process group.",
                json!({
                    "child": child,
                    "signal": {"type": "string", "enum": ["SIGHUP", "SIGINT", "SIGQUIT", "SIGTERM", "SIGPIPE", "SIGKILL", "SIGUSR1", "SIGUSR2"]},
                }), &["child", "signal"]),
        ]);
    }
    tools
}
