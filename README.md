# cce-remote

Use a phone as a trackpad + keyboard for the cce desktop.

A single small server: it serves an embedded web page (touch trackpad +
keyboard UI) over HTTP on your tailnet and bridges the page's WebSocket input
events into the compositor's control socket — the same channel `ccectl`
uses, so injection rides the real compositor input path.

## Run

```sh
make install        # installs cce-remote to ~/.local/bin
cce-remote          # serves loopback + the tailnet on :17017 (or: cce-remote <port>)
cce-remote --lan    # every interface, plain HTTP (or CCE_REMOTE_LAN=1)
```

Open `http://<this-machine's-Tailscale-address>:17017` on the phone (its
100.x address or MagicDNS name). Add it to the Home Screen for a fullscreen
app feel. A connection from anywhere else gets a 403 saying so.

## Controls

- one-finger drag — move the pointer
- tap — left click; two-finger tap — right click
- two-finger drag — scroll (natural direction)
- press-and-hold, then drag — held drag (release on lift)
- `left` / `right` buttons — explicit clicks
- top bar — esc/tab/arrows; ctrl/alt/sup are sticky toggles (tap to hold,
  tap again to release — chords work: ctrl on, tap `c`, ctrl off)
- ⌨ — summon the phone keyboard (typing goes through a US-layout
  char→evdev map; iOS `beforeinput` is used, so autocorrect noise is
  filtered)
- ☰ — window switcher: tap a window to focus it
- 🖥 — window view mode: a live stream of the focused window, delivered
  ack-clocked over a WebSocket — at most one frame in flight, so a slow
  link drops frame rate instead of falling behind — with resolution and
  quality adapting to the measured link (up to 1400px edge when it's fast).
  Frames come from the compositor's damage-driven window stream, falling
  back to wlr-screencopy, then grim; `/stream` remains as a curl-friendly
  MJPEG debug endpoint.
  Input is identical to the trackpad — tap = click, two-finger tap = right
  click, press-and-hold = held drag, one-finger drag = pointer motion (a
  cyan ring marks the cursor — compositor frames carry none); taps never
  warp the pointer. Two fingers pinch-zoom / pan the view itself. Toggle
  again for the trackpad. Both `/stream` and the one-shot
  `/shot` endpoint are PIN-gated; nothing accumulates on disk.

## Security

Pairing PIN: a persistent 6-digit PIN is generated on first run (printed at
startup, stored 0600 in `~/.config/cce/cce-remote.pin`). The page asks for
it once per device and remembers it (localStorage); the server closes any
WebSocket whose first frame isn't `auth <pin>`, so no input can be injected
without pairing. Delete the PIN file to rotate it.

Wrong PINs are rate-limited per source address — five in a row, then one more
every 30 seconds (HTTP replies `429 Too Many Requests`) — and globally, twenty
in a row across every address, then one per 30 seconds, so the 6-digit space
can't be walked from one address or from many. Correct PINs cost nothing and a
success clears the peer's record, so a phone reconnecting its stream is never
throttled — unless someone has drained the global budget, which locks everyone
out until it refills.

Traffic is plain HTTP, so the PIN crosses the wire in clear. That is why only
loopback and the tailnet (100.64.0.0/10, fd7a:115c:a1e0::/48) are served by
default: WireGuard encrypts the tailnet end to end. `--lan` serves every
interface; use it only on a network you trust.
