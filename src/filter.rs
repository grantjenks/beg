mod runner;

use crate::backend::{Backend, Mode, Update};
use crate::cache::{Cache, MAX_CODE, ProgramEntry, Script, StdinMode, Storage};
use crate::cli::Invocation;
use crate::process::{self, Event};
use crate::streams::{Stream, Streams};
use crate::trace::Trace;
use crate::unix::Signals;
use crate::{End, Failure};
use runner::{FilterIo, Runner, Usage};
use serde::Deserialize;
use serde_json::json;
use std::env;
use std::fs::Metadata;
use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

pub struct Run<'a> {
    pub script: &'a Script,
    pub metadata: &'a Metadata,
    pub cwd: &'a Path,
    pub invocation: &'a Invocation,
    pub signals: &'a Signals,
    pub deadline: Option<Instant>,
}

#[derive(Clone, Copy)]
enum Origin {
    Embedded,
    Cache,
    Prepared,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Publication {
    source: String,
    stdin: StdinMode,
}

impl Run<'_> {
    pub fn attempt(&self, streams: &Streams, trace: &mut Trace) -> Result<Option<End>, End> {
        let host = env::current_exe()
            .map(|exe| exe.with_file_name("codex-code-mode-host"))
            .map_err(|error| {
                Failure::host(format!("cannot locate the local filter host: {error}"))
            })?;
        if !host.is_file() {
            if self.invocation.embed {
                trace.record("cache", json!({"action": "runner_unavailable"}));
            }
            return self.fallback("install the matching codex-code-mode-host beside beg");
        }
        let cache = match Cache::open() {
            Ok(cache) => Some(cache),
            Err(error) => {
                trace.record(
                    "cache",
                    json!({"action": "cache_unavailable", "reason": error.to_string()}),
                );
                None
            }
        };
        let model = self.invocation.model.as_deref();
        let entry = cache
            .as_ref()
            .and_then(|cache| cache.program(self.script, model));
        let (local, declined) = match entry {
            Some(ProgramEntry::Prepared(storage)) => (Some(storage), false),
            Some(ProgramEntry::Declined) => (None, true),
            None => (None, false),
        };
        for (storage, origin) in [
            (self.script.embedded(model), Origin::Embedded),
            (local, Origin::Cache),
        ] {
            if let Some(storage) = storage
                && let Some(end) =
                    self.execute(streams, trace, cache.as_ref(), &host, &storage, origin)?
            {
                return Ok(Some(end));
            }
        }
        if !self.invocation.embed {
            if declined {
                trace.record("cache", json!({"action": "cached_decline"}));
                return Ok(None);
            }
            if cache.is_none() {
                return Ok(None);
            }
        }
        let storage = match self.prepare(trace) {
            Ok(Some(storage)) => storage,
            Ok(None) => {
                trace.record("cache", json!({"action": "preparation_declined"}));
                if let Some(cache) = &cache
                    && let Err(error) = cache.put_declined(self.script, model)
                {
                    trace.record(
                        "cache",
                        json!({"action": "write_failed", "reason": error.to_string()}),
                    );
                }
                return self.fallback("Codex did not publish a reusable filter");
            }
            Err(End::Failed(error))
                if error.code == 70 && !self.invocation.embed && trace.error().is_none() =>
            {
                trace.record(
                    "cache",
                    json!({"action": "preparation_failed", "reason": error.message}),
                );
                return Ok(None);
            }
            Err(end) => return Err(end),
        };
        match self.execute(
            streams,
            trace,
            cache.as_ref(),
            &host,
            &storage,
            Origin::Prepared,
        )? {
            Some(end) => Ok(Some(end)),
            None => self.fallback("the prepared filter did not pass validation"),
        }
    }

    fn fallback(&self, reason: &str) -> Result<Option<End>, End> {
        if self.invocation.embed {
            Err(Failure::host(reason).into())
        } else {
            Ok(None)
        }
    }

