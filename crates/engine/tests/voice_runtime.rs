//! Offline boundary tests: rejected voice must not launch a harness or task.
use serde_json::json;
use zeron_engine::{EngineCore, EngineProfile, HarnessRegistry};
use zeron_proto::{
    HarnessId,
    voice::{VoiceEligibility, VoiceRejection},
};
use zeron_rpc::{RpcClient, methods};

async fn rejects_without_launching(client: &RpcClient, device: &str) {
    let request = json!({"chatId":"voice-chat", "hostDeviceId":device});
    let eligibility: VoiceEligibility = client
        .call_as(methods::VOICE_ELIGIBILITY, request.clone())
        .await
        .unwrap();
    assert!(!eligibility.available);
    // The registry has no Codex: the reason asks for an install.
    assert_eq!(
        eligibility.reason,
        Some(VoiceRejection::NativeRuntimeUnavailable)
    );
    assert!(!eligibility.credits_excluded);
    assert_eq!(eligibility.ordinary_usage_allowed, None);
    assert!(
        client
            .call(methods::START_VOICE, request.clone())
            .await
            .is_err()
    );
    assert!(
        client
            .call(
                methods::START_VOICE,
                json!({"chatId":"voice-chat", "hostDeviceId":device, "apiKey":"invalid"})
            )
            .await
            .is_err()
    );
    let remote: VoiceEligibility = client
        .call_as(
            methods::VOICE_ELIGIBILITY,
            json!({"chatId":"voice-chat", "hostDeviceId":"other-device"}),
        )
        .await
        .unwrap();
    assert_eq!(remote.reason, Some(VoiceRejection::RemoteHost));
    assert!(client.call(methods::START_VOICE, json!({"chatId":"voice-chat", "hostDeviceId":device, "targetDeviceId":"other-device"})).await.is_err());
}

#[tokio::test]
async fn voice_billing_gate_is_identical_in_memory_and_loopback() {
    let temp = tempfile::tempdir().unwrap();
    // Empty registry: even an accidentally requested harness cannot be started.
    let core = EngineCore::assemble_with_profile(
        EngineProfile::local(temp.path()).unwrap(),
        std::sync::Arc::new(HarnessRegistry::new()),
        HarnessId::Codex,
        None,
    )
    .unwrap();
    let config = serde_json::from_value(
        json!({"harness":"codex", "model":null, "reasoning":null, "sandbox":"danger-full-access"}),
    )
    .unwrap();
    core.workspace
        .create_chat(
            "voice-chat",
            None,
            Some(&core.device_id),
            Some(config),
            None,
        )
        .unwrap();
    rejects_without_launching(
        &zeron_rpc::memory_client(core.rpc_service()),
        &core.device_id,
    )
    .await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(zeron_rpc::serve_ws_listener(listener, core.rpc_service()));
    let client = zeron_rpc::connect_ws(&format!("ws://{address}"))
        .await
        .unwrap();
    rejects_without_launching(&client, &core.device_id).await;
    assert!(core.sessions.last_request("voice-chat").is_none());
    assert!(core.sessions.watch_sessions().borrow().is_empty());
    server.abort();
    core.sessions.shutdown().await;
}

