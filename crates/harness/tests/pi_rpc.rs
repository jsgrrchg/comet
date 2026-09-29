#![cfg(feature = "native-fixture")]
use futures::StreamExt;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use zeron_harness::{CancellationToken, Harness, PiHarness, RunControls, SteerMessage};
use zeron_proto::{AgentEvent, DoneStatus, RunRequest, SandboxLevel};
fn harness() -> PiHarness {
    PiHarness::new()
        .with_executable(env!("CARGO_BIN_EXE_harness-pi-fixture"))
        .with_graces(Duration::from_millis(100), Duration::from_millis(100))
}
fn request(cwd: &std::path::Path, prompt: &str) -> RunRequest {
    RunRequest {
        prompt: prompt.into(),
        harness: None,
        model: None,
        reasoning: None,
        model_options: Default::default(),
        cwd: cwd.display().to_string(),
        sandbox: SandboxLevel::WorkspaceWrite,
        auto_approve: true,
        resume: None,
        attachments: vec![],
        worktree: None,
        mcp: None,
    }
}
fn controls() -> (RunControls, mpsc::Sender<SteerMessage>, CancellationToken) {
    let (tx, rx) = mpsc::channel(8);
    let token = CancellationToken::new();
    (
        RunControls {
            execution_lease: None,
            steering: rx,
            interrupt: token.clone(),
            request_input: Box::new(|_| {
                let (tx, rx) = oneshot::channel();
                let _ = tx.send(vec![]);
                rx
            }),
        },
        tx,
        token,
    )
}
async fn collect(prompt: &str) -> Vec<AgentEvent> {
    let dir = tempfile::tempdir().unwrap();
    let (c, tx, _) = controls();
    drop(tx);
    let stream = harness()
        .with_session_store(dir.path().join("index"))
        .run(request(dir.path(), prompt), c)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), stream.map(Result::unwrap).collect())
        .await
        .unwrap()
}
#[tokio::test]
async fn terminal_contract_handles_normal_errors_retries_compaction_and_consumed_prompts() {
    for (prompt, status) in [
        ("hello", DoneStatus::Completed),
        ("error", DoneStatus::Errored),
        ("reject", DoneStatus::Errored),
        ("retry", DoneStatus::Completed),
        ("compact", DoneStatus::Completed),
        ("/noop", DoneStatus::Completed),
        ("handled", DoneStatus::Completed),
        ("crash", DoneStatus::Errored),
    ] {
        let events = collect(prompt).await;
        let dones: Vec<_> = events
            .iter()
            .filter_map(|e| {
                if let AgentEvent::Done { status, .. } = e {
                    Some(status)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(dones, vec![&status], "{prompt}: {events:?}");
        if prompt == "hello" {
            let text: String = events
                .iter()
                .filter_map(|e| {
                    if let AgentEvent::TextDelta { text } = e {
                        Some(text.as_str())
                    } else {
                        None
                    }
                })
                .collect();
            assert_eq!(text, "reply:hello");
        }
    }
}

#[tokio::test]
async fn resumes_the_same_session_after_process_shutdown() {
    let dir = tempfile::tempdir().unwrap();
    let h = harness().with_session_store(dir.path().join("index"));
    let mut id = None;
    for text in ["first", "second"] {
        let (c, tx, _) = controls();
        drop(tx);
        let mut req = request(dir.path(), text);
        req.resume = id.clone();
        let events: Vec<_> = tokio::time::timeout(
            Duration::from_secs(5),
            h.run(req, c).await.unwrap().collect(),
        )
        .await
        .unwrap();
        for event in events {
            if let AgentEvent::Done {
                session_id, status, ..
            } = event.unwrap()
            {
                assert_eq!(status, DoneStatus::Completed);
                if id.is_some() {
                    assert_eq!(id, session_id);
                }
                id = session_id;
            }
        }
    }
    assert!(id.is_some());
}

#[tokio::test]
async fn steers_confirm_on_consumption_and_interrupt_is_terminal_once() {
    let dir = tempfile::tempdir().unwrap();
    let (c, tx, token) = controls();
    let mut stream = harness()
        .with_session_store(dir.path().join("index"))
        .run(request(dir.path(), "slow"), c)
        .await
        .unwrap();
    tx.send(SteerMessage {
        prompt: "redirect".into(),
        message_id: Some("user-id".into()),
    })
    .await
    .unwrap();
    let mut confirmed = 0;
    let mut done = vec![];
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(event) = stream.next().await {
            match event.unwrap() {
                AgentEvent::Steered {
                    next_assistant_message_id,
                    ..
                } => {
                    assert_ne!(next_assistant_message_id.as_deref(), Some("user-id"));
                    confirmed += 1;
                    token.cancel();
                }
                AgentEvent::Done { status, .. } => done.push(status),
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(confirmed, 1);
    assert_eq!(done, vec![DoneStatus::Interrupted]);
}
#[tokio::test]
async fn idle_mailbox_starts_another_turn_without_restarting_process() {
    let dir = tempfile::tempdir().unwrap();
    let (c, tx, _) = controls();
    let mut stream = harness()
        .with_session_store(dir.path().join("index"))
        .run(request(dir.path(), "first"), c)
        .await
        .unwrap();
    let mut sender = Some(tx);
    let mut done = 0;
    let mut confirmed = 0;
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(event) = stream.next().await {
            match event.unwrap() {
                AgentEvent::Done { status, .. } => {
                    assert_eq!(status, DoneStatus::Completed);
                    done += 1;
                    if let Some(tx) = sender.take() {
                        tx.send(SteerMessage {
                            prompt: "second".into(),
                            message_id: None,
                        })
                        .await
                        .unwrap();
                    }
                }
                AgentEvent::Steered { .. } => confirmed += 1,
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(done, 2);
    assert_eq!(confirmed, 1);
}
