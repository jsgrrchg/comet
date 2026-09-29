# Side chats

Hover a completed Codex, Claude Code, or OpenCode reply and choose **Fork in side chat**, immediately to the left of Copy. The same controls are reachable by keyboard. Zeron opens a saved side chat with an empty composer and the original conversation through that reply. Creating it does not send a prompt or run the model.

The new conversation resumes an independent native provider session. Later turns in the original conversation are excluded. The original chat keeps running. Both chats use the same execution device, provider, and current checkout; this action does not restore files or create a worktree. The provider picker stays locked in the saved side chat.

A fork made inside a side chat appears as a sibling under that chat's existing root. Replies inherited from an earlier fork retain their original native provenance. Forking one of those replies uses the native session that actually contains its boundary; new replies belong to the child's session.

The fork icon is hidden when a reply has no recorded native boundary or the execution host does not support native message forks. This includes older replies without a mapping and hosts that need updating. The icon appears automatically when both become available. Other availability failures, such as an unverified provider contract, keep the icon disabled with an explanation. Codex boundaries correspond to completed provider turns, so a segment closed during steering may have no native boundary. Zeron never substitutes a textual context copy for this action.

If the child's native session has disappeared or cannot be resumed, sending fails explicitly. If a fork request loses its provider response, Zeron reports an indeterminate outcome and does not repeat creation automatically. A confirmed result can be retried safely using the same operation identity. Closing or changing panels does not cancel persistence or redirect the result to a different conversation.

Existing empty-side-chat and context-copy actions keep their existing behavior. See [native fork regression evidence](regressions/native-message-forks.md) for supported contracts, tests, and limitations.
