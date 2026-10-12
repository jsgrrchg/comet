Native GPUI screenshots of a split view from the isolated `sidebar-fixture`
example. All projects and chats are synthetic; no engine or account is connected,
so the panes' transcripts are empty.

```sh
ZERON_SIDEBAR_SPLIT=1 cargo run -p zeron-ui --example sidebar-fixture --features project-palette-fixture
ZERON_SIDEBAR_SPLIT=1 ZERON_SIDEBAR_COMPACT=1 cargo run -p zeron-ui --example sidebar-fixture --features project-palette-fixture
```

These were captured on a headless Weston (`--backend=headless --renderer=pixman
--fake-seat`) with `ZERON_FIXTURE_OPAQUE=1`, which keeps the window opaque where
no compositor blurs behind it; `ZERON_PALETTE_LIGHT=1` selects the light theme.

- `detailed.png`, `compact.png`, `compact-light.png`: the selected session open
  in a split with another. Each pane has its own transcript and composer, and
  its title sits in the titlebar above it, the focused pane's raised. The
  sidebar lists the split once, as a row of pills where its first member's row
  would be; compact rows keep one line, detailed rows add each chat's status or
  time under its title.

Behaviour covered by headless tests (`shell/split_view_tests.rs`,
`shell/chat_dropzone_tests.rs`, `split_view.rs`): panes follow the selection and
hand drafts back and forth with the main composer; focus moves between panes
without opening or closing the right pane, explorer or terminal, whose content
follows the focused chat; dividers resize panes down to 240px; closing panes
dissolves a split left with one chat; archived chats leave their split; the
sidebar, cycling and jump shortcuts count a split as one slot; a session carried
over a conversation mentions it from the composer and opens beside it from
either half of the transcript; pills carried onto a pane rearrange the split.
