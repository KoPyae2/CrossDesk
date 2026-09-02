# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

CrossDesk is a software KVM: one machine's keyboard, mouse and clipboard drive
every other machine on the LAN. Tauri v2 + React 19 shell over a Rust core. No
account, no relay, no cloud — discovery and transport are local-network only.

Latency is the design constraint that decides arguments here. When a change
trades a few microseconds on the input path for cleaner code, the microseconds
usually win, and the surrounding comments say why.

## Commands

Frontend commands run from the repo root; Cargo commands from `src-tauri/`.

```sh
npm install
npm run tauri dev            # full app (spawns vite on :1420, then cargo run)
npm run tauri build          # package
npm run build                # tsc && vite build — frontend only
npx tsc --noEmit             # typecheck without emitting

cd src-tauri
cargo check --all-targets
cargo test
cargo test layout::tests::snapping_closes_the_gap_on_both_axes   # one test
cargo clippy --all-targets
cargo fmt
```

Tests live in `src-tauri/src/layout.rs` and `src-tauri/src/permission.rs` — the
two modules whose logic is pure enough to test without real input devices or two
machines. (`permission.rs`'s tests cover the wording it chooses, not the OS calls.)

The four checks worth running after a change that touches both sides:
`cargo check --all-targets`, `cargo test`, `npx tsc --noEmit`, `npm run build`.

## Architecture

### The host does the thinking; clients are dumb replayers

`layout.rs` defines the **wall**: one shared coordinate space in which every
device's monitors are placed at an offset. The host runs all of this. Crossing
from one machine to another is a containment test against the wall, not an "at
the edge" state machine, so there is no edge state to get stuck in.

A client never learns the wall exists. It receives `InputEvent::MoveAbs` with
coordinates *already translated into its own virtual-desktop pixels*, so its hot
path is "decode a frame, hand it to the OS". Keep it that way — layout logic on
the client side would need the wall replicated and kept in sync.

Consequences worth knowing before editing:

- Motion that lands in a gap between devices stays on the current device. That
  makes any gap an uncrossable wall, which is why `AUTO_GAP` is 0 and why
  `snap_edges` resolves the two axes independently (`layout.rs`).
- Hit-testing uses individual monitors (`Device::wall_displays`), not the
  bounding box, so an L-shaped setup doesn't swallow the pointer in its notch.
- A device that goes offline keeps its place in `Wall::devices` with
  `online: false`: still draggable in the editor, but not crossable.

### Roles are exclusive, and `Drop` is load-bearing

`lib.rs` holds `Role::{Idle, Host, Client}` behind one mutex. Starting either
role tears the other down — hosting and following at once builds an input loop.

Teardown lives in `Drop for Host` / `Drop for Client`, not in the stop command,
because the damage a skipped teardown leaves is real: input still suppressed on
the host, or keys still held down on a client. This is also why `run()` hooks
`WindowEvent::Destroyed` → `stop_role`. Anything that must happen on the way out
belongs in `Drop`.

Per-connection tasks are *not* owned by the role struct (they're spawned per
peer), so they can't be dropped — they're told to stop via `stopping: AtomicBool`
plus `tokio::sync::Notify`, with the flag checked after registering interest so a
stop can't be lost in the gap.

### Windows capture needs two mechanisms

`capture/win.rs` runs `WH_MOUSE_LL`/`WH_KEYBOARD_LL` hooks **and** Raw Input on
one dedicated high-priority thread with a message-only window:

- Hooks are the only way to **suppress** input without a driver (return non-zero).
- Hook coordinates are clamped to the desktop, so they stop changing exactly when
  the wall needs to know the user kept pushing outward. **Raw Input** (`WM_INPUT`)
  supplies true device deltas and drives all crossing decisions.

Hook callbacks run inline on every input event in the system and Windows silently
unhooks one that is too slow: they do a channel send and nothing else — no
allocation, no locking beyond that send.

Injected events carry `INJECTED_TAG` in `dwExtraInfo`, and both the hook and the
Raw Input path filter on it, so a machine never captures its own synthetic input.

Modifier state is tracked *inside* the hook (`MODIFIERS`) because the escape
hotkey Ctrl+Alt+Home has to work while every key is being suppressed.

### Platform coverage

Host: Windows only — `capture/` has just `win.rs`, and `capture::supported()`
(`cfg!(windows)`) is surfaced as `Snapshot::can_host` so the UI disables the host
tab instead of failing on the button. Client: everywhere, via `inject/other.rs`
(enigo). Nothing else in the app is platform-specific, so a `CGEventTap` or evdev
capture backend is the whole remaining job for host-anywhere.

`inject/other.rs` translates Windows virtual-key codes into enigo keys, because
the host speaks in whatever its hooks reported.

### macOS refuses input in silence, so `permission.rs` asks

`CGEventPost` returns `void`: a refused injection is indistinguishable from a
delivered one at the call site. A Mac client without Accessibility therefore
connects, reports itself connected, counts packets and moves nothing. Nothing
fails, so nothing can be reported from the injection path — which is why
permission is a separate question rather than an error.

Three things in that module exist because of a specific way the naive version
misleads the user:

- `status()` accepts **either** `CGPreflightPostEventAccess` or
  `AXIsProcessTrusted`. Asking only the latter is the bug behind "I already gave
  permission and it still says permission required": the answer a *running*
  process gets from the Accessibility list does not reliably flip when the box is
  ticked. Whichever record has caught up wins. Neither call prompts, which is what
  makes it safe on the 250 ms UI tick — and `spawn_ui_tick` polls it, because a
  grant made in System Settings happens entirely outside this process and nothing
  else would mark the state dirty.
- `open_settings()` calls `CGRequestPostEventAccess` **first**. That is what
  registers the running binary in the list. Adding the app by hand with **+** means
  picking a path, and the wrong path (the packaged `.app` rather than the binary
  `tauri dev` is running, or a stale copy) grants permission to something that is
  not running — the list shows CrossDesk enabled while every event is dropped.
- `probe()` moves the pointer, sleeps `SETTLE`, and reads it back, because the OS's
  answer can be wrong in *both* directions — macOS keys a grant to a code signature
  and bundle path, so a rebuilt unsigned binary is a different identity. Movement
  is the ground truth, and the UI lets `probe.moved` outrank `snap.input_access`.
  Two nudges in opposite directions: a pointer already in a corner cannot move
  further that way, and a clamped move looks exactly like a refused one.

The surface is `Snapshot::input_access` plus the `test_input` and
`request_input_access` commands; `ClientView.tsx` renders it as an advisory
notice, never a block. Two consequences to preserve:

- `InjectError::Blocked` is **not** fatal. `Injector` holds `Option<Backend>` and
  reopens it every `REOPEN_AFTER`, so a grant made while the window is open takes
  effect without reconnecting. The held-key ledger is updated *before* the backend
  is consulted, so a grant landing mid-drag does not inherit a phantom press.
- enigo's `open_prompt_to_get_permissions` defaults to **true** and `Enigo::new`
  prompts. `warp`, `cursor_position` and every reopen build a fresh `Enigo`, so the
  default would raise a system dialog per pointer read. `inject/other.rs::settings()`
  turns it off; asking is a deliberate user action instead.

### Wire protocol

`protocol.rs` is the contract. **postcard**, a non-self-describing binary format:
field and variant *order is part of the protocol*. Only ever append variants.

`transport.rs` frames as `u32` LE length + ChaCha20-Poly1305 ciphertext over TCP
with `TCP_NODELAY` (Nagle delay on a mouse-move packet is the exact stutter this
app exists to avoid). Nonces are a local counter never sent on the wire, so a
replayed or reordered frame fails authentication and kills the connection.

Pairing (`crypto.rs`): the code is mixed into key derivation, so a wrong code
yields different session keys and fails at the confirmation step — codes are
never compared. On success the host mints a random 32-byte PSK over the encrypted
channel, so the code is typed once per device. Clients try the stored key first
and fall back to the typed code, which is what makes a re-paired host still work.

Discovery (`discovery.rs`) is a UDP query/response on 47811: clients broadcast,
hosts answer. Broadcast goes out per-interface — a machine on both WiFi and
Ethernet has two broadcast addresses, and 255.255.255.255 gets dropped by plenty
of stacks. Ports: 47810/TCP, 47811/UDP.

### Concurrency rules

- **Lock order in `host.rs` is wall → peers → settings.** Never take them in
  another order. `lib.rs` takes role → settings, matching it.
- One writer task per peer with an unbounded channel, so a peer on bad WiFi
  cannot slow the pointer down for anyone else.
- `Peer::session` is a generation counter: a reconnecting device replaces its own
  entry, and the older connection's cleanup checks the session before removing
  anything so it can't tear down its successor.
- `Shared::disconnect_peer` only queues `HostMsg::Bye`. Closing the writer makes
  the reader fail, and the normal per-connection cleanup does the removal —
  removing it directly would race that cleanup.
- Resolve state under one lock, then do I/O outside it (see `on_motion`).

### UI cost control

Input arrives thousands of times a second; React must not see that rate.

- Backend sets a `dirty` flag; one timer emits `crossdesk://state` at most every
  250 ms (`spawn_ui_tick`). It uses `swap`, not load-then-clear, so a change
  landing between the two isn't dropped.
- Every command returns a fresh `Snapshot`, so a click updates immediately; the
  tick carries whatever changed on its own. `App.tsx` keeps a `generation` ref so
  an in-flight tick can't overwrite a newer command reply.
- **Layout drags bypass all of that.** `move_device` places the device, flags the
  UI, returns nothing — no snapshot (that would re-enumerate monitors per frame)
  and no persistence (a disk write per frame). `commit_layout` writes once on
  pointer-up. The frontend coalesces to one `move_device` per
  `requestAnimationFrame`, and `api.move` deliberately skips the `busy` flag so
  the buttons don't flicker.
- `LayoutCanvas` freezes its fit transform for the duration of a drag; otherwise
  dragging a device outward grows the wall, rescales the view, and shifts the
  device out from under the pointer moving it.

### Conventions

- `types.ts` mirrors the Rust structs with **snake_case** fields, because serde
  sends them exactly as declared. Tauri command *arguments*, however, are
  camelCase from JS (`deviceId` → `device_id`).
- Device ids are `[u8; 16]` in Rust and lowercase hex strings everywhere they
  cross a boundary (`protocol::id_to_hex`).
- Settings (`settings.rs`) are JSON in the per-user config dir, saved
  write-then-rename. The file holds the paired PSKs, so it is as sensitive as a
  password store and is created owner-only where the platform allows it.
- Comments here explain *why*, especially where an obvious simplification would
  reintroduce a latency or correctness bug. Match that when editing; don't strip
  a comment that encodes a constraint.

