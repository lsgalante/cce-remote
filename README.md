# cce-remote

Use a phone as a trackpad + keyboard for the cce desktop.

A single small server: it serves an embedded web page (touch trackpad +
keyboard UI) over HTTP on your tailnet and bridges the page's WebSocket input
events into the compositor's control socket — the same channel `ccectl`
uses, so injection rides the real compositor input path.

The page is a cce-ui app compiled to WebAssembly, so it looks like the
desktop (its colours and font come from the DE's config). It draws with
WebGPU — on an iPhone that means iOS 26 or later — and only on a page served
over HTTPS (see below).

## Run

Building needs the `wasm32-unknown-unknown` target and the `wasm-bindgen`
CLI at the version in `Cargo.lock` (`cargo install wasm-bindgen-cli
--version 0.2.126`); `CCE_REMOTE_SKIP_WEB=1` builds without the app.

```sh
make install        # installs cce-remote to ~/.local/bin
cce-remote          # serves loopback + the tailnet on :17017 (or: cce-remote <port>)
cce-remote --lan    # every interface, plain HTTP (or CCE_REMOTE_LAN=1)
```

Only connections over the tailnet (or loopback) are served; anything else
gets a 403 saying so. Add the page to the Home Screen for a fullscreen app
feel.

The page needs HTTPS (a browser offers WebGPU only on a secure page; over
plain HTTP you get a note saying so). Point a DNS name you control at
the machine's tailnet address, then, with a Cloudflare API token for that
zone (Zone:DNS:Edit) in `~/.config/cce/cloudflare-dns.token`:

```sh
cce-remote-cert remote.example.com                 # Let's Encrypt via DNS; needs lego
systemctl --user enable --now cce-remote-cert.timer   # renews it
```

and open `https://remote.example.com:17017`. A renewed certificate is
picked up without a restart.

## Controls

- one-finger drag — move the pointer
- tap — left click; two-finger tap — right click
- two-finger drag — scroll (natural direction)
- press-and-hold, then drag — held drag (release on lift)
- `left` / `right` buttons — explicit clicks
- top bar — esc/tab/arrows; ctrl/alt/sup are sticky toggles (tap to hold,
  tap again to release — chords work: ctrl on, tap `c`, ctrl off)
- keyboard button — summon the phone keyboard (typing goes through a US-layout
  char→evdev map; iOS `beforeinput` is used, so autocorrect noise is
  filtered). A tap that opens a text field on the desktop raises it by
  itself; a field opened any other way shows a "tap to type" bar
- windows button (three bars) — window switcher: tap a window to focus it
- menu button (three dots) → live view — window view mode: a live stream of the focused window, delivered
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

Over plain HTTP the PIN crosses the wire in clear (over HTTPS it does not).
That is why only
loopback and the tailnet (100.64.0.0/10, fd7a:115c:a1e0::/48) are served by
default: WireGuard encrypts the tailnet end to end. `--lan` serves every
interface; use it only on a network you trust.
