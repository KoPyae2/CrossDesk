# CrossDesk

One keyboard, mouse and clipboard across every machine on your WiFi. Push the
pointer off the edge of one screen and it carries on onto the next machine — the
keyboard follows it, and anything you copy is available on all of them.

Local network only. No account, no relay, no cloud.

## How it works

One machine **hosts**: it owns the real keyboard and mouse, and it does all the
coordinate maths. The others are **clients**: they replay absolute coordinates
the host has already translated into their own pixels. That split is deliberate —
the client's hot path is "decode a frame, hand it to the OS", with no layout
decisions in it.

- **Capture** on the host suppresses local input while the pointer is away and
  reads true device deltas, so motion keeps working past the edge of the desktop
  where the OS cursor stops moving.
- **The wall** is one shared coordinate space with each device's monitors placed
  in it. Crossing a boundary is a containment test, not an edge state to get
  stuck in.
- **Transport** is postcard over TCP with `TCP_NODELAY`, encrypted with
  ChaCha20-Poly1305 after an X25519 handshake. Every peer has its own write
  queue, so one bad WiFi link cannot slow the pointer down for anybody else.
- **Pairing** mixes the host's code into key derivation: a wrong code fails
  authentication rather than being compared anywhere. On success the host issues
  that device a long-term key, so the code is typed once.
- **Discovery** is a UDP query on port 47811; hosts answer, clients don't poll.

## Platform support

| | Host | Client |
| - | ---- | ------ |
| Windows 10/11 | yes | yes |
| macOS | not yet | yes |
| Linux | not yet | yes |

Any machine can be a client — that side only has to inject what it is told, which
works everywhere. Hosting is the part that needs platform-specific code:
capturing every keystroke system-wide *and* suppressing it locally *and* reading
mouse deltas past the edge of the desktop. Only the Windows backend
(`WH_*_LL` hooks plus Raw Input) exists today, so a non-Windows machine opens
straight onto the client panel and says so. Nothing else in the app is
Windows-specific — the wall, transport, pairing, discovery and clipboard are all
portable, so adding a `CGEventTap` or evdev backend would be the whole job.

## Using it

On the machine with the keyboard you want to use, pick **Share this machine** and
**Start hosting** (a Windows machine, for now — see above). On every other machine
pick **Follow another**, choose the host from the list, and type the code shown on
the host's screen.

Then drag the devices in the layout editor so they match how the screens really
sit on your desk. **Snap to edges** tidies up the gaps. Positions are remembered
per device, including across reconnects and restarts.

<kbd>Ctrl</kbd>+<kbd>Alt</kbd>+<kbd>Home</kbd> brings the pointer back to the
host from anywhere — it works even while every key is being suppressed, which is
the point.

### macOS clients need Accessibility

macOS will not let any app move the pointer or type until it is on the
Accessibility list, and it refuses in **silence** — the API that posts an event
reports nothing, so a client without permission connects, looks connected, and
moves nothing.

Press **Grant permission** on the client panel. That does the part that actually
matters: it asks macOS for access, which registers *this* copy of CrossDesk in
the list, and then opens the pane so you can see it. Adding the app by hand with
**+** is the usual way to end up with a permission that does nothing, because the
path you pick may be a different build from the one that is running.

If the panel says permission is granted but nothing moves, press **Test pointer**.
It nudges the pointer and reads the position back, which is the only answer that
cannot be wrong. A grant that fails that test belongs to an older build: remove
CrossDesk from the list with **−**, press **Grant permission** again, and restart
the app. The warning clears by itself once the permission is real — there is
nothing to reconnect.

## Development

```sh
npm install
npm run tauri dev      # run
npm run tauri build    # package
cd src-tauri && cargo test
```

Requires the Rust toolchain and the [Tauri v2
prerequisites](https://tauri.app/start/prerequisites/) for your platform. A client
on Linux needs X11 (`XTEST`) to inject input; on macOS, see the Accessibility
section above.

## Ports

| Port | Protocol | Purpose |
| ---- | -------- | ------- |
| 47810 | TCP | input, clipboard, layout |
| 47811 | UDP | discovery |

Both need to be allowed through the host's firewall on your local network.
