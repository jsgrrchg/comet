//! The engine owns Codex's native WebRTC helper. This viewport receives only
//! ephemeral controls, meters and transcripts, never PCM or SDP credentials.
use crate::state::EngineHandle;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use zeron_proto::voice::*;
use zeron_rpc::methods;
struct ProviderGuard {
    engine: EngineHandle,
    lease: VoiceLease,
}
impl Drop for ProviderGuard {
    fn drop(&mut self) {
        let engine = self.engine.clone();
        let lease = self.lease.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let _ = tokio::time::timeout(
                    Duration::from_secs(8),
                    engine
                        .client()
                        .call(methods::STOP_VOICE, serde_json::to_value(lease).unwrap()),
                )
                .await;
            });
        }
    }
}
pub(super) async fn run(
    engine: EngineHandle,
    request: StartVoice,
    cancel: CancellationToken,
    events: mpsc::Sender<VoiceEvent>,
    mut controls: mpsc::Receiver<super::VoiceControl>,
) -> Result<(), VoiceRejection> {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => return Ok(()),
        result = super::permissions::microphone() => result?,
    }
    let owner_client = engine
        .media_client()
        .await
        .map_err(|_| VoiceRejection::Protocol)?;
    let lease: VoiceLease = tokio::select! {biased;_=cancel.cancelled()=>return Ok(()),result=tokio::time::timeout(Duration::from_secs(60),engine.client().call_as(methods::START_VOICE,serde_json::to_value(request).unwrap()))=>result.map_err(|_|VoiceRejection::Protocol)?.map_err(rejection)?};
    let _provider = ProviderGuard {
        engine: engine.clone(),
        lease: lease.clone(),
    };
    let mut owner = owner_client
        .subscribe_scoped(methods::OWN_VOICE, serde_json::to_value(&lease).unwrap())
        .await
        .map_err(|_| VoiceRejection::Protocol)?;
    let (results, mut result_rx) = mpsc::channel(1);
    let mut pending = false;
    // Mute I/O cannot block receipt of owner events. Dropping this guard aborts an unfinished control.
    struct Pending(Option<tokio::task::JoinHandle<()>>);
    impl Drop for Pending {
        fn drop(&mut self) {
            if let Some(t) = self.0.take() {
                t.abort();
            }
        }
    }
    let mut control_task = Pending(None);
    loop {
        tokio::select! {biased;
            _=cancel.cancelled()=>break,
            _=events.closed()=>break,
            control=controls.recv(),if !pending=>match control{
                Some(super::VoiceControl::Mute(muted))=>{
                    pending=true;let engine=engine.clone();let lease=lease.clone();let results=results.clone();
                    control_task.0=Some(tokio::spawn(async move{let result=tokio::time::timeout(Duration::from_secs(8),engine.client().call(methods::MUTE_VOICE,serde_json::to_value(MuteVoice{lease,muted}).unwrap())).await;
                        let _=results.send(result.map_err(|_|VoiceRejection::Protocol).and_then(|r|r.map(|_|()).map_err(|_|VoiceRejection::Protocol))).await;
                    }));
                },None=>break,
            },
            result=result_rx.recv(),if pending=>{result.ok_or(VoiceRejection::Protocol)??;pending=false;control_task.0=None;},
            event=owner.recv()=>{
                let event:VoiceEvent=serde_json::from_value(event.ok_or(VoiceRejection::Protocol)?).map_err(|_|VoiceRejection::Protocol)?;
                let generation=match &event{VoiceEvent::Snapshot{snapshot}=>snapshot.generation,VoiceEvent::Partial{generation,..}|VoiceEvent::Levels{generation,..}|VoiceEvent::Closed{generation,..}|VoiceEvent::InvalidatePlayout{generation,..}=>*generation,VoiceEvent::Final{transcript} if transcript.session_id==lease.session_id=>lease.generation,_=>continue};
                if generation!=lease.generation{continue;}
                let closed=matches!(event,VoiceEvent::Closed{..});
                events.try_send(event).map_err(|_|VoiceRejection::Overflow)?;
                if closed{break;}
            }
        }
    }
    Ok(())
}

fn rejection(error: zeron_rpc::RpcError) -> VoiceRejection {
    let message = error.to_string();
    for reason in [
        VoiceRejection::NativeRuntimeUnavailable,
        VoiceRejection::ChatgptRequired,
        VoiceRejection::DeviceUnavailable,
        VoiceRejection::Busy,
        VoiceRejection::RemoteHost,
        VoiceRejection::WrongHarness,
        VoiceRejection::Unsupported,
    ] {
        if message.contains(&format!("{reason:?}")) {
            return reason;
        }
    }
    VoiceRejection::Protocol
}
