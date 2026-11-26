use crate::backend::{self, Backend, ToolCall};
use crate::cache::Script;
use crate::cli::Invocation;
use crate::filter;
use crate::policy::{self, Policy};
use crate::streams::{self, Completion, Stream, Streams};
use crate::tools::{Operation, Spawn};
use crate::trace::Trace;
use crate::unix::{self, Group, Signal, Signals};
use crate::{End, Failure};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::env;
use std::ffi::OsString;
use std::fs::{File, Metadata};
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread;
use std::time::{Duration, Instant};

pub(crate) const TICK: Duration = Duration::from_millis(20);
const SIGNAL_GRACE: Duration = Duration::from_secs(5);
pub(crate) const SHUTDOWN_GRACE: Duration = Duration::from_millis(200);
pub(crate) const SHUTDOWN_LIMIT: Duration = Duration::from_millis(500);
const EVENT_LIMIT: usize = 64;
const EVENT_CHANNEL_CAPACITY: usize = 16;

pub enum Event {
    Backend(backend::Message),
    Stream(Completion),
}

struct Prepared {
    script: Script,
    metadata: Metadata,
    cwd: PathBuf,
    policy: Policy,
    trace: Trace,
}

impl Prepared {
    fn load(
        path: &Path,
        trace_path: Option<&Path>,
        debug: bool,
        open_trace: bool,
        started: Instant,
    ) -> Result<Self, Failure> {
        let source_error =
            |error: io::Error| Failure::new(66, format!("cannot read {path:?}: {error}"));
        let file = File::open(path).map_err(source_error)?;
        let source_info = file.metadata().map_err(source_error)?;
        let mut source = String::new();
        file.take((backend::MAX_FRAME + 1) as u64)
            .read_to_string(&mut source)
            .map_err(source_error)?;
        if source.len() > backend::MAX_FRAME {
            return Err(Failure::new(66, "script exceeds the 8 MiB source limit"));
        }
        let cwd = env::current_dir().map_err(|error| {
            Failure::host(format!("cannot determine the working directory: {error}"))
        })?;
        let policy = Policy::load()?;
        let script = Script::new(source);
        let trace = Trace::open(trace_path, debug, open_trace, &source_info, started)
            .map_err(|error| Failure::host(format!("cannot open trace: {error}")))?;
        Ok(Self {
            script,
            metadata: source_info,
            cwd,
            policy,
            trace,
        })
    }
}

pub fn run(invocation: Invocation, started: Instant, signals: Signals) -> End {
    match start(invocation, started, signals) {
        Ok(mut supervisor) => {
            let end = match supervisor.drive() {
                Ok(end) => end,
                Err(error) => supervisor
                    .signal_grace
                    .map_or_else(|| error.into(), |(signal, _)| End::Signal(signal)),
            };
            let end = supervisor.shutdown(end);
            finish_invocation(end, &supervisor.streams, &mut supervisor.trace)
        }
        Err(end) => end,
    }
}

fn finish_invocation(mut end: End, streams: &Streams, trace: &mut Trace) -> End {
    let detail = match &end {
        End::Exit(code) => json!({"kind": "exit", "code": code}),
        End::Failed(error) => json!({"kind": "error", "code": error.code}),
        End::Signal(signal) => json!({"kind": "signal", "signal": signal.name()}),
    };
    trace.record("finish", detail);
    if let Some(error) = trace.take_error()
        && matches!(end, End::Exit(_))
    {
        end = Failure::host(format!("cannot write trace: {error}")).into();
    }
    if let Err(error) = trace.open_viewer() {
        streams.diagnostic(format!("cannot open trace viewer: {error}"));
    }
    streams.finish(end)
}

