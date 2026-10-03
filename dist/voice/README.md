# Pinned media runtime notices

`Codex-LICENSE.txt` is the unmodified OpenAI license at source commit
`a956835d020762cb2b570053af06f643a11c0ecc`:
https://github.com/openai/codex/blob/a956835d020762cb2b570053af06f643a11c0ecc/LICENSE

The packager retains the full voice subtree, dependency notices and sources
manifest and adds this helper license. Binaries and libraries are copied
byte-for-byte; macOS signatures are verified and preserved. It does not copy
the Codex CLI or authentication files.

Linux and Windows package builds download the checksum-pinned official Codex
`0.160.0` package on the build machine, or use the explicit directory provided
in `ZERON_VOICE_RUNTIME_PACKAGE`. Packaging requires Python 3.12 or later.
No runtime download occurs on the user's device.

| Zeron package | Media target | Runtime location |
| --- | --- | --- |
| Linux x86_64 | `x86_64-unknown-linux-gnu` | Beside `zeron`, in `codex-resources/voice` |
| Linux ARM64 | `aarch64-unknown-linux-gnu` | Beside `zeron`, in `codex-resources/voice` |
| Windows x86_64 | `x86_64-pc-windows-msvc` | Beside `zeron.exe`, in `codex-resources/voice` |
| Windows ARM64 | `aarch64-pc-windows-msvc` | Beside `zeron.exe`, in `codex-resources/voice` |
| macOS ARM64 | `aarch64-apple-darwin` | `Contents/Resources/codex-resources/voice` |

Linux's CLI source package has a `*-unknown-linux-musl` app target; its bundled
audio helper has a `*-unknown-linux-gnu` target and requires the host's glibc
and ALSA system libraries. The complete bundled `.so`/GStreamer tree is retained.
Windows packages retain the `.exe`, DLLs and other runtime resources. The
packager validates the source version, source commit, architecture, file hashes
and library inventory; the client checks target and hashes before execution.

The Linux tarball contains the runtime, so installing or updating that tarball
retains it. On Windows the portable ZIP and per-user installer contain it; the
standalone updater `.exe` is still executable-only and preserves an existing
runtime. Older Windows installs without these resources need the ZIP or setup
once to acquire them. This change does not replace the Windows updater.

For an offline build, provide the already extracted **complete** official
standalone package (including its `codex-package.json` and `codex-resources`):

```sh
ZERON_VOICE_RUNTIME_PACKAGE=/absolute/path/to/codex-package-x86_64-unknown-linux-musl \
  scripts/package-linux.sh
```

```powershell
$env:ZERON_VOICE_RUNTIME_PACKAGE = 'C:\build\codex-package-x86_64-pc-windows-msvc'
.\scripts\package-windows.ps1 -ReleasesUrl 'https://github.com/zeronsh/comet/releases/latest/download'
```

The packager can also project a development runtime directly, without building
Zeron, with `--package` or the pinned `--download` input:

```sh
python3 scripts/package-voice-runtime.py --download \
  --target x86_64-unknown-linux-gnu --destination /tmp/zeron/codex-resources/voice
```

`ZERON_VOICE_MEDIA_DIR` may point at that projected `codex-resources/voice`
directory for direct development runs. Remote voice remains opt-in through
`ZERON_REMOTE_VOICE=1`; packaging does not change authentication, billing or
the session gate. Physical microphone and live provider acceptance remain
separate from package validation.
