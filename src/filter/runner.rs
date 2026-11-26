use crate::cache::{Environment, EnvironmentInputs, MAX_INPUT, MAX_OUTPUT, ResultEntry, StdinMode};
use crate::streams::{self, Stream, Streams};
use crate::tools::{StreamRead, StreamWrite, decode_base64};
use crate::unix::Signal;
use crate::{End, Failure};
use base64::{Engine, engine::general_purpose::STANDARD};
use codex_code_mode::{
    CodeModeNestedToolCall, CodeModeOutputTruncation, CodeModeSession,
    CodeModeSessionCellExecutionLimits, CodeModeSessionDelegate, CodeModeSessionOptions,
    CodeModeSessionProvider, CodeModeToolKind, ExecuteRequest, FunctionCallOutputContentItem,
    NotificationFuture, ProcessOwnedCodeModeSessionProvider, RuntimeResponse, ToolDefinition,
    ToolInvocationFuture, WaitRequest,
};
use codex_protocol::ToolName;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Cursor, Read};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

const JAVASCRIPT: &str = include_str!("runner.js");

pub(super) struct Runner {
    session: Arc<dyn CodeModeSession>,
    args: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Usage {
    pub stderr: String,
    pub code: u8,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EnvironmentQuery {
    name: Option<String>,
    base64: Option<String>,
}

impl Runner {
    pub(super) async fn open(host: &Path, argv: &[OsString]) -> Result<Self, String> {
        let provider = ProcessOwnedCodeModeSessionProvider::with_host_program(host.to_path_buf());
        let session = provider
            .create_session_with_options(CodeModeSessionOptions {
                cell_execution_limits: CodeModeSessionCellExecutionLimits {
                    max_yield_time_ms: None,
                    max_heap_size_bytes: Some(64 * 1024 * 1024),
                },
                output_truncation: CodeModeOutputTruncation::Server,
                heartbeat_interval_s: 0,
            })
            .await?;
        let args = argv
            .iter()
            .map(|arg| STANDARD.encode(arg.as_bytes()))
            .collect::<Vec<_>>();
        let args = serde_json::to_string(&args).map_err(|error| error.to_string())?;
        Ok(Self { session, args })
    }

    pub(super) async fn evaluate<T: serde::de::DeserializeOwned>(
        &self,
        code: &str,
        phase: &str,
        io: Arc<FilterIo>,
    ) -> Result<T, String> {
        let mut tools = Vec::new();
        if phase != "validate" {
            tools.push("beg_filter_env");
        }
        if phase == "run" {
            tools.push("beg_filter_write");
            if io.stdin == StdinMode::Eof {
                tools.push("beg_filter_read");
            }
        }
        let enabled_tools = tools
            .into_iter()
            .map(|name| ToolDefinition {
                name: name.into(),
                tool_name: ToolName::plain(name),
                description: String::new(),
                kind: CodeModeToolKind::Function,
                input_schema: None,
                input_schema_max_bytes: None,
                output_schema: None,
            })
            .collect();
        let source = format!(
            "{JAVASCRIPT}\nconst begProgram = (() => {{\n{code}\nreturn {{checkArgs, run}};\n}})();\nawait begExecute(begProgram, {}, \"{phase}\");",
            self.args
        );
        let request = ExecuteRequest {
            tool_call_id: phase.into(),
            enabled_tools,
            source,
            yield_time_ms: None,
            max_output_tokens: Some(4096),
        };
        let cell = self.session.execute(request, io, None).await?;
        let mut response = cell.initial_response().await?;
        loop {
            match response {
                RuntimeResponse::Result {
                    content_items,
                    error_text,
                    ..
                } => {
                    if let Some(error) = error_text {
                        return Err(error);
                    }
                    let [FunctionCallOutputContentItem::InputText { text }] =
                        content_items.as_slice()
                    else {
                        return Err("filter returned an invalid result".into());
                    };
                    return serde_json::from_str(text)
                        .map_err(|_| "filter returned an invalid result".into());
                }
                RuntimeResponse::Yielded { cell_id, .. } => {
                    response = self
                        .session
                        .wait(
                            WaitRequest {
                                cell_id,
                                yield_time_ms: 1000,
                                max_output_tokens: Some(4096),
                            },
                            None,
                        )
                        .await?
                        .into();
                }
                RuntimeResponse::Terminated { .. } => {
                    return Err("filter execution was terminated".into());
                }
            }
        }
    }
}

pub(super) struct FilterIo {
    stdin: StdinMode,
    environment: Environment,
    env_reads: Mutex<Option<BTreeSet<Vec<u8>>>>,
    state: Arc<Mutex<State>>,
    cancel: UnixStream,
    failed: Notify,
}

struct State {
    stdin: File,
    stdout: File,
    stderr: File,
    listener: UnixStream,
    prefix: Cursor<Vec<u8>>,
    prefetch_error: Option<io::Error>,
    observed_eof: bool,
    reached_eof: bool,
    read_failed: bool,
    bytes_read: usize,
    bytes_written: [usize; 2],
    capture: Option<[Vec<u8>; 2]>,
    failure: Option<End>,
}

impl FilterIo {
    pub(super) fn new(streams: &Streams, stdin: StdinMode) -> Result<Self, Failure> {
        let (cancel, listener) = UnixStream::pair().map_err(|error| {
            Failure::host(format!("cannot create filter cancellation pipe: {error}"))
        })?;
        let file = |stream| streams.duplicate(stream).map_err(Failure::host);
        let state = State {
            stdin: file(Stream::Stdin)?,
            stdout: file(Stream::Stdout)?,
            stderr: file(Stream::Stderr)?,
            listener,
            prefix: Cursor::new(Vec::new()),
            prefetch_error: None,
            observed_eof: stdin == StdinMode::Unused,
            reached_eof: false,
            read_failed: false,
            bytes_read: 0,
            bytes_written: [0; 2],
            capture: None,
            failure: None,
        };
        Ok(Self {
            stdin,
            environment: std::env::vars_os().collect(),
            env_reads: Mutex::new(Some(BTreeSet::new())),
            state: Arc::new(Mutex::new(state)),
            cancel,
            failed: Notify::new(),
        })
    }

    pub(super) fn environment(&self) -> &Environment {
        &self.environment
    }

    fn read_environment(&self, query: EnvironmentQuery) -> Result<Value, String> {
        let name = match (query.name, query.base64) {
            (Some(name), None) => OsString::from(name),
            (None, Some(base64)) => OsString::from_vec(decode_base64(&base64)?),
            (None, None) => {
                *self.env_reads.lock().unwrap() = None;
                let entries: Vec<_> = self.environment.iter().map(|(name, value)| {
                    json!({"name": native_value(name), "value": native_value(value)})
                }).collect();
                return Ok(json!(entries));
            }
            (Some(_), Some(_)) => {
                return Err("supply one representation of the variable name".into());
            }
        };
        if let Some(reads) = self.env_reads.lock().unwrap().as_mut() {
            reads.insert(name.as_bytes().to_vec());
        }
        Ok(self
            .environment
            .get(&name)
            .map_or(Value::Null, |value| native_value(value)))
    }

    pub(super) fn cancel(&self) {
        streams::cancel_read(&self.cancel);
    }

    pub(super) async fn output_failure(&self) -> End {
        self.failed.notified().await;
        self.state
            .lock()
            .unwrap()
            .failure
            .take()
            .expect("recorded output failure")
    }

    async fn work<T: Send + 'static>(
        &self,
        work: impl FnOnce(&mut State) -> Result<T, String> + Send + 'static,
    ) -> Result<T, String> {
        let state = self.state.clone();
        tokio::task::spawn_blocking(move || {
            let mut state = state
                .try_lock()
                .map_err(|_| "await each filter I/O operation")?;
            if state.failure.is_some() {
                return Err("filter output was interrupted".into());
            }
            work(&mut state)
        })
        .await
        .map_err(|error| error.to_string())?
    }

    pub(super) async fn prefetch(&self) -> Result<Option<[u8; 32]>, Failure> {
        self.work(|state| {
            let mut buf = Vec::new();
            while buf.len() <= MAX_INPUT {
                let limit = (MAX_INPUT + 1 - buf.len()).min(streams::MAX_BYTES);
                match streams::read(&mut state.stdin, &state.listener, limit) {
                    Ok(Some(bytes)) if bytes.is_empty() => {
                        state.reached_eof = true;
                        break;
                    }
                    Ok(Some(bytes)) => {
                        state.bytes_read += bytes.len();
                        buf.extend(bytes);
                    }
                    Ok(None) => return Err("filter input was interrupted".into()),
                    Err(error) => {
                        state.prefetch_error = Some(error);
                        break;
                    }
                }
            }
            let digest = state.reached_eof.then(|| Sha256::digest(&buf).into());
            state.prefix = Cursor::new(buf);
            Ok(digest)
        })
        .await
        .map_err(Failure::host)
    }

    pub(super) fn begin_capture(&self) -> Result<(), Failure> {
        let mut state = self
            .state
            .try_lock()
            .map_err(|_| Failure::host("filter I/O is already in progress"))?;
        state.capture = Some(Default::default());
        Ok(())
    }

    pub(super) fn complete(&self, code: u8) -> Result<Option<ResultEntry>, End> {
        let mut state = self
            .state
            .try_lock()
            .map_err(|_| Failure::host("filter exited with an I/O operation in progress"))?;
        if let Some(end) = state.failure.take() {
            return Err(end);
        }
        if !state.observed_eof && !state.read_failed {
            return Err(Failure::host("filter exited before reading stdin through EOF").into());
        }
        Ok(state.capture.take().map(|[stdout, stderr]| ResultEntry {
            code,
            stdout,
            stderr,
            environment: EnvironmentInputs::new(
                self.env_reads.lock().unwrap().take(),
                &self.environment,
            ),
        }))
    }

    pub(super) fn counts(&self) -> (usize, [usize; 2]) {
        self.state
            .try_lock()
            .map(|state| (state.bytes_read, state.bytes_written))
            .unwrap_or_default()
    }

    pub(super) async fn emit(&self, stream: Stream, bytes: Vec<u8>) -> Result<(), Failure> {
        self.write(stream, bytes, false)
            .await
            .map_err(Failure::host)
    }

    async fn write(&self, stream: Stream, bytes: Vec<u8>, require_eof: bool) -> Result<(), String> {
        let written = self
            .work(move |state| {
                if require_eof && !state.observed_eof && !state.read_failed {
                    return Err("filter output requires reading stdin through EOF".into());
                }
                state.write(stream, bytes)
            })
            .await?;
        if !written {
            self.failed.notify_one();
            // The driver handles the failure as a program signal or host error.
            return std::future::pending().await;
        }
        Ok(())
    }
}

fn native_value(value: &OsStr) -> Value {
    json!({"text": value.to_str(), "base64": STANDARD.encode(value.as_bytes())})
}

impl State {
    fn read(&mut self, max: usize) -> Result<Vec<u8>, String> {
        if max == 0 {
            return Err("filter read size must be positive".into());
        }
        let max = max.min(streams::MAX_BYTES);
        let buffered = self.prefix.position() < self.prefix.get_ref().len() as u64;
        let result = if buffered {
            let mut bytes = vec![0; max];
            let count = self.prefix.read(&mut bytes).expect("reading memory");
            bytes.truncate(count);
            Ok(bytes)
        } else if let Some(error) = self.prefetch_error.take() {
            Err(error)
        } else if self.reached_eof || self.read_failed {
            Ok(Vec::new())
        } else {
            streams::read(&mut self.stdin, &self.listener, max).and_then(|bytes| {
                bytes.ok_or_else(|| io::Error::other("filter input was interrupted"))
            })
        };
        match result {
            Ok(bytes) => {
                self.observed_eof = bytes.is_empty();
                self.reached_eof |= self.observed_eof;
                if !buffered {
                    self.bytes_read += bytes.len();
                }
                Ok(bytes)
            }
            Err(error) => {
                self.read_failed = true;
                self.capture = None;
                Err(error.to_string())
            }
        }
    }