    fn execute(
        &self,
        streams: &Streams,
        trace: &mut Trace,
        cache: Option<&Cache>,
        host: &Path,
        storage: &Storage,
        origin: Origin,
    ) -> Result<Option<End>, End> {
        process::check_startup(self.signals, self.deadline)?;
        let io = Arc::new(FilterIo::new(streams, storage.stdin())?);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| {
                Failure::host(format!("cannot start the local filter runtime: {error}"))
            })?;
        let result = runtime.block_on(async {
            tokio::select! {
                biased;
                end = self.wait_for_stop() => Err(end),
                end = io.output_failure() => Err(end),
                result = self.execute_local(trace, cache, host, storage, origin, io.clone()) => result,
            }
        });
        io.cancel();
        runtime.shutdown_timeout(Duration::from_millis(100));
        let (read, [stdout, stderr]) = io.counts();
        if read > 0 {
            trace.record("filter_input", json!({"bytes": read}));
        }
        for (stream, bytes) in [("stdout", stdout), ("stderr", stderr)] {
            if bytes > 0 {
                trace.record("stream_written", json!({"stream": stream, "bytes": bytes}));
            }
        }
        if matches!(result, Ok(Some(End::Exit(_))))
            && matches!(origin, Origin::Prepared)
            && let Some(cache) = cache
            && let Err(error) =
                cache.put_program(self.script, self.invocation.model.as_deref(), storage)
        {
            trace.record(
                "cache",
                json!({"action": "write_failed", "reason": error.to_string()}),
            );
        }
        result
    }

    async fn execute_local(
        &self,
        trace: &mut Trace,
        cache: Option<&Cache>,
        host: &Path,
        storage: &Storage,
        origin: Origin,
        io: Arc<FilterIo>,
    ) -> Result<Option<End>, End> {
        let argv = &self.invocation.argv;
        let phase = if self.invocation.embed {
            "validate"
        } else {
            "check"
        };
        let prepared = async {
            let runner = Runner::open(host, argv).await?;
            let usage: Option<Usage> = runner.evaluate(storage.code(), phase, io.clone()).await?;
            Ok::<_, String>((runner, usage))
        }
        .await;
        let (runner, usage) = match prepared {
            Ok(prepared) => prepared,
            Err(reason) => {
                trace.record(
                    "cache",
                    json!({"action": "program_rejected", "reason": reason}),
                );
                return Ok(None);
            }
        };
        trace.record(
            "cache",
            json!({
                "action": match origin {
                    Origin::Embedded => "embedded_program",
                    Origin::Cache => "cached_program",
                    Origin::Prepared => "prepared_program",
                },
                "model": storage.model(),
                "stdin": storage.stdin(),
            }),
        );
        process::check_startup(self.signals, self.deadline)?;

        if self.invocation.embed {
            if !matches!(origin, Origin::Embedded) {
                self.script
                    .embed(Path::new(&argv[0]), self.metadata, storage)
                    .map_err(|error| Failure::host(format!("cannot embed the filter: {error}")))?;
            }
            trace.record("cache", json!({"action": "program_embedded"}));
            return Ok(Some(End::Exit(0)));
        }
        if let Some(usage) = usage {
            io.emit(Stream::Stderr, usage.stderr.into_bytes()).await?;
            return Ok(Some(End::Exit(usage.code)));
        }

        let mut result_key = None;
        if let Some(cache) = cache {
            let key = match storage.stdin() {
                StdinMode::Unused => Some(storage.result_key(argv, None)),
                StdinMode::Eof => io
                    .prefetch()
                    .await?
                    .map(|stdin| storage.result_key(argv, Some(&stdin))),
            };
            if let Some(key) = key {
                if let Some(result) = cache.result(&key, io.environment()) {
                    trace.record("cache", json!({"action": "replayed_result"}));
                    io.emit(Stream::Stdout, result.stdout).await?;
                    io.emit(Stream::Stderr, result.stderr).await?;
                    return Ok(Some(End::Exit(result.code)));
                }
                io.begin_capture()?;
                result_key = Some((cache, key));
            } else {
                trace.record("cache", json!({"action": "streaming_input"}));
            }
        }
        trace.record("cache", json!({"action": "ran_program"}));
        let code = runner
            .evaluate::<u8>(storage.code(), "run", io.clone())
            .await
            .map_err(|error| Failure::host(format!("local filter failed: {error}")))?;
        let output = io.complete(code)?;
        process::check_startup(self.signals, self.deadline)?;
        if let (Some((cache, key)), Some(output)) = (result_key, output)
            && let Err(error) = cache.put_result(&key, &output)
        {
            trace.record(
                "cache",
                json!({"action": "write_failed", "reason": error.to_string()}),
            );
        }
        Ok(Some(End::Exit(code)))
    }

    async fn wait_for_stop(&self) -> End {
        loop {
            if let Err(end) = process::check_startup(self.signals, self.deadline) {
                return end;
            }
            let tick = self.deadline.map_or(process::TICK, |deadline| {
                deadline
                    .saturating_duration_since(Instant::now())
                    .min(process::TICK)
            });
            tokio::time::sleep(tick).await;
        }
    }

    fn prepare(&self, trace: &mut Trace) -> Result<Option<Storage>, End> {
        let settings = process::load_backend_settings(
            self.cwd,
            self.invocation.debug,
            self.signals,
            self.deadline,
        )?;
        trace.record("cache", json!({"action": "preparing_program"}));
        let (send, events) = mpsc::sync_channel(16);
        let mut backend = Backend::start(
            settings,
            self.script.description().into(),
            self.invocation.model.clone(),
            Mode::PrepareFilter,
            send,
        )?;
        let result = self.wait_for_program(&mut backend, &events, trace);
        backend.shutdown();
        let started = Instant::now();
        while !backend.stopped() && started.elapsed() < process::SHUTDOWN_LIMIT {
            let _ = backend.reap();
            if started.elapsed() >= process::SHUTDOWN_GRACE {
                backend.force_stop();
            }
            if let Ok(Event::Backend(message)) = events.recv_timeout(process::TICK) {
                trace.backend_event(&message);
            }
        }
        for event in events.try_iter().take(16) {
            if let Event::Backend(message) = event {
                trace.backend_event(&message);
            }
        }
        result
    }

    fn wait_for_program(
        &self,
        backend: &mut Backend,
        events: &Receiver<Event>,
        trace: &mut Trace,
    ) -> Result<Option<Storage>, End> {
        let mut model = self.invocation.model.clone();
        loop {
            process::check_startup(self.signals, self.deadline)?;
            if let Some(error) = trace.error() {
                return Err(Failure::host(format!("cannot write trace: {error}")).into());
            }
            let message = match events.recv_timeout(process::TICK) {
                Ok(Event::Backend(message)) => message,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                _ => return Err(Failure::host("filter preparation stopped unexpectedly").into()),
            };
            trace.backend_event(&message);
            match backend.message(message)? {
                Update::Ignore => {}
                Update::PreparationFinished => return Ok(None),
                Update::Usage(usage) => trace.record("token_usage", usage),
                Update::Started(id) => {
                    trace.record("model", json!({"id": id}));
                    model = id.as_str().map(str::to_owned).or(model);
                }
                Update::Tool(call) => {
                    let publication = serde_json::from_value::<Publication>(call.arguments)
                        .ok()
                        .filter(|publication| {
                            call.name == "publish_filter"
                                && publication.source.len() <= MAX_CODE
                                && !publication.source.trim().is_empty()
                        });
                    trace.record(
                        "tool",
                        json!({"name": call.name, "accepted": publication.is_some()}),
                    );
                    if let Some(publication) = publication {
                        let _ = backend.reply(call.id, true, json!({"received": true}));
                        return Ok(Some(Storage::new(
                            self.script,
                            publication.source,
                            publication.stdin,
                            model,
                        )));
                    }
                    backend.reply(
                        call.id,
                        false,
                        json!({"error": "publish_filter requires JavaScript source and stdin mode eof or unused"}),
                    )?;
                }
            }
        }
    }
}