#[cfg(unix)]
fn native_package(root: &std::path::Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let fixture =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../harness/tests/fixtures");
    let binary = root.join("bin/codex");
    let helper = root.join("codex-resources/voice/bin/codex-voice-host");
    std::fs::create_dir_all(binary.parent().unwrap()).unwrap();
    std::fs::create_dir_all(helper.parent().unwrap()).unwrap();
    std::fs::write(
        root.join("codex-package.json"),
        r#"{"layoutVersion":1,"version":"0.159.0"}"#,
    )
    .unwrap();
    for (source, target) in [
        ("fake-codex-voice-native.py", &binary),
        ("fake-codex-voice-host.py", &helper),
    ] {
        std::fs::copy(fixture.join(source), target).unwrap();
        std::fs::set_permissions(target, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    binary
}
#[cfg(unix)]
async fn native_core(temp: &tempfile::TempDir) -> EngineCore {
    let package = temp.path().join("codex-package");
    let binary = native_package(&package);
    let registry = std::sync::Arc::new(HarnessRegistry::new());
    registry.register(std::sync::Arc::new(
        zeron_harness::CodexHarness::new().with_executable(binary),
    ));
    let core = EngineCore::assemble_with_profile(
        EngineProfile::local(&temp.path().join("data")).unwrap(),
        registry,
        HarnessId::Codex,
        None,
    )
    .unwrap();
    let config =
        serde_json::from_value(json!({"harness":"codex","sandbox":"danger-full-access"})).unwrap();
    core.workspace
        .create_chat(
            "native-voice",
            None,
            Some(&core.device_id),
            Some(config),
            Some(package.display().to_string()),
        )
        .unwrap();
    core
}
#[cfg(unix)]
#[tokio::test]
async fn initial_account_snapshot_does_not_invalidate_voice_eligibility() {
    let temp = tempfile::tempdir().unwrap();
    let core = native_core(&temp).await;
    let client = zeron_rpc::memory_client(core.rpc_service());
    for _ in 0..2 {
        let eligible: VoiceEligibility = client
            .call_as(
                methods::VOICE_ELIGIBILITY,
                json!({"chatId":"native-voice","hostDeviceId":core.device_id}),
            )
            .await
            .unwrap();
        assert!(
            eligible.available,
            "initial account snapshot rejected: {:?}",
            eligible.reason
        );
    }
    let package = temp.path().join("codex-package");
    let wire = std::fs::read_to_string(package.join("voice-wire.jsonl")).unwrap();
    assert!(!wire.contains("thread/realtime/start"));
    assert!(!package.join("helper-wire.jsonl").exists());
    core.sessions.shutdown().await;
}

#[cfg(unix)]
#[tokio::test]
async fn concurrent_orchestrator_starts_never_swap_configurations() {
    use zeron_proto::voice::VoiceLease;
    let temp = tempfile::tempdir().unwrap();
    let core = native_core(&temp).await;
    let client = zeron_rpc::memory_client(core.rpc_service());
    let start = |sandbox: &str| {
        json!({"chatId":"", "hostDeviceId":core.device_id,
            "config":{"harness":"codex","sandbox":sandbox}})
    };
    let (a, b) = tokio::join!(
        client.call_as::<VoiceLease>(methods::START_VOICE, start("workspace-write")),
        client.call_as::<VoiceLease>(methods::START_VOICE, start("danger-full-access")),
    );
    // One window reserves the host's orchestrator; the other is busy and
    // never rewrites the configuration the winner asked for.
    let (sandbox, lease) = match (a, b) {
        (Ok(lease), Err(_)) => ("workspace-write", lease),
        (Err(_), Ok(lease)) => ("danger-full-access", lease),
        other => panic!("expected exactly one start: {other:?}"),
    };
    let orchestrators: Vec<_> = core
        .workspace
        .read_chats()
        .unwrap()
        .into_iter()
        .filter(|c| zeron_proto::voice::is_orchestrator_chat(&c.id))
        .collect();
    assert_eq!(orchestrators.len(), 1);
    assert_eq!(
        serde_json::to_value(orchestrators[0].config.as_ref().unwrap()).unwrap()["sandbox"],
        sandbox
    );
    client
        .call(methods::STOP_VOICE, serde_json::to_value(&lease).unwrap())
        .await
        .unwrap();
    core.sessions.shutdown().await;
}

#[cfg(unix)]
#[tokio::test]
async fn native_voice_signals_audio_owner_transcripts_stop_and_restart_on_same_runtime() {
    use zeron_proto::voice::{MuteVoice, VoiceEvent, VoiceLease, VoicePhase};
    let temp = tempfile::tempdir().unwrap();
    let core = native_core(&temp).await;
    let client = zeron_rpc::memory_client(core.rpc_service());
    let request = json!({"chatId":"native-voice","hostDeviceId":core.device_id});
    let eligibility: VoiceEligibility = client
        .call_as(methods::VOICE_ELIGIBILITY, request.clone())
        .await
        .unwrap();
    assert!(eligibility.available && eligibility.native_webrtc);
    assert!(!eligibility.credits_excluded);
    assert_eq!(eligibility.voices, vec!["juniper", "ember"]);
    for iteration in 0..2 {
        let lease: VoiceLease = client
            .call_as(methods::START_VOICE, request.clone())
            .await
            .unwrap();
        let mut owner = client
            .subscribe_scoped(methods::OWN_VOICE, serde_json::to_value(&lease).unwrap())
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(10),async {
            while let Some(v)=owner.recv().await {if matches!(serde_json::from_value::<VoiceEvent>(v).unwrap(),VoiceEvent::Snapshot{snapshot} if snapshot.phase==VoicePhase::Active){return;}}
            panic!("owner ended before active");
        }).await.unwrap();
        client
            .call(
                methods::MUTE_VOICE,
                serde_json::to_value(MuteVoice {
                    lease: lease.clone(),
                    muted: true,
                })
                .unwrap(),
            )
            .await
            .unwrap();
        let handle = core.doc_host.open("native-voice").unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if handle
                    .doc()
                    .read_entries()
                    .unwrap()
                    .iter()
                    .filter(|e| e.id.starts_with("voice:"))
                    .count()
                    == (iteration + 1) * 2
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(client.call(methods::APPEND_VOICE, json!({})).await.is_err());
        client
            .call(methods::STOP_VOICE, serde_json::to_value(&lease).unwrap())
            .await
            .unwrap();
        drop(owner);
    }
    let wire = std::fs::read_to_string(temp.path().join("codex-package/voice-wire.jsonl")).unwrap();
    assert_eq!(
        wire.lines()
            .filter(|s| s.contains("\"method\": \"thread/start\""))
            .count(),
        1,
        "restart must reuse the same thread"
    );
    assert_eq!(
        wire.lines()
            .filter(|s| s.contains("\"method\": \"thread/realtime/start\""))
            .count(),
        2
    );
    assert!(!wire.contains("appendAudio"));
    assert!(!wire.contains("\"method\": \"turn/start\""));
    assert!(
        core.sessions.turn_in_flight("native-voice"),
        "ending voice must preserve native delegated work"
    );
    core.sessions.shutdown().await;
}
#[cfg(unix)]
#[tokio::test]
async fn native_voice_rejects_api_auth_before_creating_call_and_unowned_start_never_opens_devices()
{
    let temp = tempfile::tempdir().unwrap();
    let core = native_core(&temp).await;
    let client = zeron_rpc::memory_client(core.rpc_service());
    let request = json!({"chatId":"native-voice","hostDeviceId":core.device_id});
    std::fs::write(temp.path().join("codex-package/account-mode"), "apiKey").unwrap();
    let eligible: VoiceEligibility = client
        .call_as(methods::VOICE_ELIGIBILITY, request.clone())
        .await
        .unwrap();
    assert_eq!(eligible.reason, Some(VoiceRejection::ChatgptRequired));
    assert!(
        client
            .call(methods::START_VOICE, request.clone())
            .await
            .is_err()
    );
    std::fs::write(temp.path().join("codex-package/account-mode"), "chatgpt").unwrap();
    let _: zeron_proto::voice::VoiceLease =
        client.call_as(methods::START_VOICE, request).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let wire = std::fs::read_to_string(temp.path().join("codex-package/voice-wire.jsonl")).unwrap();
    assert!(!wire.contains("\"method\": \"thread/realtime/start\""));
    assert!(!temp.path().join("codex-package/helper-wire.jsonl").exists());
    core.sessions.shutdown().await;
}

#[cfg(unix)]
async fn active_owner(
    client: &RpcClient,
    device: &str,
) -> (zeron_proto::voice::VoiceLease, zeron_rpc::RpcSubscription) {
    use zeron_proto::voice::{VoiceEvent, VoicePhase};
    let lease = client
        .call_as(
            methods::START_VOICE,
            json!({"chatId":"native-voice","hostDeviceId":device}),
        )
        .await
        .unwrap();
    let mut owner = client
        .subscribe_scoped(methods::OWN_VOICE, serde_json::to_value(&lease).unwrap())
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10),async {
        while let Some(v)=owner.recv().await {if matches!(serde_json::from_value::<VoiceEvent>(v).unwrap(),VoiceEvent::Snapshot{snapshot} if snapshot.phase==VoicePhase::Active){return;}}
        panic!("voice ended before active");
    }).await.unwrap();
    (lease, owner)
}
#[cfg(target_os = "linux")]
#[tokio::test]
async fn owner_loss_kills_microphone_even_when_native_controls_are_stalled() {
    let temp = tempfile::tempdir().unwrap();
    let core = native_core(&temp).await;
    let client = zeron_rpc::memory_client(core.rpc_service());
    let (lease, owner) = active_owner(&client, &core.device_id).await;
    let package = temp.path().join("codex-package");
    std::fs::write(package.join("helper-mode"), "stall-controls").unwrap();
    let rpc = zeron_rpc::memory_client(core.rpc_service());
    let mute = tokio::spawn(async move {
        rpc.call(
            methods::MUTE_VOICE,
            serde_json::to_value(zeron_proto::voice::MuteVoice { lease, muted: true }).unwrap(),
        )
        .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while !package.join("controls-stalled").exists() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let pid = std::fs::read_to_string(package.join("helper.pid")).unwrap();
    drop(owner);
    tokio::time::timeout(std::time::Duration::from_millis(500), async {
        while std::path::Path::new(&format!("/proc/{pid}")).exists() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("native capture must stop independently of the blocked actor");
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(8), mute)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    core.sessions.shutdown().await;
}
#[cfg(unix)]
#[tokio::test]
async fn device_open_failure_releases_lease_and_preserves_text_runtime() {
    use zeron_proto::voice::{VoiceEvent, VoiceLease};
    let temp = tempfile::tempdir().unwrap();
    let core = native_core(&temp).await;
    let client = zeron_rpc::memory_client(core.rpc_service());
    std::fs::write(temp.path().join("codex-package/helper-mode"), "exit-open").unwrap();
    let request = json!({"chatId":"native-voice","hostDeviceId":core.device_id});
    let lease: VoiceLease = client
        .call_as(methods::START_VOICE, request.clone())
        .await
        .unwrap();
    let mut owner = client
        .subscribe_scoped(methods::OWN_VOICE, serde_json::to_value(&lease).unwrap())
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while let Some(v) = owner.recv().await {
            if let VoiceEvent::Closed { reason, .. } = serde_json::from_value(v).unwrap() {
                assert_eq!(reason, Some(VoiceRejection::DeviceUnavailable));
                return;
            }
        }
        panic!("missing device failure");
    })
    .await
    .unwrap();
    drop(owner);
    std::fs::remove_file(temp.path().join("codex-package/helper-mode")).unwrap();
    let (_lease, owner) = active_owner(&client, &core.device_id).await;
    drop(owner);
    assert!(core.sessions.turn_in_flight("native-voice"));
    core.sessions.shutdown().await;
}

#[cfg(unix)]
#[tokio::test]
async fn voice_delegates_to_other_providers_without_changing_voice_eligibility() {
    let temp = tempfile::tempdir().unwrap();
    let core = native_core(&temp).await;
    let client = zeron_rpc::memory_client(core.rpc_service());
    let (lease, owner) = active_owner(&client, &core.device_id).await;
    for phase in ["active", "stopped"] {
        if phase == "stopped" {
            client
                .call(methods::STOP_VOICE, serde_json::to_value(&lease).unwrap())
                .await
                .unwrap();
        }
        for harness in ["grok", "claude-code"] {
            let chat_id = format!("{phase}-{harness}");
            // Keep the old origin field to also cover older MCP clients.
            client
                .call(
                    methods::MUTATE,
                    json!({"op":"createChat", "chatId":chat_id,
                        "originChatId":"native-voice", "parentChatId":"native-voice",
                        "deviceId":core.device_id,
                        "config":{"harness":harness,"sandbox":"workspace-write"}}),
                )
                .await
                .unwrap();
            let chat = core.workspace.chat(&chat_id).unwrap().unwrap();
            assert_eq!(chat.parent_chat_id.as_deref(), Some("native-voice"));
            assert_eq!(
                serde_json::to_value(chat.config.unwrap().harness).unwrap(),
                harness
            );
            client
                .call(
                    methods::QUEUE_MESSAGE,
                    json!({"originChatId":"native-voice", "chatId":chat_id,
                        "text":"hello", "holdForTurnEnd":true}),
                )
                .await
                .unwrap();
            let queued = client
                .call(
                    methods::QUEUE_COMMAND,
                    json!({"originChatId":"native-voice", "chatId":chat_id,
                        "command":{"kind":"run", "messageId":format!("hello-{chat_id}"),
                            "request":{"prompt":"hello", "harness":harness,
                                "cwd":temp.path(), "sandbox":"workspace-write"}}}),
                )
                .await
                .unwrap();
            assert!(queued["commandId"].is_string());
            let request = json!({"chatId":chat_id,"hostDeviceId":core.device_id});
            let eligibility: VoiceEligibility = client
                .call_as(methods::VOICE_ELIGIBILITY, request.clone())
                .await
                .unwrap();
            assert_eq!(eligibility.reason, Some(VoiceRejection::WrongHarness));
            assert!(
                client
                    .call(methods::START_VOICE, request)
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("WrongHarness")
            );
        }
    }
    drop(owner);
    core.sessions.shutdown().await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn account_update_kills_stalled_native_media_at_reader_boundary() {
    let temp = tempfile::tempdir().unwrap();
    let core = native_core(&temp).await;
    let client = zeron_rpc::memory_client(core.rpc_service());
    let (lease, owner) = active_owner(&client, &core.device_id).await;
    let package = temp.path().join("codex-package");
    std::fs::write(package.join("helper-mode"), "stall-controls").unwrap();
    let rpc = zeron_rpc::memory_client(core.rpc_service());
    let mute = tokio::spawn(async move {
        rpc.call(
            methods::MUTE_VOICE,
            serde_json::to_value(zeron_proto::voice::MuteVoice { lease, muted: true }).unwrap(),
        )
        .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while !package.join("controls-stalled").exists() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let pid = std::fs::read_to_string(package.join("helper.pid")).unwrap();
    std::fs::write(package.join("identity-updated"), "changed").unwrap();
    tokio::time::timeout(std::time::Duration::from_millis(500), async {
        while std::path::Path::new(&format!("/proc/{pid}")).exists() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("auth changes must kill capture without waiting for native controls");
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(8), mute)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    drop(owner);
    core.sessions.shutdown().await;
}

#[cfg(unix)]
async fn wait_marker(path: &std::path::Path) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !path.exists() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn voice_rejects_project_backend_overrides_and_resolved_thread_provider() {
    for mode in 0..3 {
        let temp = tempfile::tempdir().unwrap();
        let core = native_core(&temp).await;
        let package = temp.path().join("codex-package");
        if mode == 0 {
            std::fs::write(
                package.join("project-config"),
                r#"{"experimental_realtime_webrtc_call_base_url":"https://unverified.example"}"#,
            )
            .unwrap();
        }
        if mode == 1 {
            std::fs::write(
                package.join("project-config"),
                r#"{"model_providers":{"openai":{"base_url":"https://unverified.example"}}}"#,
            )
            .unwrap();
        }
        if mode == 2 {
            std::fs::write(package.join("thread-provider"), "custom").unwrap();
        }
        let client = zeron_rpc::memory_client(core.rpc_service());
        let request = json!({"chatId":"native-voice","hostDeviceId":core.device_id});
        let eligible: VoiceEligibility = client
            .call_as(methods::VOICE_ELIGIBILITY, request.clone())
            .await
            .unwrap();
        assert_eq!(eligible.reason, Some(VoiceRejection::ChatgptRequired));
        assert!(client.call(methods::START_VOICE, request).await.is_err());
        let wire = std::fs::read_to_string(package.join("voice-wire.jsonl")).unwrap();
        assert!(!wire.contains("thread/realtime/start"));
        assert!(!package.join("helper-wire.jsonl").exists());
        if mode != 2 {
            assert!(
                wire.lines()
                    .any(|line| line.contains("config/read") && line.contains("cwd"))
            );
        }
        core.sessions.shutdown().await;
    }
}

#[cfg(unix)]
#[tokio::test]
async fn exhausted_ordinary_quota_with_allowed_credits_uses_native_backend_decision() {
    let temp = tempfile::tempdir().unwrap();
    let core = native_core(&temp).await;
    let package = temp.path().join("codex-package");
    std::fs::write(package.join("rate-limits"), r#"{"ordinaryUsageAllowed":false,"rateLimits":{"credits":{"hasCredits":true,"unlimited":false},"spendControlReached":false}}"#).unwrap();
    let client = zeron_rpc::memory_client(core.rpc_service());
    let eligible: VoiceEligibility = client
        .call_as(
            methods::VOICE_ELIGIBILITY,
            json!({"chatId":"native-voice","hostDeviceId":core.device_id}),
        )
        .await
        .unwrap();
    assert!(eligible.available);
    assert_eq!(eligible.ordinary_usage_allowed, None);
    assert!(!eligible.credits_excluded);
    let (_, owner) = active_owner(&client, &core.device_id).await;
    drop(owner);
    // The quota is the backend's decision: never a round trip before a call.
    let wire = std::fs::read_to_string(package.join("voice-wire.jsonl")).unwrap();
    assert!(!wire.contains("account/rateLimits/read"));
    core.sessions.shutdown().await;
}

#[cfg(unix)]
#[tokio::test]
async fn host_change_during_probe_invalidates_start_before_reservation() {
    let temp = tempfile::tempdir().unwrap();
    let core = native_core(&temp).await;
    let package = temp.path().join("codex-package");
    std::fs::write(package.join("probe-delay"), "delay").unwrap();
    let client = zeron_rpc::memory_client(core.rpc_service());
    let rpc = zeron_rpc::memory_client(core.rpc_service());
    let device = core.device_id.clone();
    let start = tokio::spawn(async move {
        rpc.call(
            methods::START_VOICE,
            json!({"chatId":"native-voice","hostDeviceId":device}),
        )
        .await
    });
    wait_marker(&package.join("probing")).await;
    client
        .call(
            methods::MUTATE,
            json!({"op":"setChatHost","chatId":"native-voice","deviceId":"remote"}),
        )
        .await
        .unwrap();
    std::fs::write(package.join("probe-release"), "go").unwrap();
    assert!(start.await.unwrap().is_err());
    assert!(!package.join("helper-wire.jsonl").exists());
    core.sessions.shutdown().await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn active_voice_closes_on_host_change_without_waiting_for_ui() {
    for direct_sync_update in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let core = native_core(&temp).await;
        let client = zeron_rpc::memory_client(core.rpc_service());
        let (_, mut owner) = active_owner(&client, &core.device_id).await;
        let package = temp.path().join("codex-package");
        let pid = std::fs::read_to_string(package.join("helper.pid")).unwrap();
        if direct_sync_update {
            core.workspace
                .set_chat_host("native-voice", "remote")
                .unwrap();
        } else {
            client
                .call(
                    methods::MUTATE,
                    json!({"op":"setChatHost","chatId":"native-voice","deviceId":"remote"}),
                )
                .await
                .unwrap();
        }
        tokio::time::timeout(std::time::Duration::from_millis(500), async {
            while std::path::Path::new(&format!("/proc/{pid}")).exists() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while let Some(event) = owner.recv().await {
                if matches!(
                    serde_json::from_value::<zeron_proto::voice::VoiceEvent>(event).unwrap(),
                    zeron_proto::voice::VoiceEvent::Closed { .. }
                ) {
                    return;
                }
            }
            panic!("missing closed event");
        })
        .await
        .unwrap();
        core.sessions.shutdown().await;
    }
}

#[cfg(unix)]
#[tokio::test]
async fn external_identity_change_during_probe_rejects_start_and_explicit_retry_renews_idle_runtime()
 {
    let temp = tempfile::tempdir().unwrap();
    let core = native_core(&temp).await;
    let package = temp.path().join("codex-package");
    std::fs::write(package.join("probe-delay"), "delay").unwrap();
    std::fs::write(package.join("conversation-only"), "true").unwrap();
    let client = zeron_rpc::memory_client(core.rpc_service());
    let rpc = zeron_rpc::memory_client(core.rpc_service());
    let device = core.device_id.clone();
    let start = tokio::spawn(async move {
        rpc.call(
            methods::START_VOICE,
            json!({"chatId":"native-voice","hostDeviceId":device}),
        )
        .await
    });
    wait_marker(&package.join("probing")).await;
    std::fs::write(package.join("identity-updated"), "changed").unwrap();
    wait_marker(&package.join("identity-notified")).await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    std::fs::write(package.join("probe-release"), "go").unwrap();
    assert!(start.await.unwrap().is_err());
    assert!(!package.join("helper-wire.jsonl").exists());
    let wire = std::fs::read_to_string(package.join("voice-wire.jsonl")).unwrap();
    assert!(!wire.contains("thread/realtime/start"));
    std::fs::remove_file(package.join("probe-delay")).unwrap();
    let (_, owner) = active_owner(&client, &core.device_id).await;
    drop(owner);
    core.sessions.shutdown().await;
}

#[cfg(unix)]
#[tokio::test]
async fn pure_voice_updates_sidebar_activity_once_and_replay_does_not_bump_it() {
    let temp = tempfile::tempdir().unwrap();
    let core = native_core(&temp).await;
    let package = temp.path().join("codex-package");
    std::fs::write(package.join("conversation-only"), "true").unwrap();
    let client = zeron_rpc::memory_client(core.rpc_service());
    assert!(
        core.workspace
            .chat("native-voice")
            .unwrap()
            .unwrap()
            .last_message_at
            .is_none()
    );
    let (_, owner) = active_owner(&client, &core.device_id).await;
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while core
            .workspace
            .chat("native-voice")
            .unwrap()
            .unwrap()
            .last_message_preview
            .as_deref()
            != Some("I can do that.")
        {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let row = core.workspace.chat("native-voice").unwrap().unwrap();
    assert!(row.last_message_at.is_some());
    assert!(!core.sessions.turn_in_flight("native-voice"));
    std::fs::write(package.join("replay-transcripts"), "replay").unwrap();
    wait_marker(&package.join("replay-sent")).await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let replayed = core.workspace.chat("native-voice").unwrap().unwrap();
    assert_eq!(replayed.last_message_at, row.last_message_at);
    assert_eq!(replayed.last_message_preview, row.last_message_preview);
    assert_eq!(
        core.doc_host
            .open("native-voice")
            .unwrap()
            .doc()
            .read_entries()
            .unwrap()
            .len(),
        2
    );
    drop(owner);
    core.sessions.shutdown().await;
}

#[cfg(unix)]
#[tokio::test]
async fn failed_start_or_mute_emits_one_requested_stop_barrier_before_immediate_retry() {
    use zeron_proto::voice::{MuteVoice, VoiceEvent, VoiceLease};
    for mode in ["exit-open", "exit-controls"] {
        let temp = tempfile::tempdir().unwrap();
        let core = native_core(&temp).await;
        let package = temp.path().join("codex-package");
        std::fs::write(package.join("delayed-close"), "delay").unwrap();
        std::fs::write(package.join("spontaneous-close"), "true").unwrap();
        let client = zeron_rpc::memory_client(core.rpc_service());
        if mode == "exit-open" {
            std::fs::write(package.join("helper-mode"), mode).unwrap();
            let lease: VoiceLease = client
                .call_as(
                    methods::START_VOICE,
                    json!({"chatId":"native-voice","hostDeviceId":core.device_id}),
                )
                .await
                .unwrap();
            let mut owner = client
                .subscribe_scoped(methods::OWN_VOICE, serde_json::to_value(lease).unwrap())
                .await
                .unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(3), async {
                while let Some(v) = owner.recv().await {
                    if matches!(
                        serde_json::from_value::<VoiceEvent>(v).unwrap(),
                        VoiceEvent::Closed { .. }
                    ) {
                        return;
                    }
                }
                panic!("missing failure");
            })
            .await
            .unwrap();
        } else {
            let (lease, _owner) = active_owner(&client, &core.device_id).await;
            std::fs::write(package.join("helper-mode"), mode).unwrap();
            assert!(
                client
                    .call(
                        methods::MUTE_VOICE,
                        serde_json::to_value(MuteVoice { lease, muted: true }).unwrap()
                    )
                    .await
                    .is_err()
            );
        }
        std::fs::remove_file(package.join("helper-mode")).unwrap();
        let (lease, owner) = active_owner(&client, &core.device_id).await;
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        client
            .call(
                methods::MUTE_VOICE,
                serde_json::to_value(MuteVoice { lease, muted: true }).unwrap(),
            )
            .await
            .expect("late terminal must not close successor");
        let wire = std::fs::read_to_string(package.join("voice-wire.jsonl")).unwrap();
        assert_eq!(
            wire.lines()
                .filter(|line| line.contains("\"method\": \"thread/realtime/stop\""))
                .count(),
            1,
            "one native stop per failed session"
        );
        drop(owner);
        core.sessions.shutdown().await;
    }
}

#[cfg(unix)]
#[tokio::test]
#[ignore = "run with ZERON_REMOTE_VOICE=1; no provider or hardware access"]
async fn remote_failed_prepare_after_rotation_preserves_thread_for_retry() {
    use zeron_proto::voice::{ORCHESTRATOR_CHAT_PREFIX, remote as wire};
    assert_eq!(std::env::var("ZERON_REMOTE_VOICE").as_deref(), Ok("1"));
    let temp = tempfile::tempdir().unwrap();
    let core = native_core(&temp).await;
    let client = zeron_rpc::memory_client(core.rpc_service());
    let config =
        serde_json::from_value(json!({"harness":"codex","sandbox":"danger-full-access"})).unwrap();
    let mut request = wire::Prepare {
        attempt_key: wire::AttemptKey::new(),
        config,
        voice: Some("invalid-voice".into()),
    };
    let envelope = |p: serde_json::Value| json!({"targetDeviceId":core.device_id,"payload":p});

    // Seed a previous call at the rotation threshold with its native thread.
    let previous = format!("{ORCHESTRATOR_CHAT_PREFIX}previous");
    core.workspace
        .create_chat(
            &previous,
            None,
            Some(&core.device_id),
            Some(request.config.clone()),
            None,
        )
        .unwrap();
    core.workspace
        .set_chat_harness_session(&previous, "remembered-thread", "");
    // The engine's `ROTATE_AFTER`.
    const ROTATE_AFTER: usize = 1000;
    let doc = core.doc_host.open(&previous).unwrap();
    for i in 0..ROTATE_AFTER {
        doc.write_user_message(&format!("m{i}"), "remember me", i as i64)
            .unwrap();
    }

    // An unsupported voice fails after rotation and exercises the real
    // preparation cleanup, rather than manually deleting the successor.
    let failure = client
        .call(methods::PREPARE_VOICE_V2, envelope(json!(request)))
        .await
        .unwrap_err();
    assert!(
        matches!(failure, zeron_rpc::RpcError::Failed(ref message)
            if message == "voice unavailable: Unsupported"),
        "unexpected preparation failure: {failure:?}"
    );
    let remaining: Vec<_> = core
        .workspace
        .read_chats()
        .unwrap()
        .into_iter()
        .filter(|c| zeron_proto::voice::is_orchestrator_chat(&c.id))
        .collect();
    assert_eq!(
        remaining.len(),
        1,
        "cleanup must remove the empty successor"
    );
    assert_eq!(
        remaining[0].id, previous,
        "cleanup must keep the predecessor"
    );
    assert_eq!(
        core.workspace.chat_harness_session(&previous).unwrap().0,
        "remembered-thread"
    );
    assert_eq!(doc.doc().read_entries().unwrap().len(), ROTATE_AFTER);

    // A fresh attempt rotates again and resumes the original Codex thread.
    request.attempt_key = wire::AttemptKey::new();
    request.voice = None;
    let prepared: wire::Prepared = client
        .call_as(methods::PREPARE_VOICE_V2, envelope(json!(request)))
        .await
        .unwrap();
    assert_ne!(prepared.chat_id, previous);
    assert_eq!(
        core.sessions
            .last_request(&prepared.chat_id)
            .unwrap()
            .resume
            .as_deref(),
        Some("remembered-thread")
    );
    assert!(core.workspace.chat(&previous).unwrap().is_some());
    client
        .call(methods::STOP_VOICE_V2, envelope(json!(prepared.lease)))
        .await
        .unwrap();
    core.sessions.shutdown().await;
}

#[cfg(unix)]
#[tokio::test]
#[ignore = "run with ZERON_REMOTE_VOICE=1; no provider or hardware access"]
async fn remote_voice_full_control_flow_and_idempotent_prepare_without_host_audio() {
    use zeron_proto::voice::{VoiceEvent, remote as wire};
    assert_eq!(std::env::var("ZERON_REMOTE_VOICE").as_deref(), Ok("1"));
    let temp = tempfile::tempdir().unwrap();
    let core = native_core(&temp).await;
    std::fs::remove_file(
        temp.path()
            .join("codex-package/codex-resources/voice/bin/codex-voice-host"),
    )
    .unwrap();
    let client = zeron_rpc::memory_client(core.rpc_service());
    let request = wire::Prepare {
        attempt_key: wire::AttemptKey::new(),
        config: serde_json::from_value(json!({"harness":"codex","sandbox":"danger-full-access"}))
            .unwrap(),
        voice: None,
    };
    let envelope = |p: serde_json::Value| json!({"targetDeviceId":core.device_id,"payload":p});
    let call = envelope(serde_json::to_value(&request).unwrap());
    let prepared: wire::Prepared = client
        .call_as(methods::PREPARE_VOICE_V2, call.clone())
        .await
        .unwrap();
    let again: wire::Prepared = client
        .call_as(methods::PREPARE_VOICE_V2, call)
        .await
        .unwrap();
    assert_eq!(again.chat_id, prepared.chat_id);
    assert_eq!(
        again.lease.voice.session_id,
        prepared.lease.voice.session_id
    );
    assert!(prepared.chat_id.starts_with("voice-orchestrator-"));
    assert_eq!(
        core.workspace
            .chat(&prepared.chat_id)
            .unwrap()
            .unwrap()
            .title
            .as_deref(),
        Some(zeron_proto::voice::ORCHESTRATOR_CHAT_TITLE)
    );
    let mut owner = client
        .subscribe_checked(
            methods::OWN_VOICE_V2,
            envelope(serde_json::to_value(&prepared.lease).unwrap()),
        )
        .await
        .unwrap();
    assert!(
        client
            .subscribe_checked(
                methods::OWN_VOICE_V2,
                envelope(serde_json::to_value(&prepared.lease).unwrap())
            )
            .await
            .is_err()
    );
    let id = wire::AttemptKey::new();
    let negotiation = wire::Negotiate {
        lease: prepared.lease.clone(),
        negotiation_id: id.clone(),
        offer: wire::Sdp::new("fixture-offer".into()).unwrap(),
    };
    let reply: wire::Negotiated = client
        .call_as(
            methods::NEGOTIATE_VOICE_V2,
            envelope(serde_json::to_value(&negotiation).unwrap()),
        )
        .await
        .unwrap();
    assert_eq!(reply.answer.expose(), "fixture-answer");
    let repeat: wire::Negotiated = client
        .call_as(
            methods::NEGOTIATE_VOICE_V2,
            envelope(serde_json::to_value(&negotiation).unwrap()),
        )
        .await
        .unwrap();
    assert_eq!(repeat.answer, reply.answer);
    client
        .call(
            methods::CONFIRM_VOICE_MEDIA_V2,
            envelope(
                serde_json::to_value(wire::Confirm {
                    lease: prepared.lease.clone(),
                    negotiation_id: id,
                    muted: false,
                })
                .unwrap(),
            ),
        )
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            if matches!(
                serde_json::from_value::<VoiceEvent>(owner.recv().await.unwrap()).unwrap(),
                VoiceEvent::Final { .. }
            ) {
                break;
            }
        }
    })
    .await
    .unwrap();
    drop(owner);
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    assert!(
        client
            .call(
                methods::PREPARE_VOICE_V2,
                envelope(serde_json::to_value(request).unwrap())
            )
            .await
            .is_err()
    );
    assert!(!temp.path().join("codex-package/helper-wire.jsonl").exists());
    core.sessions.shutdown().await;
}

#[cfg(unix)]
#[tokio::test]
#[ignore = "run with ZERON_REMOTE_VOICE=1; no provider or hardware access"]
async fn remote_cancel_before_prepare_and_owner_drop_preserve_other_calls() {
    use zeron_proto::voice::remote as wire;
    assert_eq!(std::env::var("ZERON_REMOTE_VOICE").as_deref(), Ok("1"));
    let temp = tempfile::tempdir().unwrap();
    let core = native_core(&temp).await;
    let client = zeron_rpc::memory_client(core.rpc_service());
    let wrap = |p: serde_json::Value| json!({"targetDeviceId":core.device_id,"payload":p});
    let config =
        serde_json::from_value(json!({"harness":"codex","sandbox":"danger-full-access"})).unwrap();
    let mut request = wire::Prepare {
        attempt_key: wire::AttemptKey::new(),
        config,
        voice: None,
    };
    client
        .call(
            methods::CANCEL_VOICE_ATTEMPT_V2,
            wrap(json!({"attemptKey":request.attempt_key})),
        )
        .await
        .unwrap();
    assert!(
        client
            .call(methods::PREPARE_VOICE_V2, wrap(json!(request)))
            .await
            .is_err()
    );
    assert!(!temp.path().join("codex-package/voice-wire.jsonl").exists());
    request.attempt_key = wire::AttemptKey::new();
    let prepared: wire::Prepared = client
        .call_as(methods::PREPARE_VOICE_V2, wrap(json!(request)))
        .await
        .unwrap();
    let owner = client
        .subscribe_checked(methods::OWN_VOICE_V2, wrap(json!(prepared.lease)))
        .await
        .unwrap();
    drop(owner);
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while core.workspace.chat(&prepared.chat_id).unwrap().is_some() {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let _: zeron_proto::EngineInfo = client
        .call_as(methods::ENGINE_INFO, json!({}))
        .await
        .unwrap();
    // The dropped voice stream did not close the connection or another generation.
    request.attempt_key = wire::AttemptKey::new();
    let next: wire::Prepared = client
        .call_as(methods::PREPARE_VOICE_V2, wrap(json!(request)))
        .await
        .unwrap();
    client
        .call(methods::STOP_VOICE_V2, wrap(json!(prepared.lease)))
        .await
        .unwrap();
    let next_owner = client
        .subscribe_checked(methods::OWN_VOICE_V2, wrap(json!(next.lease)))
        .await
        .unwrap();
    drop(next_owner);
    core.sessions.shutdown().await;
}

#[cfg(unix)]
#[path = "support/voice_relay.rs"]
mod voice_relay;

#[cfg(unix)]
#[tokio::test]
#[ignore = "run with ZERON_REMOTE_VOICE=1; no provider or hardware access"]
async fn remote_voice_crosses_two_engines_and_owner_drop_releases_host() {
    use std::{sync::Arc, time::Duration};
    use zeron_proto::voice::{VoiceEvent, remote as wire};
    use zeron_rpc::{HostRelay, HostRelayConfig, LinkCache, LinkCacheConfig, StaticToken};
    assert_eq!(std::env::var("ZERON_REMOTE_VOICE").as_deref(), Ok("1"));
    let temp = tempfile::tempdir().unwrap();
    let host = native_core(&temp).await;
    std::fs::remove_file(
        temp.path()
            .join("codex-package/codex-resources/voice/bin/codex-voice-host"),
    )
    .unwrap();
    let (url, room) = voice_relay::fake_device_room().await;
    let relay = HostRelay::spawn(
        HostRelayConfig::new(
            &url,
            host.device_id.clone(),
            Arc::new(StaticToken("test".into())),
        ),
        host.rpc_service(),
        Arc::new(|_| true),
    );
    let viewer_dir = tempfile::tempdir().unwrap();
    let viewer = EngineCore::assemble_with_profile(
        EngineProfile::local(viewer_dir.path()).unwrap(),
        Arc::new(HarnessRegistry::new()),
        HarnessId::Codex,
        None,
    )
    .unwrap();
    let mut links = LinkCacheConfig::new(url, Arc::new(StaticToken("test".into())));
    links.probe_timeout = Duration::from_secs(3);
    viewer.set_links(LinkCache::new(links));
    let client = zeron_rpc::memory_client(viewer.rpc_service());
    let wrap = |p: serde_json::Value| json!({"targetDeviceId":host.device_id,"payload":p});
    tokio::time::timeout(Duration::from_secs(12), async {
        loop {
            if client
                .call(methods::VOICE_CAPABILITIES_V2, wrap(json!({})))
                .await
                .is_ok()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap();
    let request = wire::Prepare {
        attempt_key: wire::AttemptKey::new(),
        config: serde_json::from_value(json!({"harness":"codex","sandbox":"danger-full-access"}))
            .unwrap(),
        voice: None,
    };
    let prepared: wire::Prepared = client
        .call_as(methods::PREPARE_VOICE_V2, wrap(json!(request)))
        .await
        .unwrap();
    assert!(viewer.workspace.chat(&prepared.chat_id).unwrap().is_none());
    let mut owner = client
        .subscribe_checked(methods::OWN_VOICE_V2, wrap(json!(prepared.lease)))
        .await
        .unwrap();
    let id = wire::AttemptKey::new();
    let _: wire::Negotiated = client
        .call_as(
            methods::NEGOTIATE_VOICE_V2,
            wrap(json!(wire::Negotiate {
                lease: prepared.lease.clone(),
                negotiation_id: id.clone(),
                offer: wire::Sdp::new("fixture-offer".into()).unwrap()
            })),
        )
        .await
        .unwrap();
    client
        .call(
            methods::CONFIRM_VOICE_MEDIA_V2,
            wrap(json!(wire::Confirm {
                lease: prepared.lease.clone(),
                negotiation_id: id,
                muted: false
            })),
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while !matches!(
            serde_json::from_value::<VoiceEvent>(owner.recv().await.unwrap()).unwrap(),
            VoiceEvent::Final { .. }
        ) {}
    })
    .await
    .unwrap();
    drop(owner);
    tokio::time::timeout(Duration::from_secs(4), async {
        loop {
            let report = wire::Report {
                lease: prepared.lease.clone(),
                sequence: 1,
                muted: true,
                state: wire::MediaState::Ready,
            };
            if client
                .call(methods::REPORT_VOICE_MEDIA_V2, wrap(json!(report)))
                .await
                .is_err()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let info: zeron_proto::EngineInfo = client
        .call_as(
            methods::ENGINE_INFO,
            json!({"targetDeviceId":host.device_id}),
        )
        .await
        .unwrap();
    assert_eq!(info.device_id, viewer.device_id); // EngineInfo is deliberately local.
    client
        .call(methods::VOICE_CAPABILITIES_V2, wrap(json!({})))
        .await
        .unwrap();
    assert!(!temp.path().join("codex-package/helper-wire.jsonl").exists());
    drop(relay);
    room.abort();
    host.sessions.shutdown().await;
    viewer.sessions.shutdown().await;
}
