# Generated images in conversation threads

Codex `imageGeneration` uses `savedPath` as its only source. The engine imports a
PNG, JPEG, WebP, or GIF of at most 24 MiB into the active profile's uploads root
before publishing the event. Neither the original path nor `result` enters the
journal or session document. Source files remain owned by Codex.

The transcript reads through the existing attachment RPC/cache. It tries the
message's device first, then the chat host and local device, without duplicate
candidates. Raster decoding accepts at most 4096 × 4096 pixels and 64 MiB of
allocation, and retains one static frame. The lightbox is shared with user
attachments. Media bytes stay on the host; an offline host shows an unavailable
placeholder and uses the existing 2–15 second retry ladder.

## Automated checks

```sh
cargo test -p zeron-proto -p zeron-doc
cargo test -p zeron-engine generated_image
cargo test -p zeron-engine --test e2e --test device_routing --test workspace_sync
cargo test -p zeron-harness codex
cargo test -p zeron-harness --test codex
cargo test -p zeron-ui transcript
cargo test -p zeron-ui attachments
cd edge && npm test
```

The engine fixture covers repeated completions, restart-style Loro export/import,
removal of the original Codex source, replay after removal, and negative journal
assertions. Upload tests cover supported signatures, size/path/symlink rejection,
source replacement during copying, atomic cleanup and idempotent destinations.
The two-device routing test reads the imported file locally and via the relay,
and verifies that the original source cannot be read remotely.

`crates/ui/tests/fixtures/generated-images.json` supplies loaded, loading and
unavailable entries. Transcript tests cover their rows, copy/timestamps, owner
and metadata corrections, cache reuse, retry scheduling, and clicking through to
the lightbox with Escape restoring focus. The chunk-reader test verifies the
RPC target, MIME validation and dimension limits. These are structural and
interaction checks; they are not visual snapshot approval.

## Optional real-provider smoke (consumes quota)

These tests stay ignored in normal runs. They require an authenticated Codex
installation and a model/account provisioned for image generation.

```sh
cargo test -p zeron-harness --test codex real_image_generation_smoke -- --ignored
cargo test -p zeron-engine --test e2e real_image_generation_profile_smoke -- --ignored
```

The harness smoke checks that the real provider returns an existing saved file;
the engine smoke also requires the persisted path to be under profile uploads.
No real-provider smoke or manual visual check was performed as part of the
implementation's automated tests.

## Manual visual check

Run a normal Codex chat in both light and dark themes, including a narrow window:

1. Generate a goblin PNG. Check the live chip, resolved chip, inline image,
   rounded corners, contained aspect ratio, and lightbox click/Escape focus.
2. Generate two images with text before and after. Check order, spacing and
   timestamp placement. Small images should keep their natural size.
3. Change chat and return, then restart. The durable image should reappear.
4. Open the thread on another client. Disconnect the owning host and check
   the unavailable placeholder without affecting other message parts.
5. Exercise the fake provider's quota-error and missing-path scenarios. Neither
   should leave an unresolved chip or produce an empty image part.

For the generated turn, inspect the profile journal/document for the uploads
reference and absence of the original Codex path, inline `result`, data URLs or
image Base64. Other user-authored content may legitimately contain those strings.

## Implementation validation (2026-09-14)

- Proto/document, generated-image engine tests, e2e/device routing, Codex fake
  integration, transcript and attachment tests pass. Edge's 54 unit/workerd
  tests and TypeScript typecheck pass.
- The workspace run passed 1,826 tests (21 intentionally ignored) with
  `--skip online_runtime_shutdown_stops_edge_workers_and_retires_the_graph`.
  The unfiltered run and an isolated retry failed that shutdown assertion;
  the same failure was reproduced three times using a test binary built from
  the original `1ab04195` checkout. No shutdown behavior was changed here.
- `cargo fmt --all -- --check` reports pre-existing formatting differences in
  10 files; each affected file also fails formatting in the original checkout.
- Strict `cargo clippy --workspace --all-targets -- -D warnings` stops on
  pre-existing `zeron-theme` lints. A full run with `--cap-lints warn` completes
  and preserves the existing warnings for review.

The full workspace command used to continue past the confirmed baseline failure:

```sh
cargo test --workspace -- --skip online_runtime_shutdown_stops_edge_workers_and_retires_the_graph
```
