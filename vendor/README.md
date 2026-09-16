# Local IronRDP security patches

`ironrdp-svc` is copied from the crates.io release 0.8.0 (archive SHA-256
`24c36b82ab0f7fef2668fb7004008a0f3c100a3d9a7b18bd4495a71aba55b796`).
The source, normalized manifest, README and MIT/Apache-2.0 licenses are retained.
The root `[patch.crates-io]` makes every dependency use this copy.

Local changes:

- Optional `StaticVirtualChannel::set_max_message_size` bounds both the advertised
  size and cumulative bytes before extending the incoming SVC buffer, including
  unfragmented messages. An overflow frees the partial buffer and returns an error.
- Unit tests are enabled and run in the RDP CI job.
- The RDP connector sets CLIPRDR's limit to 1 MiB plus its eight-byte message
  header before negotiation. These channel objects persist into the active session
  and through display reactivation. Protocol errors propagate out of the session
  task, which drops the connection and remaining channel state.

The cap does not allocate based on the peer's advertised size, and repeated FIRST
flags do not reset its accounting. Channels without a configured limit retain
upstream behavior. Existing clipboard format/text validation remains in place.

When upgrading IronRDP, retain these patches and tests until equivalent upstream
limits are available and configured before any peer traffic is processed.