    fn write(&mut self, stream: Stream, bytes: Vec<u8>) -> Result<bool, String> {
        let (file, index) = match stream {
            Stream::Stdout => (&mut self.stdout, 0),
            Stream::Stderr => (&mut self.stderr, 1),
            Stream::Stdin => return Err("cannot write to stdin".into()),
        };
        let result = streams::write(file, &bytes);
        self.bytes_written[index] += result.written;
        if let Some(error) = result.error {
            self.capture = None;
            self.failure = Some(if error.kind() == io::ErrorKind::BrokenPipe {
                End::Signal(Signal::Pipe)
            } else {
                Failure::host(format!("cannot write {}: {error}", stream.name())).into()
            });
            return Ok(false);
        }
        if let Some(output) = &mut self.capture {
            if output[0].len() + output[1].len() + bytes.len() <= MAX_OUTPUT {
                output[index].extend_from_slice(&bytes);
            } else {
                self.capture = None;
            }
        }
        Ok(true)
    }
}

impl CodeModeSessionDelegate for FilterIo {
    fn invoke_tool<'a>(
        &'a self,
        invocation: CodeModeNestedToolCall,
        cancellation: CancellationToken,
    ) -> ToolInvocationFuture<'a> {
        Box::pin(async move {
            let tool = async {
                let input = invocation.input.unwrap_or(Value::Null);
                match invocation.tool_name.name.as_str() {
                    "beg_filter_env" => {
                        let query =
                            serde_json::from_value(input).map_err(|error| error.to_string())?;
                        self.read_environment(query)
                    }
                    "beg_filter_read" => {
                        let args: StreamRead =
                            serde_json::from_value(input).map_err(|error| error.to_string())?;
                        self.work(move |state| state.read(args.max_bytes))
                            .await
                            .map(|bytes| json!(STANDARD.encode(bytes)))
                    }
                    "beg_filter_write" => {
                        let args: StreamWrite =
                            serde_json::from_value(input).map_err(|error| error.to_string())?;
                        let stream = args.stream;
                        let bytes = args.bytes()?;
                        if bytes.len() > streams::MAX_BYTES {
                            return Err("filter write exceeds the chunk size".into());
                        }
                        self.write(stream, bytes, true).await?;
                        Ok(Value::Null)
                    }
                    _ => Err("unknown filter I/O operation".into()),
                }
            };
            tokio::select! {
                result = tool => result,
                _ = cancellation.cancelled() => Err("filter I/O was cancelled".into()),
            }
        })
    }

    fn notify<'a>(
        &'a self,
        _call_id: String,
        _cell_id: codex_code_mode::CellId,
        _text: String,
        _cancellation: CancellationToken,
    ) -> NotificationFuture<'a> {
        Box::pin(async { Err("filter notifications are unavailable".into()) })
    }

    fn cell_closed(&self, _cell_id: &codex_code_mode::CellId) {}
}
