Pi RPC compatibility contract

The native driver targets Pi >= 0.85.1. Requests and events are JSONL, never
JSON-RPC. Responses and lifecycle events must retain stdout order.

`prompt` success acknowledges preflight. Normal runs finish at `agent_settled`,
not `agent_end`; a final error message determines the terminal status after
retries. Extension commands and input handlers may acknowledge without a run.

Legacy ACK barrier (verified against installed 0.85.1 and inspected 0.87.1):
after prompt acceptance request `get_state`. In these versions AgentSession's
`isStreaming` is `_isAgentRunActive`, covering retries/post-run continuations.
The synchronous path sets it immediately after the preflight callback, before
another stdin command can execute. A false isStreaming AND false isCompacting
at this ordered barrier closes a handled prompt (or a run already settled).
This is NOT polling for idle or a quiet timeout. Process all preceding events
before the response, and scope barriers to the submitted execution. A future
`data.disposition` is additive; the state barrier also catches extension-started
work, which can accompany a handled disposition.

Independent work started later by an extension is a new run (`agent_start`).
Discovery/startup timeouts diagnose startup only; no active model run is timed
out for lack of output. Interruption clears queues before aborting.

Sources inspected: Pi dist/core/agent-session.js and dist/modes/rpc/rpc-mode.js
0.85.1, and upstream commit f07218c4d4bbc12bef056a7058c3dd49dfe41abe.
