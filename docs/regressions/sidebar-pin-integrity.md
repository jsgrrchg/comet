# Sidebar pin integrity

## Storage and scope

Synced profiles store one `sidebarPins/{sessionId}` row per pin in the per-user/org registry. Independent `pinned` and `orderKey` fields use the existing per-field logical clocks. Moving a pin writes only its position; unpinning writes explicit false without deleting its row. A delayed move cannot re-pin an item. Concurrent moves of different pins survive independently; concurrent moves of the same pin resolve by their clocks. Pin operations never rewrite sessions or normal activity ordering.

Local profiles retain profile-keyed device preferences. Personal cut adds a one-time migration per synced registry (user/org): after an authoritative snapshot, the old `preferences/sidebar-v1` list becomes individual pins in its original order. Deleted sessions and duplicates are skipped; archived sessions remain pinned. An explicit empty legacy registry list takes precedence over old device settings.

`personalCutLegacyImported` on `preferences/sidebarPins` is the durable, replicated completion marker. New-format records, including unpin tombstones and pre-existing explicit empty state, win over legacy data. A fresh personal-cut readiness placeholder carries `personalCutLegacyPending` so the desktop may supply its profile-keyed local list if no legacy registry list exists. The engine persists this import before acknowledging it; a failed save can be retried. The UI attempts once per engine/profile attachment, waits for sync, ignores replies for previous attachments, and retains the original local list as a backup. Registry sources are also retained, never continuously re-imported. Ordinary new-format pin edits close the migration window.

Rust and Swift migrate registry lists with identical ordinal hexadecimal keys and the original list's field clock. Pins and marker form one batch, so concurrent imports converge and a delayed import cannot beat a causally later unpin. Malformed/oversized sources are retained without partially importing them. No compatibility writer for whole-list pin mutations remains. Update desktop/host together; old clients still use a different pin schema.

The `preferences/sidebarPins` row records readiness only, never membership or order. Once an authoritative snapshot arrives, even an empty list is initialized so it remains editable offline after restarting. Cleanup runs against the same locked authoritative chat/pin snapshot, never independent UI watch streams. Archived chats keep their pins; deleted chats are unpinned individually.

## Ordering keys

Rust and Swift use the same byte-ordered, variable-length hexadecimal algorithm. It allocates a prefix interval strictly between adjacent keys, then appends a fixed-width hexadecimal encoding of the unique operation HLC, padded to 149 bytes, plus a nonzero terminator. Distinct operation clocks cannot generate identical keys. The complete key participates in allocation, so another item can be inserted between keys generated concurrently in the same gap. No floating-point arithmetic or random jitter is used.

Keys are bounded at 8192 ASCII bytes, below the registry operation-size budget. Exhaustion fails explicitly rather than truncating, colliding or rewriting unrelated pins. Edits observe the affected pin's clocks before generating a later local operation, including clocks ahead of local wall time. Uniqueness relies on the registry's existing device identity and persisted monotonic clock assumptions.

The maximum of 200 is an admission limit for new pins, including hidden and archived pins. Simultaneous offline additions can exceed it; all existing pins remain visible, reorderable and removable. Cleanup never truncates overflow. Further additions are rejected until capacity is available.

## Interaction and persistence

Menu and drop paths validate profile identity, readiness, IDs and capacity before changing anything. Unknown preferences are not editable. Rejected drops animate back; successful drops do not replay the activity-sort animation. Moving a visible pin preserves every other pin's relative order, including hidden and archived pins.

Dragging changes only the preview. Each accepted drop queues one pin, unpin or move intent, not a replacement list. Neighbor IDs are resolved against current state by the engine: a surviving right anchor takes precedence, otherwise the left anchor is used; if both disappear, append. Moving an already unpinned item is a no-op.

Pending intents are projected over the latest confirmed state. Unrelated mutations cannot cancel their task. Writes are serialized per engine attachment. Failure removes the affected optimistic entry and reveals current confirmed state plus remaining intents, never a stale full-list backup. Old profile/attachment/queue replies are ignored. Operations use the existing durable pending outbox and snapshot persistence; engine acknowledgement is not proof of remote delivery.

Watch and acknowledgement revisions are monotonic within an engine attachment, not synchronized order keys. Older messages cannot replace a newer confirmed snapshot. A timeout cancels unsent edits and blocks new writes until the original request resolves or its attachment is replaced. Late confirmations are still reconciled.

## Regression commands

```sh
cargo test --locked -p zeron-proto --lib
cargo test --locked -p zeron-doc --lib
cargo test --locked -p zeron-engine --lib
cargo test --locked -p zeron-ui --lib -- --test-threads=1
cd edge && npm run typecheck && npm test
```

Run `ZeronTests/PinnedSessionsTests` on an iOS simulator for Swift key parity, dense insertion, membership, persistence and ignored old preferences. Registry tests cover independent concurrent moves, move/unpin races, duplicated delivery, restarts and capacity overflow. Workerd tests verify single-pin writes and unchanged session activity on real SQLite. Native UI and RPC tests cover filtered movement, drag gaps, empty targets, animated returns, successful-drop animation suppression, serialization, failures, timeouts and late acknowledgements.
