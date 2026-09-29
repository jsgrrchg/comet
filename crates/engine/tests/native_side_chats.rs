use async_trait::async_trait;
use futures::{StreamExt, stream::BoxStream};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use zeron_doc::*;
use zeron_engine::{EngineCore, HarnessRegistry};
use zeron_harness::{Harness, HarnessError, NativeForkControls, NativeForkError, RunControls};
use zeron_proto::*;
use zeron_rpc::methods;

#[derive(Default)]
struct NativeStore {
    forks: AtomicUsize,
    sessions: Mutex<std::collections::HashMap<String, Vec<String>>>,
    requests: Mutex<Vec<RunRequest>>,
    ambiguous: bool,
}
#[async_trait]
impl Harness for NativeStore {
    fn id(&self) -> HarnessId {
        HarnessId::Mock
    }
    fn display_name(&self) -> &str {
        "Native store"
    }
    fn supports_steering(&self) -> bool {
        false
    }
    fn steering_mode(&self) -> SteeringMode {
        SteeringMode::TurnBoundary
    }
    fn reasoning_levels(&self) -> &[ReasoningLevel] {
        &[]
    }
    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        Ok(vec![])
    }
    async fn native_fork_support(&self, _: &std::path::Path) -> NativeForkAvailability {
        NativeForkAvailability::available()
    }
    async fn fork_native(
        &self,
        point: &NativeForkPoint,
        _: NativeForkControls,
    ) -> Result<NativeForkResult, NativeForkError> {
        self.forks.fetch_add(1, Ordering::SeqCst);
        if self.ambiguous {
            return Err(NativeForkError::Indeterminate("lost provider reply".into()));
        }
        assert_eq!(point.source_session_id, "canonical");
        let NativeForkBoundary::AppServerTurn { turn_id } = &point.boundary else {
            panic!()
        };
        assert_eq!(turn_id, "a1");
        let id = format!("native-child-{}", self.forks.load(Ordering::SeqCst));
        self.sessions
            .lock()
            .unwrap()
            .insert(id.clone(), vec!["U1".into(), "A1".into()]);
        Ok(NativeForkResult {
            session_id: id,
            cwd: point.cwd.clone(),
        })
    }
    async fn run(
        &self,
        request: RunRequest,
        _: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        let session_id = request.resume.clone();
        self.requests.lock().unwrap().push(request);
        Ok(futures::stream::iter(vec![
            Ok(AgentEvent::TextDelta {
                text: "child answer".into(),
            }),
            Ok(AgentEvent::Done {
                status: DoneStatus::Completed,
                result: None,
                error: None,
                session_id,
            }),
        ])
        .boxed())
    }
}
fn setup(dir: &std::path::Path, harness: Arc<NativeStore>) -> EngineCore {
    let registry = Arc::new(HarnessRegistry::new());
    registry.register(harness);
    let core = EngineCore::assemble(dir, registry, HarnessId::Mock, None).unwrap();
    core.workspace
        .create_chat(
            "main",
            None,
            Some(&core.device_id),
            None,
            Some("/tmp".into()),
        )
        .unwrap();
    core.workspace
        .set_chat_harness_session("main", "canonical", "/tmp");
    let doc = core.doc_host.open("main").unwrap();
    for (id, role) in [
        ("u1", MessageRole::User),
        ("a1", MessageRole::Assistant),
        ("u2", MessageRole::User),
        ("a2", MessageRole::Assistant),
    ] {
        doc.doc()
            .push_message(&SessionMessageEntry {
                id: id.into(),
                role,
                parts: vec![MessagePart::Text {
                    id: format!("text-{id}"),
                    text: id.to_uppercase(),
                }],
                created_at: 1,
                device_id: core.device_id.clone(),
                status: Some(MessageStatus::Complete),
                continuation_of: None,
                duration_ms: None,
                native_fork_point: None,
            })
            .unwrap();
    }
    doc.doc()
        .set_native_fork_point(
            "a1",
            &NativeForkPoint {
                format_version: 1,
                harness: HarnessId::Mock,
                source_device_id: core.device_id.clone(),
                source_session_id: "canonical".into(),
                cwd: "/tmp".into(),
                boundary: NativeForkBoundary::AppServerTurn {
                    turn_id: "a1".into(),
                },
            },
        )
        .unwrap();
    core
}
fn request(core: &EngineCore) -> ForkMessageSideChatRequest {
    ForkMessageSideChatRequest {
        request_id: "op-one".into(),
        chat_id: "child".into(),
        source_chat_id: "main".into(),
        source_message_id: "a1".into(),
        parent_chat_id: None,
        target_device_id: core.device_id.clone(),
    }
}
#[tokio::test]
async fn native_fork_rpc_freezes_exact_prefix_dedupes_and_preserves_canonical_lineage() {
    let dir = tempfile::tempdir().unwrap();
    let harness = Arc::new(NativeStore::default());
    let core = setup(dir.path(), harness.clone());
    let client = zeron_rpc::memory_client(core.rpc_service());
    let params = serde_json::to_value(request(&core)).unwrap();
    let (a, b) = tokio::join!(
        client.call_as::<Chat>(methods::FORK_MESSAGE_SIDE_CHAT, params.clone()),
        client.call_as::<Chat>(methods::FORK_MESSAGE_SIDE_CHAT, params.clone())
    );
    let child = a.unwrap();
    assert_eq!(child, b.unwrap());
    assert_eq!(harness.forks.load(Ordering::SeqCst), 1);
    let entries = core
        .doc_host
        .open("child")
        .unwrap()
        .doc()
        .read_entries()
        .unwrap();
    assert_eq!(
        entries.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(),
        ["u1", "a1", "fork:child"]
    );
    assert!(harness.requests.lock().unwrap().is_empty());
    assert_eq!(
        harness.sessions.lock().unwrap()[child.harness_session_id.as_ref().unwrap()],
        ["U1", "A1"]
    );
    let mut changed = params.clone();
    changed["sourceMessageId"] = serde_json::json!("a2");
    assert!(
        client
            .call(methods::FORK_MESSAGE_SIDE_CHAT, changed)
            .await
            .is_err()
    );
    let mut inherited = request(&core);
    inherited.request_id = "op-two".into();
    inherited.chat_id = "sibling".into();
    inherited.source_chat_id = "child".into();
    let sibling = core.native_forks.create(inherited).await.unwrap();
    assert_eq!(sibling.parent_chat_id.as_deref(), Some("main"));
    assert_eq!(
        core.doc_host
            .open("sibling")
            .unwrap()
            .doc()
            .read_entries()
            .unwrap()[1]
            .native_fork_point,
        entries[1].native_fork_point
    );
    assert_eq!(
        core.doc_host
            .open("main")
            .unwrap()
            .doc()
            .read_entries()
            .unwrap()
            .len(),
        4
    );
    core.shutdown().await;
    drop(client);
    drop(core);
    let registry = Arc::new(HarnessRegistry::new());
    registry.register(harness.clone());
    let restarted = EngineCore::assemble(dir.path(), registry, HarnessId::Mock, None).unwrap();
    let replay = zeron_rpc::memory_client(restarted.rpc_service())
        .call_as::<Chat>(methods::FORK_MESSAGE_SIDE_CHAT, params)
        .await
        .unwrap();
    assert_eq!(replay, child);
    assert_eq!(harness.forks.load(Ordering::SeqCst), 2);
    restarted.shutdown().await;
}
#[tokio::test]
async fn native_fork_indeterminate_never_retries_or_publishes() {
    let dir = tempfile::tempdir().unwrap();
    let harness = Arc::new(NativeStore {
        ambiguous: true,
        ..Default::default()
    });
    let core = setup(dir.path(), harness.clone());
    let request = request(&core);
    for _ in 0..2 {
        assert!(
            core.native_forks
                .create(request.clone())
                .await
                .unwrap_err()
                .contains("indeterminate")
        );
    }
    assert_eq!(harness.forks.load(Ordering::SeqCst), 1);
    assert!(core.workspace.chat("child").unwrap().is_none());
    core.shutdown().await;
}
#[tokio::test]
async fn native_fork_availability_and_host_validation_fail_closed() {
    let dir = tempfile::tempdir().unwrap();
    let harness = Arc::new(NativeStore::default());
    let core = setup(dir.path(), harness.clone());
    let availability = core
        .native_forks
        .availability(NativeForkAvailabilityRequest {
            source_chat_id: "main".into(),
            message_ids: vec!["a1".into(), "a2".into(), "u1".into()],
            target_device_id: core.device_id.clone(),
        })
        .await
        .unwrap();
    assert!(availability["a1"].available);
    assert!(!availability["a2"].available);
    assert!(!availability["u1"].available);
    let mut bad = request(&core);
    bad.target_device_id = "foreign".into();
    assert!(core.native_forks.create(bad).await.is_err());
    let mut bad = request(&core);
    bad.parent_chat_id = Some("unrelated".into());
    assert!(core.native_forks.create(bad).await.is_err());
    assert_eq!(harness.forks.load(Ordering::SeqCst), 0);
    core.shutdown().await;
}

