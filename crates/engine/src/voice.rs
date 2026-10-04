//! Exclusive, local, ephemeral voice ownership. Native media remains in Codex's helper.
pub(crate) mod remote;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use zeron_harness::codex::realtime::{RealtimeHandle, VoiceCommand};
use zeron_proto::{SessionStatus, voice::*};

/// Visual speaker activity: hysteresis and a short hold bridge syllable gaps.
#[derive(Default)]
struct SpeakerActivity {
    playing: bool,
    last_loud: Option<Instant>,
}
impl SpeakerActivity {
    fn update(&mut self, peak: u16, now: Instant) -> bool {
        const ENTER: u16 = 655;
        const EXIT: u16 = 328;
        const HOLD: Duration = Duration::from_millis(250);
        if peak >= if self.playing { EXIT } else { ENTER } {
            self.playing = true;
            self.last_loud = Some(now);
        } else if self.last_loud.is_none_or(|last| now.duration_since(last) >= HOLD) {
            self.playing = false;
        }
        self.playing
    }
}

#[derive(Clone, Default)]
pub struct VoiceManager {
    inner: Arc<Inner>,
}
#[derive(Default)]
struct Inner {
    slot: Mutex<Option<Slot>>,
    attempts:
        Mutex<std::collections::HashMap<zeron_proto::voice::remote::AttemptKey, remote::Attempt>>,
    generation: AtomicU64,
    identity_epoch: AtomicU64,
    // A successor waits until the old native stop completes, even after its owner is dropped.
    preparation: tokio::sync::Mutex<()>,
    native_lifecycle: Arc<tokio::sync::Mutex<()>>,
    mute_order: tokio::sync::Mutex<()>,
}
struct Slot {
    lease: VoiceLease,
    snapshot: VoiceSnapshot,
    attach_deadline: Instant,
    owner_attached: bool,
    events: Option<mpsc::Receiver<VoiceEvent>>,
    sender: mpsc::Sender<VoiceEvent>,
    cancel: CancellationToken,
    provider: Option<RealtimeHandle>,
    remote: Option<remote::RemoteSlot>,
}
pub struct VoiceOwner {
    manager: VoiceManager,
    lease: VoiceLease,
    events: mpsc::Receiver<VoiceEvent>,
}
impl VoiceOwner {
    pub async fn next(&mut self) -> Option<VoiceEvent> {
        self.events.recv().await
    }
}
impl Drop for VoiceOwner {
    fn drop(&mut self) {
        let _ = self.manager.stop(&self.lease);
    }
}