fn start(invocation: Invocation, started: Instant, signals: Signals) -> Result<Supervisor, End> {
    let deadline = invocation
        .timeout
        .map(|timeout| {
            started
                .checked_add(timeout)
                .ok_or_else(|| Failure::new(64, "timeout is too large"))
        })
        .transpose()?;
    let path = PathBuf::from(&invocation.argv[0]);
    let trace = invocation.trace.clone();
    let debug = invocation.debug;
    let open_trace = invocation.open_trace;
    let Prepared {
        script,
        metadata,
        cwd,
        policy,
        mut trace,
    } = startup_task(&signals, deadline, move || {
        Prepared::load(&path, trace.as_deref(), debug, open_trace, started)
    })?;
    let streams = Streams::new()
        .map_err(|error| Failure::host(format!("cannot open standard descriptors: {error}")))?;
    if let Some(path) = trace.temporary_path() {
        let label = if debug { "debug trace" } else { "trace" };
        streams::diagnostic(format!("{label} at {}", path.display()));
    }
    trace.record(
        "start",
        json!({
            "backend": backend::transport_name(),
            "codex_version": backend::VERSION,
            "codex_revision": backend::REVISION,
            "model": invocation.model,
            "script_sha256": script.digest,
            "argc": invocation.argv.len(),
            "launch_argc": invocation.launch.len(),
            "timeout_ms": invocation.timeout.map(|duration| duration.as_millis()),
            "child_execution": if policy.spawn { "inherit" } else { "deny" },
        }),
    );
    if let Some(error) = trace.take_error() {
        return Err(Failure::host(format!("cannot write trace: {error}")).into());
    }
    if let Err(error) = unix::isolate_stdio() {
        return Err(streams.finish(
            Failure::host(format!("cannot isolate standard descriptors: {error}")).into(),
        ));
    }

    let filter = filter::Run {
        script: &script,
        metadata: &metadata,
        cwd: &cwd,
        invocation: &invocation,
        signals: &signals,
        deadline,
    };
    match filter.attempt(&streams, &mut trace) {
        Ok(None) => {}
        Ok(Some(end)) | Err(end) => return Err(finish_invocation(end, &streams, &mut trace)),
    }
    let settings = match load_backend_settings(&cwd, debug, &signals, deadline) {
        Ok(settings) => settings,
        Err(end) => return Err(finish_invocation(end, &streams, &mut trace)),
    };
    let (send_events, events) = mpsc::sync_channel(EVENT_CHANNEL_CAPACITY);
    let backend = match Backend::start(
        settings,
        script.description().into(),
        invocation.model,
        backend::Mode::Run {
            spawn_allowed: policy.spawn,
        },
        send_events.clone(),
    ) {
        Ok(backend) => backend,
        Err(failure) => {
            return Err(finish_invocation(failure.into(), &streams, &mut trace));
        }
    };
    Ok(Supervisor {
        argv: invocation.argv,
        cwd,
        policy,
        streams,
        backend,
        trace,
        signals,
        deadline,
        events,
        send_events,
        signal_grace: None,
        subscriptions: HashSet::new(),
        tool_calls: HashSet::new(),
        children: BTreeMap::new(),
        next_child: 1,
        read: None,
        writes: Default::default(),
        lifecycle: VecDeque::new(),
        event_wait: None,
    })
}

fn startup_task<T: Send + 'static>(
    signals: &Signals,
    deadline: Option<Instant>,
    task: impl FnOnce() -> Result<T, Failure> + Send + 'static,
) -> Result<T, End> {
    check_startup(signals, deadline)?;
    let (send, wait) = mpsc::channel();
    thread::Builder::new()
        .name("beg-startup".into())
        .spawn(move || {
            let _ = send.send(task());
        })
        .map_err(|error| Failure::host(format!("cannot prepare invocation: {error}")))?;
    loop {
        check_startup(signals, deadline)?;
        match wait.recv_timeout(TICK) {
            Ok(result) => {
                check_startup(signals, deadline)?;
                return Ok(result?);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(Failure::host("preparation stopped unexpectedly").into());
            }
        }
    }
}

pub(crate) fn load_backend_settings(
    cwd: &Path,
    debug: bool,
    signals: &Signals,
    deadline: Option<Instant>,
) -> Result<backend::Settings, End> {
    let cwd = cwd.to_path_buf();
    startup_task(signals, deadline, move || {
        backend::Settings::load(&cwd, debug)
    })
}

pub(crate) fn check_startup(signals: &Signals, deadline: Option<Instant>) -> Result<(), End> {
    if let Some(signal) = signals.first() {
        return Err(End::Signal(signal));
    }
    if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
        return Err(timed_out());
    }
    Ok(())
}

fn timed_out() -> End {
    Failure::new(124, "invocation deadline exceeded").into()
}

