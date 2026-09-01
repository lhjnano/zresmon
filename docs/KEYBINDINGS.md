# Keybindings — tracking table

Every advertised key must actually work: this is the TUI equivalent of the
no-dead-buttons rule. The on-screen footer
(`q quit · ←/→/Tab pool · 1-9 jump · h/l panel · ↑/↓ scroll · read-only`)
must stay in sync with this table — a key advertised in the footer but
missing here (or vice versa) is a documentation bug.

For a TUI there is no HTTP API, so the **TUI contract** column records the
code-level equivalent (run-loop state mutation, `Source` call, or render
contract) that each key is wired to.

Source of truth: the key match in the run loop (the `KeyCode` arms) and
the IME fallback projection.

| Element | Type | TUI contract | Notes |
|---------|------|--------------|-------|
| `q` | quit | run-loop `break` (terminal restored by caller) | IME: `ㅂ` (and shift `ㅃ`) projects to `q` |
| `Esc` | quit | run-loop `break` | Not advertised in the footer (extra binding, not a dead button) |
| `→` | pool navigation | `active = (active + 1) % n` (wraps to first) | Arrow key — delivered regardless of IME state |
| `Tab` | pool navigation | same as `→` | |
| `←` | pool navigation | `active - 1`, wraps to last pool | Arrow key — IME-independent |
| `BackTab` (Shift+Tab) | pool navigation | same as `←` | Not advertised in the footer |
| `1`–`9` | pool jump | `active = idx` when `idx < snaps.len()`, else no-op | Out-of-range digits are inert by design (semantically disabled); digit row passes through a Korean IME natively |
| `h` | panel focus | `panel_focus = PANEL_TREE` (0) | IME: `ㅗ` projects to `h`; focused panel title renders cyan+bold with cyan border accent |
| `l` | panel focus | `panel_focus = PANEL_MAP` (1) | IME: `ㅣ` projects to `l` |
| `↑` | scroll | focused panel's offset `saturating_sub(1)` (`tree_scroll` or `map_scroll`) | Scrolls the panel selected by `h`/`l`; no-op at offset 0 |
| `↓` | scroll | focused panel's offset `+= 1` | Clamped to content height by the renderer |

All keys operate on per-tick data: the run loop re-collects every pool via
`Source::sample_pool` on each poll, so navigation state is always paired
with a fresh snapshot.

## Korean IME equivalences

Hotkeys keep working when a Korean two-set (두벌식) IME is on. The run loop
normalizes every character key through the IME fallback projection
(a position-based projection onto the QWERTY layout, NOT romanization):

| Delivered | Projects to | Binding |
|-----------|-------------|---------|
| `ㅂ` / `ㅃ` | `q` | quit |
| `ㅗ` | `h` | focus vdev tree |
| `ㅣ` | `l` | focus error map |
| digit row | ASCII digits natively | pool jump `1`–`9` |
| `Esc`/`Tab`/`BackTab`/arrows | (control keys) | delivered as-is by the terminal |
