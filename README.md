# cce-remote

Use a phone as a trackpad + keyboard for the cce desktop.

A single small server: it serves an embedded web page (touch trackpad +
keyboard UI) over HTTP on the LAN and bridges the page's WebSocket input
events into the compositor's control socket — the same channel `ccectl`
uses, so injection rides the real compositor input path.

## Run

```sh
make install        # installs cce-remote to ~/.local/bin
cce-remote          # serves on 0.0.0.0:17017 (or: cce-remote <port>)
```

Open `http://<this-machine's-LAN-IP>:17017` on the phone. Add it to the
Home Screen for a fullscreen app feel.

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
- 🖥 — window view mode: a live MJPEG stream of the focused window
  (damage-driven wlr-screencopy: ~11 fps when the window is active, idle
  throttled to output damage; grim remains as a fallback path).
  Tap to click that spot, long-press to right-click; one-finger drag moves
  the pointer exactly like the trackpad (a cyan ring marks the cursor —
  compositor frames carry none), and two fingers pinch-zoom / pan the view
  itself. Toggle again for the trackpad. Both `/stream` and the one-shot
  `/shot` endpoint are PIN-gated; nothing accumulates on disk.

## Security

Pairing PIN: a persistent 6-digit PIN is generated on first run (printed at
startup, stored 0600 in `~/.config/cce/cce-remote.pin`). The page asks for
it once per device and remembers it (localStorage); the server closes any
WebSocket whose first frame isn't `auth <pin>`, so no input can be injected
without pairing. Delete the PIN file to rotate it. Traffic is plain HTTP on
the LAN — for hostile networks, tunnel it.