impl VoiceManager {
    pub(crate) async fn preparation(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.inner.preparation.lock().await
    }
    pub fn unavailable(reason: VoiceRejection) -> VoiceEligibility {
        VoiceEligibility {
            available: false,
            reason: Some(reason),
            ordinary_usage_allowed: None,
            credits_excluded: false,
            format: None,
            duplex_verified: false,
            native_webrtc: false,
            voices: Vec::new(),
        }
    }
    pub(crate) fn identity_epoch(&self) -> u64 {
        self.inner.identity_epoch.load(Ordering::Acquire)
    }
    #[cfg(test)]
    pub(crate) fn reserve(&self, chat: &str) -> Result<VoiceLease, VoiceRejection> {
        self.reserve_at(chat, self.identity_epoch())
    }
    pub(crate) fn reserve_at(&self, chat: &str, epoch: u64) -> Result<VoiceLease, VoiceRejection> {
        self.reserve_with_deadline(chat, epoch, 5)
    }
    fn reserve_with_deadline(
        &self,
        chat: &str,
        epoch: u64,
        seconds: u64,
    ) -> Result<VoiceLease, VoiceRejection> {
        let mut state = self.inner.slot.lock().unwrap();
        if self.identity_epoch() != epoch {
            return Err(VoiceRejection::InvalidLease);
        }
        if state.is_some() {
            return Err(VoiceRejection::Busy);
        }
        let generation = self.inner.generation.fetch_add(1, Ordering::AcqRel) + 1;
        let lease = VoiceLease {
            session_id: uuid::Uuid::new_v4().to_string(),
            generation,
            token: uuid::Uuid::new_v4().to_string(),
        };
        let snapshot = VoiceSnapshot {
            session_id: lease.session_id.clone(),
            chat_id: chat.into(),
            generation,
            phase: VoicePhase::Starting,
            muted: false,
            playing: false,
            work: VoiceWork::Idle,
            reason: None,
            voice: None,
            voices: Vec::new(),
        };
        let (sender, events) = mpsc::channel(32);
        let _ = sender.try_send(VoiceEvent::Snapshot {
            snapshot: snapshot.clone(),
        });
        *state = Some(Slot {
            lease: lease.clone(),
            snapshot,
            attach_deadline: Instant::now() + Duration::from_secs(seconds),
            owner_attached: false,
            events: Some(events),
            sender,
            cancel: CancellationToken::new(),
            provider: None,
            remote: None,
        });
        drop(state);
        let manager = self.clone();
        let pending = lease.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(seconds)).await;
            manager.expire_unattached(&pending);
        });
        Ok(lease)
    }
    fn expire_unattached(&self, lease: &VoiceLease) {
        let expired = self.inner.slot.lock().unwrap().as_ref().is_some_and(|s| {
            Self::matches(s, lease) && !s.owner_attached && Instant::now() >= s.attach_deadline
        });
        if expired {
            let _ = self.stop(lease);
        }
    }
    fn matches(s: &Slot, l: &VoiceLease) -> bool {
        s.lease.session_id == l.session_id
            && s.lease.generation == l.generation
            && s.lease.token == l.token
    }
    pub fn own(&self, lease: VoiceLease) -> Result<VoiceOwner, VoiceRejection> {
        let mut state = self.inner.slot.lock().unwrap();
        let slot = state
            .as_mut()
            .filter(|s| Self::matches(s, &lease))
            .ok_or(VoiceRejection::InvalidLease)?;
        if slot.owner_attached || Instant::now() >= slot.attach_deadline {
            return Err(VoiceRejection::InvalidLease);
        }
        let events = slot.events.take().ok_or(VoiceRejection::InvalidLease)?;
        slot.owner_attached = true;
        if let Some(remote) = &mut slot.remote {
            remote.last_report = Instant::now();
        }
        Ok(VoiceOwner {
            manager: self.clone(),
            lease,
            events,
        })
    }
    fn publish(&self, lease: &VoiceLease, event: VoiceEvent) -> Result<(), VoiceRejection> {
        let result = {
            let state = self.inner.slot.lock().unwrap();
            let slot = state
                .as_ref()
                .filter(|s| Self::matches(s, lease))
                .ok_or(VoiceRejection::InvalidLease)?;
            slot.sender
                .try_send(event)
                .map_err(|_| VoiceRejection::Overflow)
        };
        if result.is_err() {
            let _ = self.stop(lease);
        }
        result
    }
    fn update(
        &self,
        lease: &VoiceLease,
        f: impl FnOnce(&mut VoiceSnapshot),
    ) -> Result<(), VoiceRejection> {
        let snapshot = {
            let mut state = self.inner.slot.lock().unwrap();
            let slot = state
                .as_mut()
                .filter(|s| Self::matches(s, lease))
                .ok_or(VoiceRejection::InvalidLease)?;
            f(&mut slot.snapshot);
            slot.snapshot.clone()
        };
        self.publish(lease, VoiceEvent::Snapshot { snapshot })
    }
    pub async fn mute(&self, lease: &VoiceLease, muted: bool) -> Result<(), VoiceRejection> {
        let _order = self.inner.mute_order.lock().await;
        let provider = {
            let state = self.inner.slot.lock().unwrap();
            let s = state
                .as_ref()
                .filter(|s| Self::matches(s, lease))
                .ok_or(VoiceRejection::InvalidLease)?;
            if s.snapshot.phase == VoicePhase::Active {
                s.provider.clone()
            } else {
                None
            }
        };
        if let Some(provider) = provider {
            if let Err(reason) = provider.mute(muted).await {
                let _ = self.finish(lease, Some(reason));
                return Err(reason);
            }
        }
        self.update(lease, |s| s.muted = muted)
    }
    pub fn stop(&self, lease: &VoiceLease) -> Result<(), VoiceRejection> {
        self.finish(lease, None)
    }
    fn finish(
        &self,
        lease: &VoiceLease,
        reason: Option<VoiceRejection>,
    ) -> Result<(), VoiceRejection> {
        let mut state = self.inner.slot.lock().unwrap();
        let Some(s) = state.as_ref() else {
            return Ok(());
        };
        if !Self::matches(s, lease) {
            return Err(VoiceRejection::InvalidLease);
        }
        let s = state.take().unwrap();
        drop(state);
        Self::close_slot(s, reason);
        Ok(())
    }
    fn close_slot(s: Slot, reason: Option<VoiceRejection>) {
        s.cancel.cancel();
        if let Some(provider) = s.provider.as_ref() {
            provider.abort_voice(s.lease.generation);
        }
        let _ = s.sender.try_send(VoiceEvent::Closed {
            generation: s.lease.generation,
            reason,
        });
    }
    #[cfg(test)]
    pub(crate) fn owns_chat(&self, chat: &str) -> bool {
        self.inner
            .slot
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|s| s.snapshot.chat_id == chat)
    }
    pub fn retire(&self) {
        let mut state = self.inner.slot.lock().unwrap();
        // The same lock covers reservation and identity changes: a pending probe
        // may never create a lease after account/profile retirement.
        self.inner.identity_epoch.fetch_add(1, Ordering::AcqRel);
        for attempt in self.inner.attempts.lock().unwrap().values() {
            attempt.cancel.cancel();
        }
        self.inner.generation.fetch_add(1, Ordering::AcqRel);
        let old = state.take();
        drop(state);
        if let Some(slot) = old {
            Self::close_slot(slot, None);
        }
    }
    pub(crate) fn retire_chat(&self, chat: &str) {
        let mut state = self.inner.slot.lock().unwrap();
        self.inner.identity_epoch.fetch_add(1, Ordering::AcqRel);
        let old = if state.as_ref().is_some_and(|s| s.snapshot.chat_id == chat) {
            state.take()
        } else {
            None
        };
        drop(state);
        if let Some(slot) = old {
            Self::close_slot(slot, None);
        }
    }
    pub(crate) fn connect(
        &self,
        lease: VoiceLease,
        chat: String,
        voice: Option<String>,
        voices: Vec<String>,
        bridge: RealtimeHandle,
        mut events: tokio::sync::broadcast::Receiver<VoiceEvent>,
        active: Arc<crate::sessions::VoiceActivity>,
        doc: Arc<crate::doc_host::ChatDocHandle>,
        sessions: crate::sessions::SessionsEngine,
        workspace: crate::workspace_host::WorkspaceHost,
        expected_chat: zeron_proto::Chat,
    ) -> Result<(), VoiceRejection> {
        if bridge.invalidated() {
            return Err(VoiceRejection::InvalidLease);
        }
        let cancel = {
            let mut state = self.inner.slot.lock().unwrap();
            let s = state
                .as_mut()
                .filter(|s| Self::matches(s, &lease))
                .ok_or(VoiceRejection::InvalidLease)?;
            s.provider = Some(bridge.clone());
            s.snapshot.voice = voice.clone();
            s.snapshot.voices = voices;
            s.cancel.clone()
        };
        let call = active.call();
        let manager = self.clone();
        tokio::spawn(async move {
            let _call = call;
            struct ChatGuard(tokio::task::JoinHandle<()>);
            impl Drop for ChatGuard {
                fn drop(&mut self) {
                    self.0.abort();
                }
            }
            let watch_manager = manager.clone();
            let watch_lease = lease.clone();
            let watch_cancel = cancel.clone();
            let watched_workspace = workspace.clone();
            let mut rows = workspace.watch_chats();
            let _chat_guard = ChatGuard(tokio::spawn(async move {
                loop {
                    let invalid = match watched_workspace.chat(&expected_chat.id) {
                        Ok(Some(row)) if row.device_id != watched_workspace.device_id() => {
                            Some(VoiceRejection::RemoteHost)
                        }
                        Ok(Some(row))
                            if row.config.as_ref().map(|c| c.harness)
                                != Some(zeron_proto::HarnessId::Codex) =>
                        {
                            Some(VoiceRejection::WrongHarness)
                        }
                        Ok(Some(row))
                            if row.config != expected_chat.config
                                || row.cwd != expected_chat.cwd =>
                        {
                            Some(VoiceRejection::InvalidLease)
                        }
                        Ok(Some(_)) => None,
                        _ => Some(VoiceRejection::Unsupported),
                    };
                    if let Some(reason) = invalid {
                        let _ = watch_manager.finish(&watch_lease, Some(reason));
                        return;
                    }
                    tokio::select! { biased;
                        _ = watch_cancel.cancelled() => return,
                        changed = rows.changed() => if changed.is_err() { let _ = watch_manager.finish(&watch_lease, Some(VoiceRejection::Protocol)); return; },
                    }
                }
            }));
            let result=async {
                let _serial=tokio::select!{biased;_=cancel.cancelled()=>return Ok(()),lock=manager.inner.native_lifecycle.lock()=>lock};
                loop {
                    let owner=manager.inner.slot.lock().unwrap().as_ref().filter(|s|Self::matches(s,&lease)).is_some_and(|s|s.owner_attached);
                    if owner{break;}tokio::select!{biased;_=cancel.cancelled()=>return Ok(()),_=tokio::time::sleep(Duration::from_millis(10))=>{}}
                }
                let (reply,rx)=tokio::sync::oneshot::channel();
                tokio::select!{biased;
                    _=cancel.cancelled()=>return Ok(()),
                    sent=tokio::time::timeout(Duration::from_secs(8),bridge.commands.send(VoiceCommand::Start{voice,session_id:lease.session_id.clone(),generation:lease.generation,reply}))=>sent.map_err(|_|VoiceRejection::Protocol)?.map_err(|_|VoiceRejection::Protocol)?,
                }
                let startup=async {
                    tokio::time::timeout(Duration::from_secs(110),rx).await.map_err(|_|VoiceRejection::Protocol)?.map_err(|_|VoiceRejection::Protocol)??;
                    loop {
                        let owner=manager.inner.slot.lock().unwrap().as_ref().filter(|s|Self::matches(s,&lease)).is_some_and(|s|s.owner_attached);
                        if owner{break;}tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    let _order=manager.inner.mute_order.lock().await;
                    let muted=manager.inner.slot.lock().unwrap().as_ref().filter(|s|Self::matches(s,&lease)).ok_or(VoiceRejection::InvalidLease)?.snapshot.muted;
                    let work=match sessions.session_status(&chat).map(|s|s.status){Some(SessionStatus::AwaitingInput)=>VoiceWork::AwaitingInput,Some(SessionStatus::Working)=>VoiceWork::Working,_=>VoiceWork::Idle};
                    if work==VoiceWork::AwaitingInput{return Err(VoiceRejection::Busy);}
                    bridge.mute(muted).await?;manager.update(&lease,|s|{s.phase=VoicePhase::Active;s.work=work;})?;Ok::<_,VoiceRejection>(())
                };
                // Startup cancellation includes the helper and late native signaling requests.
                let started=tokio::select!{biased;_=cancel.cancelled()=>Ok(()),r=startup=>r};
                if started.is_err()||cancel.is_cancelled(){let _=bridge.stop().await;return started;}
                let mut statuses=sessions.watch_sessions();
                let mut last_partial:Option<String>=None;
                let mut speaker_activity = SpeakerActivity::default();
                let run=async {loop{tokio::select!{biased;
                    _=cancel.cancelled()=>return Ok(()),
                    changed=statuses.changed()=>{
                        changed.map_err(|_|VoiceRejection::Protocol)?;
                        let status=statuses.borrow().iter().find(|s|s.chat_id==chat).map(|s|s.status);
                        let work=match status{Some(SessionStatus::AwaitingInput)=>VoiceWork::AwaitingInput,Some(SessionStatus::Working)=>VoiceWork::Working,_=>VoiceWork::Idle};
                        // The orchestrator outlives navigation: a pending question is
                        // surfaced on the voice stage instead of ending the session.
                        manager.update(&lease,|s|s.work=work)?;
                    },
                    event=events.recv()=>{
                        let event=event.map_err(|_|VoiceRejection::Overflow)?;
                        match &event {
                            VoiceEvent::Final{transcript}=>{
                                if transcript.session_id!=lease.session_id{continue;}
                                if doc.commit_voice(transcript).map_err(|_|VoiceRejection::Protocol)?.is_some() {
                                    workspace.note_message(&chat, &transcript.text);
                                }
                                last_partial=None;
                            },
                            VoiceEvent::Partial{generation,item_id,..}=>{if *generation!=lease.generation{continue;}if item_id!=&last_partial{last_partial=item_id.clone();}},
                            VoiceEvent::Levels{generation,speaker,..}=>{if *generation!=lease.generation{continue;}
                                let playing=speaker_activity.update(*speaker, Instant::now());
                                let changed=manager.inner.slot.lock().unwrap().as_ref().is_some_and(|s|s.snapshot.playing!=playing);
                                if changed{manager.update(&lease,|s|s.playing=playing)?;}
                            },
                            VoiceEvent::Closed{generation,reason}=>{if *generation!=lease.generation{continue;}return reason.map_or(Ok(()),Err);},
                            VoiceEvent::Audio{..}=>return Err(VoiceRejection::Protocol), // Never route API PCM into the subscription session.
                            _=>{},
                        }
                        manager.publish(&lease,event)?;
                    }
                }}}.await;
                let _=bridge.stop().await;run
            }.await;
            let _ = manager.finish(&lease, result.err());
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speaker_activity_ignores_noise_and_holds_through_syllable_gaps() {
        let now = Instant::now();
        let mut activity = SpeakerActivity::default();
        assert!(!activity.update(100, now));
        assert!(!activity.update(500, now));
        assert!(activity.update(1024, now));
        assert!(activity.update(0, now + Duration::from_millis(100)));
        assert!(activity.update(0, now + Duration::from_millis(249)));
        assert!(!activity.update(0, now + Duration::from_millis(250)));
        // The lower exit threshold keeps quiet speech active, but does not
        // let the same background level start a new speaking interval.
        assert!(!activity.update(500, now + Duration::from_millis(300)));
        assert!(activity.update(1024, now + Duration::from_millis(400)));
        assert!(activity.update(500, now + Duration::from_millis(600)));
        assert!(activity.update(0, now + Duration::from_millis(800)));
        assert!(!activity.update(0, now + Duration::from_millis(850)));
    }
    #[tokio::test]
    async fn exclusive_owner_drop_and_stale_guards() {
        let manager = VoiceManager::default();
        let lease = manager.reserve("chat").unwrap();
        assert_eq!(manager.reserve("chat").unwrap_err(), VoiceRejection::Busy);
        let mut bad = lease.clone();
        bad.token = "bad".into();
        assert!(manager.own(bad).is_err());
        let owner = manager.own(lease.clone()).unwrap();
        assert!(manager.own(lease.clone()).is_err());
        drop(owner);
        let next = manager.reserve("chat").unwrap();
        assert_eq!(manager.stop(&lease), Err(VoiceRejection::InvalidLease));
        assert!(manager.own(next).is_ok());
    }
    #[tokio::test(start_paused = true)]
    async fn unattached_owner_watchdog_releases_reservation() {
        let manager = VoiceManager::default();
        let old = manager.reserve("chat").unwrap();
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(6)).await;
        tokio::task::yield_now().await;
        assert!(manager.own(old.clone()).is_err());
        let next = manager.reserve("next").unwrap();
        assert!(next.generation > old.generation);
        let owner = manager.own(next).unwrap();
        tokio::time::advance(Duration::from_secs(6)).await;
        tokio::task::yield_now().await;
        assert!(manager.owns_chat("next"));
        drop(owner);
        assert!(!manager.owns_chat("next"));
    }

    #[tokio::test]
    async fn retirement_rejects_a_start_prepared_under_the_old_identity() {
        let manager = VoiceManager::default();
        let epoch = manager.identity_epoch();
        manager.retire();
        assert_eq!(
            manager.reserve_at("chat", epoch).unwrap_err(),
            VoiceRejection::InvalidLease
        );
        assert!(manager.reserve_at("chat", manager.identity_epoch()).is_ok());
    }

    #[tokio::test]
    async fn account_retirement_invalidates_lease() {
        let manager = VoiceManager::default();
        let lease = manager.reserve("chat").unwrap();
        manager.retire();
        assert!(manager.own(lease).is_err());
    }
}
