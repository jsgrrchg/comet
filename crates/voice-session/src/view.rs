//! What a client-media call shows: phase, live caption, levels and the orb's
//! state, reduced from the owner stream. Renderer-agnostic so every platform
//! presents the orchestrator the same way.
use std::time::{Duration, Instant};
use zeron_orb::OrbState;
use zeron_proto::voice::*;

/// The orb for a voice phase and host snapshot. Speaking wins over work and
/// mute, so a muted user still sees the assistant answer.
pub fn orb_state(phase: VoicePhase, snapshot: Option<&VoiceSnapshot>) -> OrbState {
    if matches!(
        phase,
        VoicePhase::Checking | VoicePhase::Starting | VoicePhase::Stopping
    ) {
        return OrbState::Connecting;
    }
    match snapshot {
        Some(s) if s.playing => OrbState::Composing,
        Some(s) if s.work == VoiceWork::AwaitingInput => OrbState::Solving,
        Some(s) if s.work == VoiceWork::Working => OrbState::Working,
        Some(s) if s.muted => OrbState::Breathing,
        Some(_) => OrbState::Listening,
        None => OrbState::Breathing,
    }
}

/// Visual speaker activity from playout peaks: hysteresis and a short hold
/// bridge syllable gaps. Client media has no host-side meter, so the client
/// derives `playing` itself.
#[derive(Default)]
pub struct SpeakerActivity {
    playing: bool,
    last_loud: Option<Instant>,
}
impl SpeakerActivity {
    pub fn update(&mut self, peak: u16, now: Instant) -> bool {
        const ENTER: u16 = 655;
        const EXIT: u16 = 328;
        const HOLD: Duration = Duration::from_millis(250);
        if peak >= if self.playing { EXIT } else { ENTER } {
            self.playing = true;
            self.last_loud = Some(now);
        } else if self
            .last_loud
            .is_none_or(|last| now.saturating_duration_since(last) >= HOLD)
        {
            self.playing = false;
        }
        self.playing
    }
}

/// Reduced state of one remote call. Events from another generation or
/// session are ignored; the first snapshot names the orchestrator chat.
#[derive(Default)]
pub struct VoiceView {
    pub phase: VoicePhase,
    pub snapshot: Option<VoiceSnapshot>,
    /// Live partial, then the final text of the latest item.
    pub caption: String,
    pub caption_role: Option<VoiceRole>,
    pub microphone: u16,
    pub speaker: u16,
    caption_item: Option<String>,
    muted: bool,
    speaker_activity: SpeakerActivity,
}

impl VoiceView {
    pub fn new() -> Self {
        Self {
            phase: VoicePhase::Checking,
            ..Self::default()
        }
    }

    pub fn chat_id(&self) -> Option<&str> {
        self.snapshot.as_ref().map(|s| s.chat_id.as_str())
    }

    /// The utterance (speaker turn) the caption belongs to, if named.
    pub fn caption_item(&self) -> Option<&str> {
        self.caption_item.as_deref()
    }

    pub fn work(&self) -> VoiceWork {
        self.snapshot.as_ref().map_or(VoiceWork::Idle, |s| s.work)
    }

    pub fn playing(&self) -> bool {
        self.snapshot.as_ref().is_some_and(|s| s.playing)
    }

    pub fn muted(&self) -> bool {
        self.muted
    }

    /// The local mute is authoritative for presentation: it applies before
    /// the host acknowledges it.
    pub fn set_muted(&mut self, muted: bool) -> bool {
        let changed = self.muted != muted;
        self.muted = muted;
        if let Some(snapshot) = &mut self.snapshot {
            snapshot.muted = muted;
        }
        changed
    }

    pub fn orb_state(&self) -> OrbState {
        orb_state(self.phase, self.snapshot.as_ref())
    }

    /// Normalized 0…1 microphone loudness; zero while muted.
    pub fn microphone_level(&self) -> f32 {
        if self.muted {
            0.0
        } else {
            f32::from(self.microphone) / f32::from(u16::MAX)
        }
    }

    pub fn speaker_level(&self) -> f32 {
        f32::from(self.speaker) / f32::from(u16::MAX)
    }

    fn current(&self, generation: u64) -> bool {
        self.snapshot
            .as_ref()
            .is_some_and(|s| s.generation == generation)
    }

