# Remote voice development testing

The implementation is opt-in. Real Mac/Fedora and physical iPhone acceptance is
pending and will be performed by the user. Compilation and fake-provider tests
are not evidence of subscription/media compatibility. No production rollout or
server deployment is part of this change.

## Desktop

Start the execution host's new Zeron engine/app with `ZERON_REMOTE_VOICE=1` in
its environment. Codex must already be installed and authenticated through
ChatGPT on that host. It needs no audio helper, microphone or speaker. Continue
using the existing registered device and account; do not register another host.

On macOS ARM64, build the client bundle with an explicit pinned runtime package:

```sh
ZERON_REMOTE_VOICE=1 \
ZERON_VOICE_RUNTIME_PACKAGE=/absolute/path/to/codex-0.160.0-package \
ZERON_DEV_BUILD_ONLY=1 ./scripts/run-macos-dev.sh
```

The package directory contains `codex-package.json` and `codex-resources/voice`.
The packaging script verifies the pinned artifact and copies the complete media
subtree unmodified, with Codex's own signatures, to
`Contents/Resources/codex-resources/voice` — the helper refuses to initialize
from any directory not named `codex-resources/voice`. It does not put the Codex executable
or credentials in the client bundle. For direct developer runs,
`ZERON_VOICE_MEDIA_DIR` may explicitly point at a projected runtime containing
`zeron-runtime.json`; it must also be a `codex-resources/voice` directory. There is no automatic PATH fallback.

Open `target/macos-dev/Zeron Dev.app`, select **Settings → Voice → Codex voice
device**, choose the registered Fedora host and start voice from the sidebar orb.
The microphone permission belongs to this Mac. Host choice is local to these UI
settings and cannot change an active call. A host with an older build or disabled
remote gate is not eligible. Style is validated by the chosen host at Prepare.

With the flag unset, desktop retains its existing local Codex voice path. Disable
the flag and restart to roll back; no transcript migration or credential transfer
is needed. Do not remove the user's CLI to test the new path: use an isolated
client environment or copy the completed bundle to a test Mac without a CLI.

### Linux and Windows desktop clients

`scripts/package-linux.sh` and `scripts/package-windows.ps1` now include the
pinned media runtime beside the executable in `codex-resources/voice`. Both
x86_64 and ARM64 inputs are supported. The Windows ZIP and per-user setup carry
the whole runtime, including `bin/codex-voice-host.exe` and its DLLs. The Linux
tarball carries `bin/codex-voice-host` and its `.so`/GStreamer libraries.

Builds require Python 3.12 or later and either download the checksum-pinned
official source package on the build machine or use an extracted complete
package in `ZERON_VOICE_RUNTIME_PACKAGE`. See [packaging instructions](../../dist/voice/README.md).
Installed clients do not download a runtime, search PATH or require a local
Codex CLI for the remote media path. The selected execution host still needs
authenticated Codex and `ZERON_REMOTE_VOICE=1`.

Offline package checks:

```sh
python3 -m unittest discover -s scripts/tests -p 'test_voice_packaging.py' -v
cargo test --locked -p zeron-voice-media --lib
```

To check an actual projected runtime's initialization without opening audio
devices or contacting a provider:

```sh
ZERON_VOICE_MEDIA_DIR=/absolute/path/to/codex-resources/voice \
  cargo test --locked -p zeron-voice-media --lib \
  packaged_native_runtime_initializes_without_audio_devices -- --ignored
```

On Windows set `$env:ZERON_VOICE_MEDIA_DIR` before the same Cargo command.
Real microphone, playback, permissions, interruption and live-provider
acceptance still need to be performed on each physical platform.

## Acceptance checklist

- Mac audio and Fedora Codex, including a host without an audio device/helper.
- Bidirectional conversation, interruption, mute and immediate local hang-up.
- Two successive calls and MCP delegation to a different provider, with return
  messages addressed to the complete orchestrator chat ID.