struct Supervisor {
    argv: Vec<OsString>,
    cwd: PathBuf,
    policy: Policy,
    streams: Streams,
    backend: Backend,
    trace: Trace,
    signals: Signals,
    deadline: Option<Instant>,
    events: Receiver<Event>,
    send_events: SyncSender<Event>,
    subscriptions: HashSet<Signal>,
    signal_grace: Option<(Signal, Instant)>,
    tool_calls: HashSet<String>,
    children: BTreeMap<u64, Child>,
    next_child: u64,
    read: Option<(Value, UnixStream)>,
    writes: [Option<Value>; 3],
    lifecycle: VecDeque<Value>,
    event_wait: Option<(Value, Option<Instant>)>,
}

enum Child {
    Running {
        process: Group,
        connected_output: bool,
        waiter: Option<Value>,
    },
    Done(ChildResult),
}

#[derive(Clone, Copy, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ChildResult {
    Exit { code: i32 },
    Signal { signal: i32 },
}

impl ChildResult {
    fn from_status(status: ExitStatus) -> Self {
        if let Some(signal) = status.signal() {
            Self::Signal { signal }
        } else {
            Self::Exit {
                code: status.code().expect("reaped child status"),
            }
        }
    }
}

enum Action {
    Reply(Value),
    Pending,
    Exit(u8),
}