    /// Whether the event changed what the call shows.
    pub fn reduce(&mut self, event: VoiceEvent, now: Instant) -> bool {
        match event {
            VoiceEvent::Snapshot { mut snapshot } => {
                if let Some(old) = &self.snapshot {
                    if old.generation != snapshot.generation
                        || old.session_id != snapshot.session_id
                    {
                        return false;
                    }
                    snapshot.playing = old.playing;
                }
                snapshot.muted = self.muted;
                self.phase = snapshot.phase;
                self.snapshot = Some(snapshot);
            }
            VoiceEvent::Partial {
                generation,
                item_id,
                text,
            } if self.current(generation) => {
                if self.caption_item != item_id || self.caption_role.is_some() {
                    self.caption.clear();
                    self.caption_item = item_id;
                    self.caption_role = None;
                }
                if self.caption.len() + text.len() > MAX_TRANSCRIPT_BYTES {
                    // Show the tail of a runaway utterance rather than grow.
                    self.caption.clear();
                }
                self.caption.push_str(&text);
            }
            VoiceEvent::Final { transcript }
                if self
                    .snapshot
                    .as_ref()
                    .is_some_and(|s| s.session_id == transcript.session_id) =>
            {
                self.caption = transcript.text;
                self.caption_item = Some(transcript.item_id);
                self.caption_role = Some(transcript.role);
            }
            VoiceEvent::Levels {
                generation,
                microphone,
                speaker,
            } if self.current(generation) => {
                self.microphone = microphone;
                self.speaker = speaker;
                let playing = self.speaker_activity.update(speaker, now);
                if let Some(snapshot) = &mut self.snapshot {
                    snapshot.playing = playing;
                }
            }
            VoiceEvent::Closed { generation, reason } if self.current(generation) => {
                self.phase = if reason.is_some() {
                    VoicePhase::Failed
                } else {
                    VoicePhase::Closed
                };
            }
            _ => return false,
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(generation: u64) -> VoiceSnapshot {
        VoiceSnapshot {
            session_id: "session".into(),
            chat_id: "voice-orchestrator-full-id".into(),
            generation,
            phase: VoicePhase::Active,
            muted: false,
            playing: false,
            work: VoiceWork::Idle,
            reason: None,
            voice: None,
            voices: vec![],
        }
    }

    #[test]
    fn speaker_activity_ignores_noise_and_holds_through_syllable_gaps() {
        let now = Instant::now();
        let mut activity = SpeakerActivity::default();
        assert!(!activity.update(500, now));
        assert!(activity.update(1024, now));
        assert!(activity.update(0, now + Duration::from_millis(249)));
        assert!(!activity.update(0, now + Duration::from_millis(250)));
        assert!(!activity.update(500, now + Duration::from_millis(300)));
    }

    #[test]
    fn remote_view_derives_speaking_and_keeps_it_across_snapshots() {
        let now = Instant::now();
        let mut view = VoiceView::new();
        assert_eq!(view.orb_state(), OrbState::Connecting);
        assert!(view.reduce(
            VoiceEvent::Snapshot {
                snapshot: snapshot(2)
            },
            now
        ));
        assert_eq!(view.chat_id(), Some("voice-orchestrator-full-id"));
        assert_eq!(view.orb_state(), OrbState::Listening);
        view.reduce(
            VoiceEvent::Levels {
                generation: 2,
                microphone: 0,
                speaker: 4000,
            },
            now,
        );
        assert_eq!(view.orb_state(), OrbState::Composing);
        let mut working = snapshot(2);
        working.work = VoiceWork::AwaitingInput;
        view.reduce(VoiceEvent::Snapshot { snapshot: working }, now);
        // The host never meters client media; its `playing: false` is stale.
        assert!(view.playing());
        view.reduce(
            VoiceEvent::Levels {
                generation: 2,
                microphone: 0,
                speaker: 0,
            },
            now + Duration::from_secs(1),
        );
        assert_eq!(view.orb_state(), OrbState::Solving);
        assert!(view.set_muted(true));
        assert_eq!(view.microphone_level(), 0.0);
    }

    #[test]
    fn stale_events_and_runaway_partials_stay_bounded() {
        let now = Instant::now();
        let mut view = VoiceView::new();
        assert!(!view.reduce(
            VoiceEvent::Partial {
                generation: 1,
                item_id: None,
                text: "early".into()
            },
            now
        ));
        view.reduce(
            VoiceEvent::Snapshot {
                snapshot: snapshot(2),
            },
            now,
        );
        assert!(!view.reduce(
            VoiceEvent::Snapshot {
                snapshot: snapshot(3)
            },
            now
        ));
        assert!(!view.reduce(
            VoiceEvent::Closed {
                generation: 1,
                reason: None
            },
            now
        ));
        for _ in 0..5 {
            view.reduce(
                VoiceEvent::Partial {
                    generation: 2,
                    item_id: Some("item".into()),
                    text: "x".repeat(MAX_TRANSCRIPT_BYTES / 2),
                },
                now,
            );
        }
        assert!(view.caption.len() <= MAX_TRANSCRIPT_BYTES);
        view.reduce(
            VoiceEvent::Final {
                transcript: VoiceTranscript {
                    session_id: "session".into(),
                    item_id: "item".into(),
                    role: VoiceRole::Assistant,
                    text: "done".into(),
                    promoted_message_id: None,
                },
            },
            now,
        );
        assert_eq!(view.caption, "done");
        assert_eq!(view.caption_role, Some(VoiceRole::Assistant));
        view.reduce(
            VoiceEvent::Partial {
                generation: 2,
                item_id: Some("item".into()),
                text: "next".into(),
            },
            now,
        );
        assert_eq!(view.caption, "next");
        assert!(view.reduce(
            VoiceEvent::Closed {
                generation: 2,
                reason: Some(VoiceRejection::Protocol)
            },
            now
        ));
        assert_eq!(view.phase, VoicePhase::Failed);
    }
}
