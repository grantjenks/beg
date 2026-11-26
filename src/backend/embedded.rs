use super::{Message, Settings};
use crate::Failure;
use crate::process::Event;
use codex_app_server_client::legacy_core::config::{ConfigBuilder, ConfigOverrides};
use codex_app_server_client::{
    EmbeddedNetworkPolicy, EnvironmentManager, ExecServerRuntimeOptions, InProcessAppServerClient,
    InProcessClientStartArgs, InProcessServerEvent,
};
use codex_app_server_protocol::{ClientRequest, JSONRPCErrorError, RequestId, ServerNotification};
use codex_arg0::Arg0DispatchPaths;
use codex_config::LoaderOverrides;
use codex_feedback::CodexFeedback;
use codex_protocol::protocol::SessionSource;
use codex_utils_absolute_path::AbsolutePathBuf;
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::mpsc::SyncSender;
use std::thread::{self, JoinHandle};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;

const CAPACITY: usize = 64;
// Match the stack size used by the upstream embedding binaries.
const STACK_SIZE: usize = 16 * 1024 * 1024;

pub(super) struct Transport {
    outgoing: Option<mpsc::Sender<Value>>,
    cancel: Option<oneshot::Sender<()>>,
    worker: Option<JoinHandle<()>>,
}

impl Transport {
    pub(super) fn start(settings: Settings, events: SyncSender<Event>) -> Result<Self, Failure> {
        let (outgoing, incoming) = mpsc::channel(CAPACITY);
        let (cancel, cancelled) = oneshot::channel();
        let worker = thread::Builder::new()
            .name("beg-codex".into())
            .stack_size(STACK_SIZE)
            .spawn(move || {
                let result = run(settings, &events, incoming, cancelled);
                if let Err(failure) = result {
                    let _ = events.send(Event::Backend(Message::Failed(failure)));
                }
            })
            .map_err(|error| {
                Failure::host(format!("cannot start embedded Codex worker: {error}"))
            })?;
        Ok(Self {
            outgoing: Some(outgoing),
            cancel: Some(cancel),
            worker: Some(worker),
        })
    }

    pub(super) fn send(&self, frame: Value) -> Result<(), Failure> {
        self.outgoing
            .as_ref()
            .ok_or_else(|| Failure::host("embedded Codex connection is closed"))?
            .try_send(frame)
            .map_err(|_| Failure::host("embedded Codex is not accepting requests"))
    }

    pub(super) fn close(&mut self) {
        self.outgoing = None;
    }

    pub(super) fn force_stop(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            let _ = cancel.send(());
        }
    }

    pub(super) fn reap(&mut self) -> Result<(), Failure> {
        if self.stopped() {
            if let Some(worker) = self.worker.take() {
                worker
                    .join()
                    .map_err(|_| Failure::host("embedded Codex worker panicked"))?;
            }
        }
        Ok(())
    }

    pub(super) fn stopped(&self) -> bool {
        self.worker.as_ref().is_none_or(JoinHandle::is_finished)
    }
}

fn run(
    settings: Settings,
    events: &SyncSender<Event>,
    incoming: mpsc::Receiver<Value>,
    cancelled: oneshot::Receiver<()>,
) -> Result<(), Failure> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(2)
        .thread_stack_size(STACK_SIZE)
        .build()
        .map_err(|error| Failure::host(format!("cannot create embedded Codex runtime: {error}")))?;
    let result = runtime.block_on(async {
        tokio::select! {
            _ = cancelled => Ok(()),
            result = serve(&settings, events, incoming) => result,
        }
    });
    runtime.shutdown_timeout(Duration::from_millis(100));
    result
}

async fn start_client(settings: &Settings) -> Result<InProcessAppServerClient, Failure> {
    let cli_overrides = settings.overrides();
    let codex_self_exe = std::env::current_exe()
        .map_err(|error| Failure::host(format!("cannot locate the beg executable: {error}")))?;
    #[cfg(target_os = "linux")]
    let codex_linux_sandbox_exe = {
        let alias = settings.scratch.path().join(super::LINUX_SANDBOX_ARG0);
        std::os::unix::fs::symlink(&codex_self_exe, &alias).map_err(|error| {
            Failure::host(format!("cannot create the Codex sandbox alias: {error}"))
        })?;
        Some(alias)
    };
    #[cfg(not(target_os = "linux"))]
    let codex_linux_sandbox_exe = None;
    let runtime_paths =
        ExecServerRuntimeOptions::new(codex_self_exe.clone(), codex_linux_sandbox_exe.clone())
            .map_err(|error| Failure::host(format!("invalid Codex helper paths: {error}")))?;
    let loader_overrides =
        LoaderOverrides {
            default_cwd: Some(AbsolutePathBuf::from_absolute_path(&settings.cwd).map_err(
                |_| Failure::host("embedded Codex requires an absolute working directory"),
            )?),
            ..Default::default()
        };
    let mut config = ConfigBuilder::default()
        .codex_home(settings.auth_home.clone())
        .cli_overrides(cli_overrides.clone())
        .loader_overrides(loader_overrides.clone())
        .harness_overrides(ConfigOverrides {
            codex_self_exe: Some(codex_self_exe.clone()),
            codex_linux_sandbox_exe: codex_linux_sandbox_exe.clone(),
            ..Default::default()
        })
        .build()
        .await
        .map_err(|error| {
            Failure::host(format!(
                "cannot load embedded Codex configuration ({})",
                error.kind()
            ))
        })?;
    let network_policy = EmbeddedNetworkPolicy::load(&loader_overrides).await;
    network_policy.activate(&mut config);

    #[cfg(target_os = "macos")]
    let runtime_paths = runtime_paths.with_allowed_symlinked_codex_home(
        codex_config::allowed_symlinked_codex_home(&config.config_layer_stack, &config.codex_home),
    );
    let environment_manager = EnvironmentManager::from_codex_home(
        config.codex_home.clone(),
        Some(runtime_paths),
        config.http_client_factory(),
    )
    .await
    .map_err(|_| Failure::host("cannot initialize Codex execution environments"))?;
    let state_db = codex_core::init_state_db(&config).await;
    InProcessAppServerClient::start(InProcessClientStartArgs {
        required_human_approval: false,
        arg0_paths: Arg0DispatchPaths {
            codex_self_exe: Some(codex_self_exe),
            codex_linux_sandbox_exe,
            ..Default::default()
        },
        config: Arc::new(config),
        cli_overrides,
        loader_overrides,
        strict_config: false,
        cloud_config_bundle: Default::default(),
        embedded_network_policy: network_policy,
        feedback: CodexFeedback::new(),
        log_db: None,
        state_db,
        environment_manager: Arc::new(environment_manager),
        config_warnings: Vec::new(),
        session_source: SessionSource::Exec,
        enable_codex_api_key_env: true,
        client_name: "beg".into(),
        client_version: env!("CARGO_PKG_VERSION").into(),
        experimental_api: true,
        mcp_server_openai_form_elicitation: false,
        opt_out_notification_methods: Vec::new(),
        channel_capacity: CAPACITY,
    })
    .await
    .map_err(|error| Failure::host(format!("cannot start embedded Codex ({})", error.kind())))
}

