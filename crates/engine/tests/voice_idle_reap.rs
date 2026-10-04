//! A call holds the idle reaper off, and the idle window restarts at hang-up:
//! a long call must not leave a runtime that is reaped the moment it ends.
#![cfg(unix)]
use serde_json::json;
use std::time::Duration;
use zeron_engine::{EngineCore, EngineProfile, HarnessRegistry};
use zeron_proto::HarnessId;
use zeron_proto::voice::{VoiceEvent, VoiceLease, VoicePhase};
use zeron_rpc::methods;

/// Idle-reaper window for this file (process-global env knob, set before any
/// engine assembles).
const IDLE_MS: u64 = 1500;
const CHAT: &str = "native-voice";

async fn native_core(temp: &tempfile::TempDir) -> EngineCore {
    use std::os::unix::fs::PermissionsExt;
    // SAFETY: the only test in this process, before any engine reads the var.
    unsafe { std::env::set_var("ZERON_SESSION_IDLE_MS", IDLE_MS.to_string()) };
    let fixture =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../harness/tests/fixtures");
    let package = temp.path().join("codex-package");
    let binary = package.join("bin/codex");
    let helper = package.join("codex-resources/voice/bin/codex-voice-host");
    for (source, target) in [
        ("fake-codex-voice-native.py", &binary),
        ("fake-codex-voice-host.py", &helper),
    ] {
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::copy(fixture.join(source), target).unwrap();
        std::fs::set_permissions(target, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    std::fs::write(
        package.join("codex-package.json"),
        r#"{"layoutVersion":1,"version":"0.159.0"}"#,
    )
    .unwrap();
    // No delegated task: a turn in flight would keep the runtime regardless.
    std::fs::write(package.join("conversation-only"), "").unwrap();
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
            CHAT,
            None,
            Some(&core.device_id),
            Some(config),
            Some(package.display().to_string()),
        )
        .unwrap();
    core
}

#[tokio::test]
async fn a_long_call_keeps_its_runtime_until_the_window_after_hang_up() {
    let temp = tempfile::tempdir().unwrap();
    let core = native_core(&temp).await;
    let client = zeron_rpc::memory_client(core.rpc_service());
    let lease: VoiceLease = client
        .call_as(
            methods::START_VOICE,
            json!({"chatId":CHAT,"hostDeviceId":core.device_id}),
        )
        .await
        .unwrap();
    let mut owner = client
        .subscribe_scoped(methods::OWN_VOICE, serde_json::to_value(&lease).unwrap())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(v) = owner.recv().await {
            if matches!(serde_json::from_value::<VoiceEvent>(v).unwrap(),
                VoiceEvent::Snapshot { snapshot } if snapshot.phase == VoicePhase::Active)
            {
                return;
            }
        }
        panic!("voice ended before active");
    })
    .await
    .unwrap();

    // The call outlasts several idle windows without being cut.
    tokio::time::sleep(Duration::from_millis(IDLE_MS * 3)).await;
    assert!(
        core.sessions.live_run_steerable(CHAT),
        "reaped during a call"
    );

    client
        .call(methods::STOP_VOICE, serde_json::to_value(&lease).unwrap())
        .await
        .unwrap();
    drop(owner);
    let hung_up = tokio::time::Instant::now();

    // The window counts from the hang-up, not from the runtime's start...
    tokio::time::sleep(Duration::from_millis(IDLE_MS / 2)).await;
    assert!(
        core.sessions.live_run_steerable(CHAT),
        "reaped right after hang-up"
    );
    // ...and still ends it: one window plus the one-second hang-up check.
    while core.sessions.live_run_steerable(CHAT) {
        assert!(
            hung_up.elapsed() < Duration::from_millis(IDLE_MS + 3000),
            "runtime outlived its idle window after the call"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(hung_up.elapsed() >= Duration::from_millis(IDLE_MS));
    core.sessions.shutdown().await;
}
