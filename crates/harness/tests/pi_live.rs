//! Real native Pi, isolated settings, and a local provider (no network/API spend).
#![cfg(unix)]
use futures::StreamExt;
use std::{os::unix::fs::PermissionsExt, time::Duration};
use tokio::sync::{mpsc, oneshot};
use zeron_harness::{CancellationToken, Harness, PiHarness, RunControls, SteerMessage};
use zeron_proto::{AgentEvent, DoneStatus, RunRequest, SandboxLevel, UserInputAnswer};

#[tokio::test]
#[ignore = "requires Pi >= 0.85.1 installed; uses only a local mock provider"]
async fn real_pi_mock_lifecycle() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path();
    let agent = cwd.join("agent");
    std::fs::create_dir_all(&agent).unwrap();
    std::fs::write(
        agent.join("settings.json"),
        r#"{"retry":{"enabled":false}}"#,
    )
    .unwrap();
    let extensions = agent.join("extensions");
    std::fs::create_dir_all(&extensions).unwrap();
    std::fs::write(
        extensions.join("probe.ts"),
        include_str!("fixtures/pi-rpc-probe.ts"),
    )
    .unwrap();
    let exe = PiHarness::new()
        .resolve_executable()
        .expect("Pi CLI installed");
    let quote =
        |p: &std::path::Path| format!("'{}'", p.display().to_string().replace('\'', "'\\''"));
    let wrapper = cwd.join("pi-probe");
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\nexport PI_CODING_AGENT_DIR={}\nexec {} \"$@\"\n",
            quote(&agent),
            quote(&exe)
        ),
    )
    .unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
    let harness = PiHarness::new()
        .with_executable(wrapper)
        .with_session_store(cwd.join("index"));
    let mut session = None;
    for (prompt, expected) in [
        ("/probe-noop", DoneStatus::Completed),
        ("hello", DoneStatus::Completed),
        ("/probe-new", DoneStatus::Completed),
        ("resume", DoneStatus::Completed),
        ("/probe-noop", DoneStatus::Completed),
        ("/probe-input", DoneStatus::Completed),
        ("error", DoneStatus::Errored),
        ("slow", DoneStatus::Interrupted),
        ("steering slow", DoneStatus::Completed),
        ("/probe-new", DoneStatus::Completed),
        ("/probe-metadata", DoneStatus::Completed),
    ] {
        let (steer, steering) = mpsc::channel(8);
        let interrupt = CancellationToken::new();
        let controls = RunControls {
            execution_lease: None,
            steering,
            interrupt: interrupt.clone(),
            request_input: Box::new(|questions| {
                let (tx, rx) = oneshot::channel();
                tx.send(vec![UserInputAnswer {
                    question_id: questions[0].id.clone(),
                    labels: vec!["local answer".into()],
                }])
                .unwrap();
                rx
            }),
        };
        let request = RunRequest {
            prompt: prompt.into(),
            harness: None,
            model: Some("zeron-probe/mock".into()),
            reasoning: None,
            model_options: Default::default(),
            cwd: cwd.display().to_string(),
            sandbox: SandboxLevel::WorkspaceWrite,
            auto_approve: true,
            resume: session.clone(),
            attachments: vec![],
            worktree: None,
            mcp: None,
        };
        let previous_session = session.clone();
        let mut stream = harness.run(request, controls).await.unwrap();
        let mut sender = Some(steer);
        let mut done = 0;
        let mut confirmed = 0;
        let mut text = String::new();
        tokio::time::timeout(Duration::from_secs(20), async {
            while let Some(event) = stream.next().await {
                match event.unwrap() {
                    AgentEvent::SessionStarted { session_id, .. } => {
                        if let Some(old) = &session {
                            if prompt != "/probe-new" {
                                assert_eq!(old, &session_id);
                            }
                        }
                        session = Some(session_id);
                        if prompt == "steering slow" {
                            sender
                                .take()
                                .unwrap()
                                .send(SteerMessage {
                                    prompt: "redirect".into(),
                                    message_id: None,
                                })
                                .await
                                .unwrap();
                        } else {
                            sender.take();
                        }
                        if prompt == "slow" {
                            let token = interrupt.clone();
                            tokio::spawn(async move {
                                tokio::time::sleep(Duration::from_millis(150)).await;
                                token.cancel();
                            });
                        }
                    }
                    AgentEvent::TextDelta { text: delta } => text.push_str(&delta),
                    AgentEvent::Steered { .. } => confirmed += 1,
                    AgentEvent::Done {
                        status,
                        error,
                        session_id,
                        ..
                    } => {
                        assert_eq!(
                            session_id, session,
                            "Done must publish the current native session identity"
                        );
                        assert_eq!(status, expected, "{prompt}: {error:?}");
                        done += 1;
                    }
                    _ => {}
                }
            }
        })
        .await
        .expect("native Pi run must settle");
        assert_eq!(done, 1, "{prompt}: {text}");
        if prompt == "/probe-new" {
            assert_ne!(session, previous_session);
        }
        if prompt == "/probe-input" {
            assert!(text.contains("answer:local answer"), "{text}");
        }
        if prompt == "steering slow" {
            assert_eq!(confirmed, 1);
            assert!(text.contains("MOCK:redirect"), "{text}");
        }
        if matches!(prompt, "hello" | "resume") {
            assert_eq!(text, format!("MOCK:{prompt}"));
        }
    }
    // Unsaved extension entries are not equivalent to an empty conversation.
    // Pi has no public RPC to restore them; refuse instead of losing them.
    let (_, steering) = mpsc::channel(1);
    let controls = RunControls {
        execution_lease: None,
        steering,
        interrupt: CancellationToken::new(),
        request_input: Box::new(|_| oneshot::channel().1),
    };
    let request = RunRequest {
        prompt: "must not run".into(),
        harness: None,
        model: Some("zeron-probe/mock".into()),
        reasoning: None,
        model_options: Default::default(),
        cwd: cwd.display().to_string(),
        sandbox: SandboxLevel::WorkspaceWrite,
        auto_approve: true,
        resume: session,
        attachments: vec![],
        worktree: None,
        mcp: None,
    };
    assert!(
        harness.run(request, controls).await.is_err(),
        "unpersisted extension state must not silently disappear"
    );
}