async fn serve(
    settings: &Settings,
    events: &SyncSender<Event>,
    mut incoming: mpsc::Receiver<Value>,
) -> Result<(), Failure> {
    let mut client = start_client(settings).await?;
    emit(events, Message::Ready)?;
    let mut requests = JoinSet::new();
    loop {
        tokio::select! {
            frame = incoming.recv() => {
                let Some(frame) = frame else { break };
                if frame["method"].is_string() {
                    let id = frame["id"].clone();
                    let interrupt = frame["method"] == "turn/interrupt";
                    let request: ClientRequest = serde_json::from_value(frame)
                        .map_err(|_| Failure::host("invalid request to embedded Codex"))?;
                    if interrupt {
                        // Shutdown can close our queue as soon as it sends this
                        // request. Submit it before we stop draining the queue.
                        let _ = client.request(request).await;
                        continue;
                    }
                    let handle = client.request_handle();
                    requests.spawn(async move {
                        match handle.request(request).await {
                            Ok(Ok(result)) => Ok(json!({"id": id, "result": result})),
                            Ok(Err(error)) => Ok(json!({"id": id, "error": error})),
                            Err(_) => Err(Failure::host("embedded Codex request was disconnected")),
                        }
                    });
                } else {
                    resolve(&client, frame).await?;
                }
            }
            result = requests.join_next(), if !requests.is_empty() => {
                let reply = result.expect("nonempty request set")
                    .map_err(|_| Failure::host("embedded Codex request task stopped unexpectedly"))??;
                emit(events, Message::Frame(reply))?;
            }
            event = client.next_event() => {
                let event = event.ok_or_else(|| Failure::host("embedded Codex stopped before the script exited"))?;
                if let Some(message) = event_message(event, settings.debug)? {
                    emit(events, message)?;
                }
            }
        }
    }
    requests.abort_all();
    client
        .shutdown()
        .await
        .map_err(|_| Failure::host("embedded Codex shutdown failed"))
}

async fn resolve(client: &InProcessAppServerClient, mut frame: Value) -> Result<(), Failure> {
    let id: RequestId = serde_json::from_value(frame["id"].take())
        .map_err(|_| Failure::host("invalid embedded Codex tool request ID"))?;
    let response = if let Some(error) = frame.get_mut("error") {
        let error: JSONRPCErrorError = serde_json::from_value(error.take())
            .map_err(|_| Failure::host("invalid embedded Codex tool error"))?;
        client.reject_server_request(id, error).await
    } else {
        client
            .resolve_server_request(id, frame["result"].take())
            .await
    };
    response.map_err(|_| Failure::host("embedded Codex tool response was disconnected"))
}

fn event_message(event: InProcessServerEvent, debug: bool) -> Result<Option<Message>, Failure> {
    let frame = match event {
        InProcessServerEvent::ServerRequest(request) => serde_json::to_value(*request),
        InProcessServerEvent::ServerNotification(notification) => {
            if !debug
                && !matches!(
                    notification.as_ref(),
                    ServerNotification::TurnStarted(_)
                        | ServerNotification::TurnCompleted(_)
                        | ServerNotification::ThreadTokenUsageUpdated(_)
                        | ServerNotification::Warning(_)
                )
            {
                return Ok(None);
            }
            serde_json::to_value(*notification)
        }
        InProcessServerEvent::Lagged { skipped } => {
            return Ok(debug.then_some(Message::EventsSkipped(skipped)));
        }
    };
    frame
        .map(|frame| Some(Message::Frame(frame)))
        .map_err(|_| Failure::host("invalid event from embedded Codex"))
}

fn emit(events: &SyncSender<Event>, message: Message) -> Result<(), Failure> {
    events
        .send(Event::Backend(message))
        .map_err(|_| Failure::host("beg closed the embedded Codex event channel"))
}
