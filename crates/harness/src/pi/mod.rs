//! Native Pi JSONL RPC driver. See PROTOCOL.md for the legacy ACK barrier.
mod catalog;
mod normalize;
mod rpc;
mod sessions;
mod ui;

use crate::{
    Harness, HarnessError, RunControls,
    process::{Child, Command, Stdio},
};
use async_trait::async_trait;
use futures::{StreamExt, stream::BoxStream};
use normalize::{Normalizer, string};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    sync::mpsc,
};
use zeron_proto::{AgentEvent, HarnessId, Model, ReasoningLevel, RunRequest, SteeringMode};

pub struct PiHarness {
    models_cache: crate::catalog::Catalog,
    workspace_commands: crate::skills::CommandDiscovery,
    session_store: Option<PathBuf>,
    executable: Option<PathBuf>,
    interrupt_grace: Duration,
    kill_grace: Duration,
}
impl Default for PiHarness {
    fn default() -> Self {
        Self {
            executable: None,
            session_store: None,
            models_cache: Default::default(),
            workspace_commands: Default::default(),
            interrupt_grace: Duration::from_secs(2),
            kill_grace: Duration::from_secs(3),
        }
    }
}
impl PiHarness {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn with_executable(mut self, path: impl Into<PathBuf>) -> Self {
        self.executable = Some(path.into());
        self
    }
    pub fn with_session_store(mut self, path: impl Into<PathBuf>) -> Self {
        self.session_store = Some(path.into());
        self
    }
    pub fn with_graces(mut self, interrupt: Duration, kill: Duration) -> Self {
        self.interrupt_grace = interrupt;
        self.kill_grace = kill;
        self
    }
    pub fn resolve_executable(&self) -> Result<PathBuf, HarnessError> {
        if let Some(path) = self
            .executable
            .clone()
            .or_else(|| std::env::var_os("PI_EXECUTABLE").map(PathBuf::from))
        {
            return crate::executable::validate_native_override(&path);
        }
        let home = crate::executable::home_or_current_dir();
        crate::executable::find_on_paths(
            "pi",
            vec![
                home.join(".local/bin/pi"),
                home.join(".npm-global/bin/pi"),
                PathBuf::from("/opt/homebrew/bin/pi"),
                PathBuf::from("/usr/local/bin/pi"),
            ],
        )
        .ok_or_else(|| {
            HarnessError::NotInstalled(
                "Pi CLI: install Pi or set PI_EXECUTABLE (the pi-acp adapter is not used)".into(),
            )
        })
    }
    async fn probe(&self, cwd: &Path, models: bool) -> Result<Value, HarnessError> {
        let mut process = self.spawn(cwd, &["--no-session".into()])?;
        let (mut tx, rx) = tokio::sync::oneshot::channel();
        let grace = self.kill_grace;
        tokio::spawn(async move {
            let result = tokio::select! {
                result=tokio::time::timeout(Duration::from_secs(60),async {
                    if models {Ok(serde_json::to_value(catalog::models(&mut process).await?).unwrap())}
                    else {process.query(json!({"type":"get_commands"}),&mut vec![]).await}
                })=>result.unwrap_or_else(|_|Err(HarnessError::Protocol("Pi discovery timed out".into()))),
                _=tx.closed()=>Err(HarnessError::Protocol("Pi discovery cancelled".into())),
            };
            process.shutdown(grace).await;
            let _ = tx.send(result);
        });
        rx.await
            .map_err(|_| HarnessError::Protocol("Pi discovery task failed".into()))?
    }
    fn spawn(&self, cwd: &Path, args: &[String]) -> Result<Process, HarnessError> {
        let exe = self.resolve_executable()?;
        if self.executable.is_none() {
            if let Some(version) = crate::executable::binary_version(&exe) {
                if version < semver::Version::new(0, 85, 1) {
                    return Err(HarnessError::Protocol(format!(
                        "Pi {version} is unsupported; update Pi to 0.85.1 or newer"
                    )));
                }
            }
        }
        let mut cmd = Command::new(&exe);
        crate::compose_child_path(&mut cmd, &exe);
        cmd.args(["--mode", "rpc", "--no-themes"])
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        cmd.process_group(0);
        let mut child = cmd.spawn()?;
        let tail = crate::StderrTail::default();
        let mut lines = BufReader::new(child.stderr.take().expect("piped stderr")).lines();
        let stderr = tail.clone();
        let stderr_task = tokio::spawn(async move {
            while let Ok(Some(line)) = lines.next_line().await {
                stderr.push(&line);
            }
            stderr.close();
        });
        let transport = rpc::Transport::new(
            child.stdin.take().expect("piped stdin"),
            child.stdout.take().expect("piped stdout"),
        );
        Ok(Process {
            child,
            transport,
            tail,
            stderr_task,
            dialogs: Default::default(),
        })
    }
}
struct Process {
    child: Child,
    transport: rpc::Transport,
    tail: crate::StderrTail,
    stderr_task: tokio::task::JoinHandle<()>,
    dialogs: ui::Dialogs,
}
impl Drop for Process {
    fn drop(&mut self) {
        self.stderr_task.abort();
    }
}
impl Process {
    async fn query(
        &mut self,
        command: Value,
        backlog: &mut Vec<Value>,
    ) -> Result<Value, HarnessError> {
        let id = self.transport.client.request(command)?;
        while let Some(frame) = self.transport.incoming.recv().await {
            let frame = frame?;
            if frame["type"] == "response" && frame["id"] == id {
                return response_data(frame);
            }
            if frame["type"] == "extension_ui_request" {
                self.dialogs.request(self.transport.client.clone(), &frame);
            } else {
                backlog.push(frame);
            }
        }
        Err(HarnessError::Protocol("Pi disconnected".into()))
    }
    async fn shutdown(&mut self, grace: Duration) {
        crate::shutdown_child(&mut self.child, grace).await;
    }
}
fn response_data(frame: Value) -> Result<Value, HarnessError> {
    if frame["success"] == true {
        Ok(frame["data"].clone())
    } else {
        Err(HarnessError::Protocol(format!(
            "Pi {}: {}",
            string(&frame, "command"),
            frame["error"].as_str().unwrap_or("command failed")
        )))
    }
}
#[async_trait]
impl Harness for PiHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Pi
    }
    fn display_name(&self) -> &str {
        "Pi"
    }
    fn supports_steering(&self) -> bool {
        true
    }
    fn steering_mode(&self) -> SteeringMode {
        SteeringMode::StepBoundary
    }
    fn reasoning_levels(&self) -> &[ReasoningLevel] {
        &[]
    }
    fn installed(&self) -> bool {
        self.resolve_executable().is_ok()
    }
    fn executable_path(&self) -> Option<PathBuf> {
        self.resolve_executable().ok()
    }
    fn deterministic_turn_end(&self) -> bool {
        true
    }
    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        Ok(self.model_catalog(false).await?.models)
    }
    fn model_context(&self) -> Result<Option<crate::ModelContext>, HarnessError> {
        let root = crate::model_context::root(
            "PI_CODING_AGENT_DIR",
            crate::executable::home_or_current_dir().join(".pi/agent"),
        );
        crate::model_context::context(
            HarnessId::Pi,
            &self.resolve_executable()?,
            &[root.join("models.json"), root.join("settings.json")],
        )
        .map(Some)
    }
    async fn model_catalog(&self, force: bool) -> Result<crate::ModelCatalog, HarnessError> {
        self.models_cache
            .get_with_timeout(
                force,
                Duration::from_secs(65),
                || Ok(self.model_context()?.expect("Pi model context").key()),
                || async {
                    let value = self.probe(&std::env::current_dir()?, true).await?;
                    serde_json::from_value(value).map_err(|e| HarnessError::Protocol(e.to_string()))
                },
            )
            .await
    }
    async fn commands(&self) -> Result<Vec<zeron_proto::SlashCommand>, HarnessError> {
        self.commands_for(&std::env::current_dir()?).await
    }
    async fn commands_for(
        &self,
        cwd: &Path,
    ) -> Result<Vec<zeron_proto::SlashCommand>, HarnessError> {
        self.workspace_commands
            .get(cwd, async {
                Ok(catalog::commands(&self.probe(cwd, false).await?))
            })
            .await
    }
    async fn skills(
        &self,
        cwd: &Path,
    ) -> Result<Option<Vec<zeron_proto::invocation::Skill>>, HarnessError> {
        let mut skills = crate::skills::discover(HarnessId::Pi, cwd).await?;
        let commands = self.commands_for(cwd).await?;
        crate::skills::attach_advertised_commands(HarnessId::Pi, &mut skills, &commands);
        Ok(Some(skills))
    }
    fn fallback_models(&self) -> Vec<Model> {
        vec![Model {
            id: "default".into(),
            label: "Pi default".into(),
            description: Some("Model configured in Pi".into()),
            reasoning_levels: vec![],
            options: vec![],
        }]
    }
    async fn run(
        &self,
        request: RunRequest,
        controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        let store = sessions::Store::new(self.session_store.clone());
        let args = if let Some(id) = &request.resume {
            vec![
                "--session".into(),
                store
                    .resolve(id, Path::new(&request.cwd))?
                    .display()
                    .to_string(),
            ]
        } else {
            vec![]
        };
        let process = self.spawn(Path::new(&request.cwd), &args)?;
        let (tx, rx) = mpsc::channel(256);
        let kill_grace = self.kill_grace;
        let interrupt_grace = self.interrupt_grace;
        tokio::spawn(async move {
            let mut runner = Runner {
                store,
                interrupted: false,
                extension_commands: HashSet::new(),
                auto_compaction: true,
                initial_pending: true,
                delivery: None,
                queued: VecDeque::new(),
                process,
                tx,
                request,
                norm: Normalizer::default(),
                active: true,
                epoch: 0,
                pending: HashMap::new(),
                session: String::new(),
                assistant: uuid::Uuid::new_v4().to_string(),
            };
            // The lease lives through shutdown even if the consumer drops its stream.
            let RunControls {
                execution_lease: _lease,
                request_input,
                mut steering,
                interrupt,
            } = controls;
            runner.process.dialogs.input = Some(std::sync::Arc::from(request_input));
            let consumer = runner.tx.clone();
            let result = tokio::select! {
                result=runner.run(&mut steering)=>result,
                _=interrupt.cancelled()=>{ runner.interrupt(interrupt_grace).await; Ok(()) },
                _=consumer.closed()=>Ok(()),
            };
            if let Err(error) = result {
                let status = runner.process.child.try_wait().ok().flatten();
                runner.norm.error = Some(if status.is_some() {
                    crate::crash_message("Pi", status, &runner.process.tail)
                } else {
                    error.to_string()
                });
            }
            runner.process.shutdown(kill_grace).await;
            let _ = runner.finish().await;
        });
        Ok(
            futures::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|v| (v, rx)) })
                .boxed(),
        )
    }
}
#[derive(Clone, Copy)]
enum Pending {
    Prompt(u64),
    Barrier(u64),
    Command(u64),
}
struct Runner {
    extension_commands: HashSet<String>,
    auto_compaction: bool,
    interrupted: bool,
    initial_pending: bool,
    delivery: Option<crate::SteerMessage>,
    queued: VecDeque<crate::SteerMessage>,
    store: sessions::Store,
    process: Process,
    tx: mpsc::Sender<Result<AgentEvent, HarnessError>>,
    request: RunRequest,
    norm: Normalizer,
    active: bool,
    epoch: u64,
    pending: HashMap<String, Pending>,
    session: String,
    assistant: String,
}
impl Runner {
    async fn emit(&self, event: AgentEvent) -> Result<(), HarnessError> {
        self.tx
            .send(Ok(event))
            .await
            .map_err(|_| HarnessError::Protocol("Pi event consumer closed".into()))
    }
    async fn started(&self, model: String) -> Result<(), HarnessError> {
        self.emit(AgentEvent::SessionStarted {
            harness: HarnessId::Pi,
            model,
            tools: vec![],
            cwd: self.request.cwd.clone(),
            session_id: self.session.clone(),
            assistant_message_id: self.assistant.clone(),
        })
        .await
    }
    async fn finish(&mut self) -> Result<(), HarnessError> {
        if !self.active {
            return Ok(());
        }
        self.active = false;
        self.process.dialogs.cancel();
        self.emit(AgentEvent::Done {
            status: if self.interrupted {
                zeron_proto::DoneStatus::Interrupted
            } else {
                self.norm.status()
            },
            result: None,
            error: self.norm.error.clone(),
            session_id: (!self.session.is_empty()).then(|| self.session.clone()),
        })
        .await
    }
    fn control_command(&self, text: &str) -> Option<Value> {
        let name = text
            .trim()
            .split_whitespace()
            .next()
            .unwrap_or("")
            .trim_start_matches('/');
        if self.extension_commands.contains(name) {
            None
        } else {
            catalog::builtin(text, self.auto_compaction)
        }
    }
    fn submit(&mut self, text: String, images: Value, steer: bool) -> Result<(), HarnessError> {
        let control = self.control_command(&text);
        let command=control.clone().unwrap_or_else(||json!({"type":"prompt","message":text,"images":images,"streamingBehavior":if steer {"steer"}else{"followUp"}}));
        let id = self.process.transport.client.request(command)?;
        self.pending.insert(
            id,
            if control.is_some() {
                Pending::Command(self.epoch)
            } else {
                Pending::Prompt(self.epoch)
            },
        );
        Ok(())
    }
    fn prompt(&mut self, text: String, images: Value) -> Result<(), HarnessError> {
        self.epoch += 1;
        self.active = true;
        self.norm.reset();
        self.submit(text, images, false)
    }
    async fn bootstrap(&mut self, backlog: &mut Vec<Value>) -> Result<Value, HarnessError> {
        if let Some(model) = self.request.model.as_deref().filter(|s| *s != "default") {
            let models = self
                .process
                .query(json!({"type":"get_available_models"}), backlog)
                .await?;
            let options = models["models"]
                .as_array()
                .ok_or_else(|| HarnessError::Protocol("Pi returned no models".into()))?;
            let found = options
                .iter()
                .find(|m| format!("{}/{}", string(m, "provider"), string(m, "id")) == model)
                .or_else(|| {
                    let mut matches = options.iter().filter(|m| string(m, "id") == model);
                    let first = matches.next()?;
                    matches.next().is_none().then_some(first)
                })
                .ok_or_else(|| {
                    HarnessError::Protocol(format!("Pi model is not available: {model}"))
                })?;
            self.process
                .query(
                    json!({"type":"set_model","provider":found["provider"],"modelId":found["id"]}),
                    backlog,
                )
                .await?;
        }
        if self.request.reasoning.is_some()
            || self
                .request
                .model_options
                .get("pi_thinking")
                .is_some_and(|v| v == "off")
        {
            let supported = self
                .process
                .query(json!({"type":"get_available_thinking_levels"}), backlog)
                .await?;
            let levels = supported["levels"].as_array().cloned().unwrap_or_default();
            let desired = if self
                .request
                .model_options
                .get("pi_thinking")
                .is_some_and(|v| v == "off")
            {
                "off".to_string()
            } else {
                serde_json::to_value(self.request.reasoning)
                    .unwrap()
                    .as_str()
                    .unwrap_or("medium")
                    .to_owned()
            };
            let order = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];
            let rank = order
                .iter()
                .position(|s| *s == desired)
                .unwrap_or(order.len() - 1);
            let effective = order[..=rank]
                .iter()
                .rev()
                .find(|s| levels.iter().any(|v| v == **s))
                .ok_or_else(|| {
                    HarnessError::Protocol("Pi returned no supported thinking level".into())
                })?;
            self.process
                .query(
                    json!({"type":"set_thinking_level","level":effective}),
                    backlog,
                )
                .await?;
        }
        let commands = self
            .process
            .query(json!({"type":"get_commands"}), backlog)
            .await?;
        self.extension_commands = commands["commands"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|c| c["source"] == "extension")
            .map(|c| string(c, "name").to_owned())
            .collect();
        self.emit(AgentEvent::AvailableCommands {
            commands: catalog::commands(&commands),
        })
        .await?;
        self.process
            .query(json!({"type":"get_state"}), backlog)
            .await
    }
    async fn run(
        &mut self,
        steering: &mut mpsc::Receiver<crate::SteerMessage>,
    ) -> Result<(), HarnessError> {
        let mut backlog = vec![];
        let state = tokio::time::timeout(Duration::from_secs(60), self.bootstrap(&mut backlog))
            .await
            .map_err(|_| HarnessError::Protocol("Pi startup timed out".into()))??;
        self.session = string(&state, "sessionId").into();
        if self.session.is_empty() {
            return Err(HarnessError::Protocol(
                "Pi get_state omitted sessionId".into(),
            ));
        }
        if self
            .request
            .resume
            .as_ref()
            .is_some_and(|id| id != &self.session)
        {
            return Err(HarnessError::Protocol(
                "Pi resumed a different session".into(),
            ));
        }
        if let Some(file) = state["sessionFile"].as_str() {
            self.store.remember(&self.session, Path::new(file))?;
        }
        self.auto_compaction = state["autoCompactionEnabled"].as_bool().unwrap_or(true);
        self.norm.window = state["model"]["contextWindow"].as_u64();
        self.started(state["model"]["id"].as_str().unwrap_or("default").into())
            .await?;
        for frame in backlog {
            self.frame(frame).await?;
        }
        let images = load_images(&self.request.attachments).await?;
        self.prompt(self.request.prompt.clone(), images)?;
        let mut open = true;
        loop {
            if self.delivery.is_none()
                && !self.initial_pending
                && !self
                    .pending
                    .values()
                    .any(|p| matches!(p, Pending::Prompt(_) | Pending::Command(_)))
            {
                if !self
                    .queued
                    .front()
                    .is_some_and(|s| self.active && self.control_command(&s.prompt).is_some())
                {
                    if let Some(steer) = self.queued.pop_front() {
                        if !self.active {
                            self.norm.reset();
                        }
                        self.epoch += 1;
                        self.active = true;
                        // Atomic Pi operation: queue at a step boundary if busy, start if idle.
                        // A separate get_state + steer pair would strand an input on the idle race.
                        self.submit(steer.prompt.clone(), json!([]), true)?;
                        self.delivery = Some(steer);
                    }
                }
            }
            if !self.active && !open && self.queued.is_empty() {
                return Ok(());
            }
            tokio::select! {
                incoming=self.process.transport.incoming.recv()=>{
                    match incoming {Some(frame)=>self.frame(frame?).await?,None=>return Err(HarnessError::Protocol("Pi disconnected".into()))}
                }
                steer=steering.recv(),if open=>match steer {Some(steer)=>self.queued.push_back(steer),None=>open=false}
            }
        }
    }
    async fn confirm_delivery(&mut self) -> Result<(), HarnessError> {
        if self.delivery.take().is_some() {
            let old = std::mem::replace(&mut self.assistant, uuid::Uuid::new_v4().to_string());
            self.emit(AgentEvent::Steered {
                assistant_message_id: Some(old),
                next_assistant_message_id: Some(self.assistant.clone()),
            })
            .await?;
        }
        Ok(())
    }
    async fn interrupt(&mut self, grace: Duration) {
        self.interrupted = true;
        self.process.dialogs.cancel();
        self.queued.clear();
        self.delivery = None;
        if !self.active {
            return;
        }
        let _ = self
            .process
            .transport
            .client
            .request(json!({"type":"clear_queue"}));
        let Ok(abort) = self
            .process
            .transport
            .client
            .request(json!({"type":"abort"}))
        else {
            return;
        };
        let _ = tokio::time::timeout(grace, async {
            while let Some(Ok(frame)) = self.process.transport.incoming.recv().await {
                if frame["id"] == abort && frame["success"] == true {
                    break;
                }
                if self.frame(frame).await.is_err() {
                    break;
                }
            }
        })
        .await;
    }
    async fn frame(&mut self, frame: Value) -> Result<(), HarnessError> {
        if frame["type"] == "extension_ui_request" {
            self.process
                .dialogs
                .request(self.process.transport.client.clone(), &frame);
            if frame["method"] == "notify" {
                self.emit(AgentEvent::TextDelta {
                    text: format!(
                        "
{}
",
                        string(&frame, "message")
                    ),
                })
                .await?;
            }
            return Ok(());
        }
        if frame["type"] == "response" {
            let Some(pending) = self.pending.remove(string(&frame, "id")) else {
                return Ok(());
            };
            let data = response_data(frame)?;
            if let Pending::Command(epoch) = pending {
                if epoch == self.epoch {
                    self.initial_pending = false;
                    self.confirm_delivery().await?;
                    let text = if data.is_null() {
                        "Pi command completed.".into()
                    } else {
                        serde_json::to_string_pretty(&data).unwrap()
                    };
                    self.emit(AgentEvent::TextDelta { text }).await?;
                    let id = self
                        .process
                        .transport
                        .client
                        .request(json!({"type":"get_state"}))?;
                    self.pending.insert(id, Pending::Barrier(epoch));
                }
                return Ok(());
            }
            match pending {
                Pending::Prompt(epoch) if epoch == self.epoch && self.active => {
                    let id = self
                        .process
                        .transport
                        .client
                        .request(json!({"type":"get_state"}))?;
                    self.pending.insert(id, Pending::Barrier(epoch));
                }
                Pending::Barrier(epoch) if epoch == self.epoch && self.active => {
                    self.auto_compaction = data["autoCompactionEnabled"]
                        .as_bool()
                        .unwrap_or(self.auto_compaction);
                    self.norm.window = data["model"]["contextWindow"].as_u64().or(self.norm.window);
                    if data["isStreaming"] == false
                        && data["isCompacting"] == false
                        && !self
                            .pending
                            .values()
                            .any(|p| matches!(p, Pending::Prompt(_) | Pending::Command(_)))
                    {
                        self.initial_pending = false;
                        if !self.interrupted {
                            self.confirm_delivery().await?;
                        }
                        self.finish().await?;
                    }
                }
                _ => {}
            }
            return Ok(());
        }
        if frame["type"] == "agent_start" && !self.active {
            self.epoch += 1;
            self.active = true;
            self.norm.reset();
            self.assistant = uuid::Uuid::new_v4().to_string();
            self.started(
                self.request
                    .model
                    .clone()
                    .unwrap_or_else(|| "default".into()),
            )
            .await?;
        }
        if frame["type"] == "message_start" && frame["message"]["role"] == "user" {
            if self.initial_pending {
                self.initial_pending = false;
            } else {
                self.confirm_delivery().await?;
            }
        }
        for event in self.norm.map(&frame) {
            self.emit(event).await?;
        }
        if frame["type"] == "agent_settled" {
            let id = self
                .process
                .transport
                .client
                .request(json!({"type":"get_state"}))?;
            self.pending.insert(id, Pending::Barrier(self.epoch));
        }
        Ok(())
    }
}
async fn load_images(paths: &[String]) -> Result<Value, HarnessError> {
    use base64::Engine;
    let mut images = vec![];
    for path in paths {
        let meta = tokio::fs::metadata(path).await?;
        if meta.len() > 20 * 1024 * 1024 {
            return Err(HarnessError::Protocol(format!(
                "Pi image exceeds 20 MiB: {path}"
            )));
        }
        let bytes = tokio::fs::read(path).await?;
        let mime = if bytes.starts_with(b"\x89PNG") {
            "image/png"
        } else if bytes.starts_with(b"\xff\xd8\xff") {
            "image/jpeg"
        } else if bytes.starts_with(b"GIF8") {
            "image/gif"
        } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
            "image/webp"
        } else {
            return Err(HarnessError::Protocol(format!(
                "Unsupported Pi image: {path}"
            )));
        };
        images.push(json!({"type":"image","data":base64::engine::general_purpose::STANDARD.encode(bytes),"mimeType":mime}));
    }
    Ok(Value::Array(images))
}