- One final transcript copy after sync; no active call or audio on other devices.
- Host shutdown, owner disconnect, half-open connection and late callbacks.
- Cancel while preparing/negotiating, then immediately start another call.
- 20 start/stop cycles, one 15-minute call and devices on separate networks.
- Open/reopen from Settings and drag the topbar; retain existing orb smoothing.

Record versions, anonymized platform/network labels, observed outcomes and
monotonic durations. Do not record SDP, tokens, audio or private transcript text.
A failed negotiation should be investigated using the opt-in split-host smoke
and its sanitized stages; it does not imply trying an API-key fallback.

## Automated coverage

`voice-tests.yml` includes the new media/coordinator crates and explicit opt-in
fake-provider remote-engine tests. These require no live provider or microphone.
The new V2 controls travel over the existing relay and remain ephemeral; no audio
frames or volume-meter stream are added to sync or the command ledger.

## iOS

Build the Rust core with `scripts/ios/build-core.sh iphonesimulator` (or `iphoneos`
for a physical device). Open `apps/ios/Zeron.xcodeproj` and use the **Zeron** scheme.
Sign a physical-device build using your existing development team. No Codex
executable or OpenAI credentials are installed in the iOS app.

Voice appears by itself once a registered execution host advertises
`voice-client-media-v1` (the host needs `ZERON_REMOTE_VOICE=1` and authenticated
Codex); the `-remote-voice` launch argument still forces it on for development.
Tap the waveform at the end of the **New session** bar to call the chosen host —
or the only one online — and hold it to pick the host and voice. On iPad it sits
beside compose in the sidebar toolbar. **More → Settings → Voice** keeps the same
choices. Microphone permission is requested after checking the host capability.

During a call the bar becomes a live strip (orb, clock, mute, hang-up) and an
open session shows the live orb in its navigation bar; both open the full-screen
stage. The stage has the desktop orb, caption, transcript button (marked when
Codex is waiting for an answer) and mute/route/end. Mute and hang-up act locally
first. Like a phone call: in hand audio plays on the loudspeaker, held to the ear
the proximity sensor turns the screen off and moves it to the earpiece, and a
headset or car route always wins. Locking the screen keeps the call (background
audio); an interruption such as an incoming call ends it, without automatic
restart. Losing a headset re-routes instead of ending the call.

`-demo -voice-preview [-voice-stage]` (debug builds) plays a scripted call for
screenshots and UI work without audio, host or relay.

Offline iOS lifecycle tests (no microphone permission or provider):

```sh
scripts/ios/build-core.sh iphonesimulator
ZERON_SKIP_CORE=1 xcodebuild -project apps/ios/Zeron.xcodeproj -scheme Zeron \
  -destination 'platform=iOS Simulator,name=Zeron iPhone 17 Pro' \
  ARCHS=arm64 ONLY_ACTIVE_ARCH=YES CODE_SIGN_IDENTITY=- \
  -only-testing:ZeronTests/RemoteVoiceLifecycleTests test
```

Use an available ARM64 simulator name on your machine. The Rust build script
currently creates an ARM64 simulator archive; a generic x86_64 simulator build
cannot link that archive. Keep simulator ad-hoc signing enabled so it can launch.

## Local validation record

On macOS ARM64, the iOS simulator build and all six native lifecycle tests
passed (including the shared orb frames). Rust tests cover incompatible hosts before permission, cancellation
while an offer is stalled, a half-open heartbeat, local meters, owner drop,
duplicate prepare/negotiation, late controls and bounded UniFFI callbacks.
The full remote flow also passed with two engines over the WebSocket test relay
and a fake Codex package whose audio helper was removed. The test checks that
closing the owner releases the call without breaking the shared connection.

Live sound, Wi-Fi/cellular behavior, Bluetooth, battery/CPU, latency, cross-network
provider acceptance and real MCP delegation are still the user's acceptance tests.
The development flag stays opt-in until those pass.
