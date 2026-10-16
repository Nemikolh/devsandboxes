# 0034: The dashboard owns mouse selection and copies through OSC 52

Status: accepted

## Context

The dashboard turns on mouse capture for clicks, drags and the wheel, which
disables the outer terminal's own selection. Shift-bypass selects whole
screen rows across panes and borders. The dashboard often runs over ssh or
inside a container, where no host clipboard API is reachable.

## Decision

- Selection is the dashboard's: `tui/select.rs` extracts and highlights
  text per pane, `app/selection.rs` routes mouse events; inner apps in the
  integrated terminal that track the mouse get the events forwarded
  instead.
- Copying writes OSC 52 (`ESC ] 52 ; c ; <base64> BEL`) to stdout
  (`tui/clipboard.rs`), wrapped in DCS passthrough under tmux, with a small
  inline base64 encoder and a size cap. Inner apps' own OSC 52 copies are
  relayed.

## Consequences

- Copy works over ssh and from containers wherever the outer terminal
  supports OSC 52; terminals without it can't copy from the dashboard.
- No clipboard crate (`arboard`/`xclip` wouldn't reach the user's machine
  from a container anyway).

Source: docs/tui-selection.md, "Problem", "Design" (Clipboard: OSC 52).