#[tokio::test]
async fn native_fork_provider_created_recovery_reuses_the_durable_native_id() {
    let dir = tempfile::tempdir().unwrap();
    let harness = Arc::new(NativeStore::default());
    let core = setup(dir.path(), harness.clone());
    let request = request(&core);
    let child = core.native_forks.create(request.clone()).await.unwrap();
    core.shutdown().await;
    drop(core);
    fn records(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut out = vec![];
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                out.extend(records(&path));
            } else if path.parent().unwrap().file_name().unwrap() == "native-forks"
                && path.extension().is_some_and(|s| s == "json")
            {
                out.push(path);
            }
        }
        out
    }
    let path = records(dir.path()).pop().unwrap();
    let mut value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    value["phase"] = serde_json::json!("ProviderCreated");
    std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    let registry = Arc::new(HarnessRegistry::new());
    registry.register(harness.clone());
    let restarted = EngineCore::assemble(dir.path(), registry, HarnessId::Mock, None).unwrap();
    assert_eq!(restarted.native_forks.create(request).await.unwrap(), child);
    assert_eq!(harness.forks.load(Ordering::SeqCst), 1);
    assert_eq!(
        restarted
            .doc_host
            .open("child")
            .unwrap()
            .doc()
            .read_entries()
            .unwrap()
            .len(),
        3
    );
    let value: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert_eq!(value["phase"], "Published");
    restarted.shutdown().await;
}