impl Supervisor {
    fn drive(&mut self) -> Result<End, Failure> {
        loop {
            if let Some(end) = self.check_stop()? {
                return Ok(end);
            }
            let now = Instant::now();
            if let Some(error) = self.trace.take_error() {
                return Err(Failure::host(format!("cannot write trace: {error}")));
            }
            self.backend.reap()?;
            if self.reap_children(true)? {
                return Ok(End::Signal(Signal::Pipe));
            }
            if self
                .event_wait
                .as_ref()
                .is_some_and(|(_, deadline)| deadline.is_some_and(|deadline| now >= deadline))
            {
                let (id, _) = self.event_wait.take().expect("pending event wait");
                self.backend.reply(id, true, json!({"event": null}))?;
            }
            let event = match self.events.recv_timeout(self.poll_timeout(now)) {
                Ok(event) => event,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(Failure::host("event loop disconnected"));
                }
            };
            if let Event::Backend(message) = &event {
                self.trace.backend_event(message);
            }
            if let Some(end) = self.check_stop()? {
                // A write may have signaled a closed pipe before shutdown can
                // finish accounting for operations already in flight.
                if let Event::Stream(completion) = event {
                    self.complete_stream(completion, false)?;
                }
                return Ok(end);
            }
            match event {
                Event::Backend(message) => match self.backend.message(message)? {
                    backend::Update::Ignore => {}
                    backend::Update::PreparationFinished => unreachable!("running a live script"),
                    backend::Update::Started(model) => {
                        self.trace.record("model", json!({"id": model}))
                    }
                    backend::Update::Usage(usage) => self.trace.record("token_usage", usage),
                    backend::Update::Tool(call) => {
                        if let Some(end) = self.tool(call)? {
                            return Ok(end);
                        }
                    }
                },
                Event::Stream(completion) => {
                    if self.complete_stream(completion, true)? {
                        return Ok(End::Signal(Signal::Pipe));
                    }
                }
            }
        }
    }

    fn check_stop(&mut self) -> Result<Option<End>, Failure> {
        if let Some(end) = self.check_signals()? {
            return Ok(Some(end));
        }
        Ok(self
            .deadline
            .filter(|deadline| Instant::now() >= *deadline)
            .map(|_| timed_out()))
    }

    fn poll_timeout(&self, now: Instant) -> Duration {
        [
            self.deadline,
            self.signal_grace.map(|(_, deadline)| deadline),
            self.event_wait.as_ref().and_then(|(_, deadline)| *deadline),
        ]
        .into_iter()
        .flatten()
        .map(|deadline| deadline.saturating_duration_since(now))
        .fold(TICK, Duration::min)
    }

    fn check_signals(&mut self) -> Result<Option<End>, Failure> {
        if let Some(signal) = self.signals.second() {
            return Ok(Some(End::Signal(signal)));
        }
        if let Some((signal, deadline)) = self.signal_grace {
            return Ok((Instant::now() >= deadline).then_some(End::Signal(signal)));
        }
        let Some(signal) = self.signals.first() else {
            return Ok(None);
        };
        let subscribed = self.subscriptions.contains(&signal);
        self.trace.record(
            "signal",
            json!({"name": signal.name(), "subscribed": subscribed}),
        );
        if !subscribed {
            return Ok(Some(End::Signal(signal)));
        }
        self.signal_grace = Some((signal, Instant::now() + SIGNAL_GRACE));
        if self.event_wait.is_none() {
            self.backend.steer_for_signal(signal);
        }
        self.publish(json!({"kind": "signal", "signal": signal.name()}), true)?;
        if let Some((_, cancel)) = &self.read {
            streams::cancel_read(cancel);
        }
        for child in self.children.values_mut() {
            if let Child::Running { waiter, .. } = child {
                if let Some(id) = waiter.take() {
                    self.backend.reply(
                        id,
                        false,
                        json!({"error": "child wait interrupted", "signal": signal.name()}),
                    )?;
                }
            }
        }
        Ok(None)
    }

    fn publish(&mut self, event: Value, priority: bool) -> Result<(), Failure> {
        if let Some((id, _)) = self.event_wait.take() {
            self.backend.reply(id, true, json!({"event": event}))?;
        } else if priority {
            if self.lifecycle.len() == EVENT_LIMIT {
                self.lifecycle.pop_back();
            }
            self.lifecycle.push_front(event);
        } else {
            if self.lifecycle.len() == EVENT_LIMIT {
                self.lifecycle.pop_front();
            }
            self.lifecycle.push_back(event);
        }
        Ok(())
    }

    fn tool(&mut self, call: ToolCall) -> Result<Option<End>, Failure> {
        if !self.tool_calls.insert(call.call_id) {
            self.backend
                .reply(call.id, false, json!({"error": "duplicate tool call"}))?;
            return Ok(None);
        }
        let operation: Operation = match serde_json::from_value(json!({
            "tool": call.name, "arguments": call.arguments,
        })) {
            Ok(operation) => operation,
            Err(error) => {
                self.trace.record("invalid_tool", json!({}));
                self.backend.reply(
                    call.id,
                    false,
                    json!({"error": format!("invalid process tool call: {error}")}),
                )?;
                return Ok(None);
            }
        };
        let name = operation.name();
        let result = self.execute(operation, &call.id);
        self.trace
            .record("tool", json!({"name": name, "accepted": result.is_ok()}));
        match result {
            Ok(Action::Reply(body)) => self.backend.reply(call.id, true, body)?,
            Ok(Action::Pending) => {}
            Ok(Action::Exit(code)) => {
                let _ = self.backend.reply(call.id, true, json!({"code": code}));
                return Ok(Some(End::Exit(code)));
            }
            Err(error) => self
                .backend
                .reply(call.id, false, json!({"error": error}))?,
        }
        Ok(None)
    }

    fn execute(&mut self, operation: Operation, id: &Value) -> Result<Action, String> {
        let body = match operation {
            Operation::ProcessContext(_) => {
                let argv: Vec<_> = self
                    .argv
                    .iter()
                    .enumerate()
                    .map(|(index, arg)| policy::native(arg, format!("argv:{index}")))
                    .collect();
                json!({
                    "argv": argv,
                    "pid": std::process::id(),
                    "ppid": unix::parent_pid(),
                    "process_group": unix::process_group(),
                    "cwd": policy::native(self.cwd.as_os_str(), "cwd".into()),
                    "streams": self.streams.context(),
                    "spawn_allowed": self.policy.spawn,
                    "environment_names": self.policy.env_names,
                })
            }
            Operation::EnvGet(args) => {
                let value = self
                    .policy
                    .get_env(&args.name)?
                    .map(|value| policy::native(&value, format!("env:{}", args.name)));
                json!({"value": value})
            }
            Operation::StreamRead(args) => {
                let cancel = self
                    .streams
                    .begin_read(args.max_bytes, self.send_events.clone())?;
                self.read = Some((id.clone(), cancel));
                return Ok(Action::Pending);
            }
            Operation::StreamWrite(args) => {
                let stream = args.stream;
                self.streams
                    .begin_write(stream, args.bytes()?, self.send_events.clone())?;
                self.writes[stream as usize] = Some(id.clone());
                return Ok(Action::Pending);
            }
            Operation::StreamFlush(args) => {
                if args.stream == Stream::Stdin {
                    return Err("cannot flush stdin".into());
                }
                self.streams.flush(args.stream)?;
                json!({})
            }
            Operation::StreamClose(args) => {
                self.streams.close(args.stream)?;
                json!({})
            }
            Operation::Spawn(args) => self.spawn(args)?,
            Operation::Wait(args) => match self.children.get_mut(&args.child) {
                Some(Child::Done(result)) => json!(result),
                Some(Child::Running { waiter, .. }) if waiter.is_none() => {
                    *waiter = Some(id.clone());
                    return Ok(Action::Pending);
                }
                Some(Child::Running { .. }) => {
                    return Err("child already has a pending wait".into());
                }
                None => return Err("unknown child handle".into()),
            },
            Operation::SignalSubscribe(args) => {
                if !args
                    .signals
                    .iter()
                    .all(|signal| Signal::TERMINATING.contains(signal))
                {
                    return Err("can subscribe to SIGINT, SIGTERM, SIGHUP and SIGQUIT".into());
                }
                self.subscriptions = args.signals.into_iter().collect();
                json!({})
            }
            Operation::SignalSend(args) => match self.children.get_mut(&args.child) {
                Some(Child::Running { process, .. }) => {
                    process
                        .signal(args.signal)
                        .map_err(|error| error.to_string())?;
                    json!({})
                }
                Some(Child::Done(_)) => return Err("child has already finished".into()),
                None => return Err("unknown child handle".into()),
            },
            Operation::EventNext(args) => {
                if self.event_wait.is_some() {
                    return Err("there is already a pending event wait".into());
                }
                if let Some(event) = self.lifecycle.pop_front() {
                    return Ok(Action::Reply(json!({"event": event})));
                }
                if args.timeout_ms == Some(0) {
                    return Ok(Action::Reply(json!({"event": null})));
                }
                let deadline = args
                    .timeout_ms
                    .map(|ms| {
                        Instant::now()
                            .checked_add(Duration::from_millis(ms))
                            .ok_or("event timeout is too large")
                    })
                    .transpose()?;
                self.event_wait = Some((id.clone(), deadline));
                return Ok(Action::Pending);
            }
            Operation::ProcessExit(args) => return Ok(Action::Exit(args.code)),
        };
        Ok(Action::Reply(body))
    }

    fn spawn(&mut self, args: Spawn) -> Result<Value, String> {
        if !self.policy.spawn {
            return Err("child execution denied by BEG_EXEC".into());
        }
        let bindings = [
            args.stdin.stream(),
            args.stdout.stream(),
            args.stderr.stream(),
        ];
        if matches!(bindings[0], Some(Stream::Stdout | Stream::Stderr))
            || bindings[1..].contains(&Some(Stream::Stdin))
        {
            return Err("stream binding has the wrong direction".into());
        }
        let resolve = |value| self.policy.resolve(value, &self.argv, &self.cwd);
        let cwd = match args.cwd {
            Some(value) => self.cwd.join(resolve(value)?),
            None => self.cwd.clone(),
        };
        let program = resolve(args.program)?;
        let program = if program.as_bytes().contains(&b'/') {
            cwd.join(program).into_os_string()
        } else {
            program
        };
        let mut command = Command::new(program);
        command.args(
            args.args
                .into_iter()
                .map(resolve)
                .collect::<Result<Vec<_>, _>>()?,
        );
        command
            .current_dir(cwd)
            .process_group(0)
            .stdin(self.streams.child_stdio(bindings[0])?)
            .stdout(self.streams.child_stdio(bindings[1])?)
            .stderr(self.streams.child_stdio(bindings[2])?);
        if !args.inherit_env {
            command.env_clear();
        }
        for (name, value) in args.env {
            if !policy::valid_env_name(&name) {
                return Err("invalid child environment variable name".into());
            }
            command.env(name, resolve(value)?);
        }
        let id = self.next_child;
        let next_id = id.checked_add(1).ok_or("child handle space exhausted")?;
        let child = command
            .spawn()
            .map_err(|error| format!("cannot spawn child: {error}"))?;
        let pid = child.id();
        self.children.insert(
            id,
            Child::Running {
                process: Group::new(child),
                connected_output: bindings[1..].iter().any(Option::is_some),
                waiter: None,
            },
        );
        self.streams.attach(bindings, id);
        self.next_child = next_id;
        self.trace
            .record("child_started", json!({"child": id, "pid": pid}));
        Ok(json!({"child": id, "pid": pid}))
    }

    fn reap_children(&mut self, notify: bool) -> Result<bool, Failure> {
        let mut completed = Vec::new();
        for (id, child) in &mut self.children {
            let Child::Running {
                process,
                connected_output,
                waiter,
            } = child
            else {
                continue;
            };
            if let Some(status) = process
                .reap()
                .map_err(|error| Failure::host(format!("cannot reap child: {error}")))?
            {
                let result = ChildResult::from_status(status);
                completed.push((*id, waiter.take(), result, *connected_output));
                *child = Child::Done(result);
            }
        }
        for (id, waiter, result, connected_output) in completed {
            self.streams.release(id);
            self.trace
                .record("child_finished", json!({"child": id, "result": result}));
            if connected_output
                && matches!(result, ChildResult::Signal { signal } if signal == Signal::Pipe.number())
            {
                return Ok(true);
            }
            if notify {
                if let Some(waiter) = waiter {
                    self.backend.reply(waiter, true, json!(result))?;
                }
                self.publish(
                    json!({"kind": "child", "child": id, "result": result}),
                    false,
                )?;
            }
        }
        Ok(false)
    }

    fn complete_stream(&mut self, completion: Completion, notify: bool) -> Result<bool, Failure> {
        match completion {
            Completion::Read(result) => {
                let (id, _) = self.read.take().expect("pending stdin read");
                match self.streams.finish_read(result) {
                    Ok((body, bytes)) if notify => {
                        self.trace.record(
                            "stdin_read",
                            json!({"bytes_to_agent": bytes, "eof": body["eof"]}),
                        );
                        self.backend.reply(id, true, body)?;
                    }
                    Err(error) if notify => {
                        self.backend.reply(id, false, json!({"error": error}))?
                    }
                    _ => {}
                }
            }
            Completion::Write(stream, result) => {
                self.streams.finish_write(stream);
                let id = self.writes[stream as usize]
                    .take()
                    .expect("pending output write");
                self.trace.record(
                    "stream_written",
                    json!({"stream": stream.name(), "bytes": result.written}),
                );
                if let Some(error) = result.error {
                    if error.kind() == io::ErrorKind::BrokenPipe {
                        return Ok(true);
                    }
                    if notify {
                        self.backend.reply(
                            id,
                            false,
                            json!({"written": result.written, "error": error.to_string()}),
                        )?;
                    }
                } else if notify {
                    self.backend
                        .reply(id, true, json!({"written": result.written}))?;
                }
            }
        }
        Ok(false)
    }

    fn signal_children(&mut self, signal: Signal) {
        for child in self.children.values_mut() {
            if let Child::Running { process, .. } = child {
                let _ = process.signal(signal);
            }
        }
    }

    fn shutdown(&mut self, mut end: End) -> End {
        self.backend.shutdown();
        if let Some((_, cancel)) = &self.read {
            streams::cancel_read(cancel);
        }
        let start = Instant::now();
        let mut forced = false;
        self.signal_children(Signal::Term);
        loop {
            let _ = self.backend.reap();
            let broken_pipe = self.reap_children(false).unwrap_or(false);
            if broken_pipe && matches!(end, End::Exit(_)) {
                end = End::Signal(Signal::Pipe);
            }
            let all_children_stopped = self
                .children
                .values()
                .all(|child| matches!(child, Child::Done(_)));
            if self.backend.stopped()
                && all_children_stopped
                && self.writes.iter().all(Option::is_none)
            {
                break;
            }
            let elapsed = start.elapsed();
            if elapsed >= SHUTDOWN_LIMIT {
                break;
            }
            if !forced && (elapsed >= SHUTDOWN_GRACE || self.signals.second().is_some()) {
                self.signal_children(Signal::Kill);
                self.backend.force_stop();
                forced = true;
            }
            match self.events.recv_timeout(TICK) {
                Ok(Event::Backend(message)) => self.trace.backend_event(&message),
                Ok(Event::Stream(completion)) => {
                    if self.complete_stream(completion, false).unwrap_or(false)
                        && matches!(end, End::Exit(_))
                    {
                        end = End::Signal(Signal::Pipe);
                    }
                }
                Err(_) => {}
            }
        }
        for event in self.events.try_iter().take(EVENT_CHANNEL_CAPACITY) {
            if let Event::Backend(message) = event {
                self.trace.backend_event(&message);
            }
        }
        end
    }
}