fn send_request(prompt: &str) -> RunRequest {
    serde_json::from_value(serde_json::json!({
        "prompt": prompt, "cwd": "/tmp", "sandbox": "workspace-write", "autoApprove": true,
        "resume": "untrusted-parent", "mcp": {"name":"zeron", "command":"parent", "env":{"ZERON_CHAT_ID":"main"}}
    })).unwrap()
}

async fn send_native(core: &EngineCore, harness: &NativeStore, prompt: &str) {
    let before = harness.requests.lock().unwrap().len();
    core.sessions
        .dispatch("child", HarnessId::Mock, send_request(prompt), None)
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while harness.requests.lock().unwrap().len() == before
            || core
                .sessions
                .session_status("child")
                .is_some_and(|s| s.status != SessionStatus::Idle)
        {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let captured = harness.requests.lock().unwrap()[before].clone();
    assert_eq!(captured.resume.as_deref(), Some("native-child-1"));
    assert_eq!(captured.resume_policy, ResumePolicy::RequireExisting);
    assert_eq!(captured.prompt, prompt);
    assert_eq!(captured.mcp.unwrap().env["ZERON_CHAT_ID"], "child");
}

#[tokio::test]
async fn native_fork_resume_survives_restart_and_never_wraps_history() {
    let dir = tempfile::tempdir().unwrap();
    let harness = Arc::new(NativeStore::default());
    let core = setup(dir.path(), harness.clone());
    core.native_forks.create(request(&core)).await.unwrap();
    core.shutdown().await;
    drop(core);
    for prompts in [["/review", "What do you remember?"], ["Continue", "Again"]] {
        let registry = Arc::new(HarnessRegistry::new());
        registry.register(harness.clone());
        let core = EngineCore::assemble(dir.path(), registry, HarnessId::Mock, None).unwrap();
        core.sessions.set_ipc_port(27699);
        for prompt in prompts {
            send_native(&core, &harness, prompt).await;
        }
        let mut changed = send_request("wrong checkout");
        changed.cwd = "/".into();
        assert!(
            core.sessions
                .dispatch("child", HarnessId::Mock, changed, None)
                .await
                .is_err()
        );
        assert!(
            core.sessions
                .dispatch(
                    "child",
                    HarnessId::Codex,
                    send_request("wrong harness"),
                    None
                )
                .await
                .is_err()
        );
        assert!(
            core.doc_host
                .open("child")
                .unwrap()
                .doc()
                .native_fork_lineage()
                .unwrap()
                .is_some()
        );
        core.shutdown().await;
    }
    assert_eq!(harness.requests.lock().unwrap().len(), 4);
    assert_eq!(harness.forks.load(Ordering::SeqCst), 1);
}
