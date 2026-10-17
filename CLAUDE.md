# CLAUDE.md

> This is the `cce-remote` crate, inside the larger **`cce` Cargo workspace** —
> read `../cce-compositor/WORKSPACE.md` first for the multi-repo layout, the
> standalone-build rule, `ccebuild`, and the `cce-ui` toolkit. This file covers only
> what is specific to this crate.

`cce-remote` turns a phone into a trackpad, keyboard, window switcher and live window
viewer for the cce desktop. It is **the odd crate in this workspace**, two programs in
one package:

- **The server** (the binary, `src/main.rs` and its modules) — not a compositor and not
  a Wayland GUI client. It draws nothing and opens no Wayland surface of its own (it
  *does* connect to Wayland, as a screencopy client and a listen-only input method). It
  is a headless tailnet server (LAN with `--lan`): HTTP + WebSocket in, compositor
  control socket out.
- **The page** (the library, `src/web`) — a **cce-ui app run in the phone's browser**
  (WebGPU on a `<canvas>`, since 2026-10-17), compiled to wasm by `build.rs` and
  embedded in the server binary. It replaced a hand-written vanilla-JS `index.html`,
  which was kept as a `/classic` fallback until the app was confirmed on the iPhone and
  then deleted (2026-10-17); `git log -- classic.html` has it.

So the server half has no `Application` trait, no `Message` enum and no widget tree —
mirroring a sibling app's structure is the wrong instinct there — and the page half has
exactly those, built the way `cce-ui/src/main.rs` (the reference app) builds them.

The server:

- **`src/main.rs`** — PIN auth + rate limiting, the HTTP/WS dispatch, the
  frame→command translator.
- **`src/site.rs`** — what a browser is handed besides the sockets: the two pages, the
  page's code (embedded from `build.rs`'s output), the DE's config and fonts.
- **`src/conn.rs`** / **`src/tls.rs`** — one accepted connection, plain or TLS by its
  first byte, with its request head kept for replay; and the certificate it is served
  with (see "HTTPS" below).
- **`src/stream.rs`** — live-view delivery: the latest-wins `Slot`, the ack-clocked
  sender and its adaptation ladder, and the producer that picks a frame source.
- **`src/screencopy.rs`** — a persistent `wlr-screencopy` client (frame source #2), and
  `downscale_encode`, shared by both raw frame sources.
- **`src/winstream.rs`** — consumer of the compositor's window-stream socket (source #1).
- **`src/textfield.rs`** — a listen-only `input-method-v2` client: whether a desktop
  text field is open, for the page's "tap to type" bar.
- **`src/agent.rs`** — the Agent view's requests (`ag <json>` frames), checked,
  rebuilt and carried to Code Agent's control sockets (see "The Agent view").

So the server is a Wayland client twice over: screencopy per stream, and one
input-method registration for the life of the process.

The page (`src/web`; see "The page" below): `app.rs` (the cce-ui `Application`),
`state.rs` (what the page knows, shared with its listeners), `net.rs` (the two sockets,
frame decoding, `/click`), `pad.rs` (the trackpad), `kb.rs` (the phone keyboard),
`icons.rs` (cce-icons glyphs, compiled in), `agent.rs` (the Agent view). `web/` holds its HTML shell, its loader
(`app.js`) and the glyph SVGs; `build.rs` builds it.

## The invariant: the control socket is a full-privilege injection channel

Everything this server does, it does by writing lines to
`/tmp/cce-{WAYLAND_DISPLAY}.sock` — the same channel `ccectl` uses. Anything that can
put a line on that socket can move the pointer, click, and **type arbitrary keystrokes
into whatever the user has focused**. There is no sandbox between a WebSocket frame and
the user's session except the code in this crate.

So the discipline is: **nothing from the network is ever forwarded raw.** `translate()`
is a whitelist that matches a fixed set of shapes and *rebuilds* the command string from
re-parsed values — an unrecognized verb returns `None` and the frame is dropped.
Numbers go through `parse::<f64>()`/`parse::<u32>()`, so they cannot smuggle a newline
and inject a second command. The one place a caller-supplied *string* reaches a command
(`wf <target>` → `focus-window`) is gated on `safe_token`. Named actions are whitelisted
individually by name — note that `cmd` deliberately matches `"restart-compositor"`
against a literal rather than passing the name through, and that shape is the point.

Two things to keep in mind when adding a message:

- **`translate()` is the single chokepoint — keep it that way.** It returns a *list*
  of commands so a verb can expand to several without the expansion living inline at
  the call site (the retired view-tap did: absolute move, then click — view-mode taps
  are plain trackpad clicks since 2026-08-23, and the verbs left the whitelist rather
  than lingering as unused injection surface). The only frames still handled in
  `handle_ws` are `wl` and `pl`, which produce a *reply* and send fixed commands
  carrying no caller-supplied content. A new message that carries any part of the
  frame into a command belongs in `translate()`, where the tests can see it.
- **`cmd <name>` is the page's System menu, and reaches the whole session.** Each wire
  name maps to one FIXED compositor command, matched against a literal: the focused
  app's `restart-app` / `close` / `fullscreen`, `overview`, `screenshot`, `reload`,
  `lock`, `idle display off`, `idle sleep`, `exit` (log out) and `restart-compositor`.
  None takes anything from the frame — "the focused app" is the compositor's to
  resolve, so `cmd restart-focused-app foot` is `restart-app`, never `restart-app foot`
  — and the wire names are deliberately not the compositor's own, so a frame naming a
  compositor command (`cmd exit`, `cmd restart-app`) is dropped. The ones that end the
  session or drop the remote's own connection (sleep, log out, restart compositor) sit
  behind nothing but a client-side `confirm()`; restarting the focused app asks
  nothing (the user's call): its windows get the compositor's close, as their close
  buttons would, and it exits through its own `on_exit` — a cce-ui app cannot veto a
  close, so work an app does not save there is lost; `restart-compositor`
  writes `/tmp/cce-restart-requested-$USER` and exits cleanly (state saved,
  `cce-display-manager`'s daemon relaunches greeter-free). Anything added to the list
  gets the same reach. `cmd` frames run on a thread of their own, because `restart-app`
  replies only once the app is back (up to 10s to exit) and `lock` once every output
  shows the lock — inline, the trackpad stalled behind them.

### What the PIN does and does not buy

A persistent 6-digit PIN (`~/.config/cce/cce-remote.pin`, 0600, generated from
`/dev/urandom` on first run, honoring `XDG_CONFIG_HOME`) must arrive as the **first**
WebSocket frame or the connection closes — with a 10s read timeout so unauthenticated
peers can't sit on a socket. The HTTP frame endpoints are gated separately, and
differently, because of a browser constraint that shaped them when the page still
used both: `/shot` takes an `X-Pin` header, but `/stream` accepts `?pin=` in the query
string, because an `<img src>` cannot carry headers. The page uses NEITHER today — the
live view rides `/wstream` — so both are debug endpoints now, and the split survives
for the curl recipes below rather than for a browser. The one HTTP endpoint the page
does use, `POST /click`, takes the header like `/shot` (`header_gate` serves both).

All three gates are pure functions — `auth_frame_ok`, `header_pin_ok`, `query_pin_ok`,
all over `pin_matches` — so they are unit-tested rather than only reachable through a
socket. `pin_matches` refuses an **empty** PIN outright: `load_or_create_pin`
regenerates on an empty file so it should be unreachable, but that is a property of a
*different* function, and if it lapsed, a bare `X-Pin:` header would authenticate
everything. Gate on the dangerous state, don't trust the caller.

### The rate limiter is what makes 20 bits a credential

A 6-digit PIN is ~20 bits compared with `==`. What keeps that from being walked in an
afternoon is not the comparison, it is `RateLimiter`: a **per-source-IP token bucket
over failed attempts**, 5 back-to-back then one recovered per 30s. That caps sustained
guessing at ~2/min, which turns a couple of hours into the order of a year. All three
gates consult it, and an unresolvable peer address is refused rather than exempted.

Three properties it must keep, each with a test:

- **Only failures are charged, and a success clears the record.** The page reconnects
  its stream on every hiccup — a WS close, the no-frame watchdog — each time presenting
  a correct PIN. If those consumed budget, a working client would throttle itself off.
- **Refill caps at the burst.** Otherwise an idle attacker banks attempts and the limit
  is only an average. Note the test asserts this on `refilled()` *directly*: going
  through the public API hides a missing cap, because `record_failure` prunes recovered
  peers and re-creates them at full.
- **The table cannot grow without bound**, or the limiter becomes its own
  memory-exhaustion vector. Recovered peers are pruned on write (a full bucket is
  indistinguishable from an absent one) with a hard cap behind that, evicting whoever is
  closest to recovered.

On the WS side, a rate-limited connection is closed **without** sending `auth fail` —
that message makes the page discard its stored PIN and prompt, so sending it would
punish a correctly-paired client for someone else's guessing from the same address. The
page reconnects on close and succeeds once the bucket refills. HTTP answers `429` with
`Retry-After`.

Behind the per-peer buckets is one **global** bucket (20 failures, then one per 30 s)
that every failure drains, because per-IP limiting alone gave a peer with many
addresses many budgets — a /22 LAN is a thousand addresses, hours to walk the PIN
space instead of a year. The price is that whoever drains it locks out a paired phone
too until it refills; `many_addresses_share_one_global_budget` pins both halves.

Be honest about what is left rather than treating the PIN as security: unless the page
is opened over HTTPS (below), it travels over **plain HTTP**; it is compared non-constant-time, is cached in `localStorage`, and for
`/stream` it rides in a URL, where it lands in any proxy or history that sees it. On a
network someone else can watch, one observed pairing is keyboard control of the
desktop. So since 2026-10-01 the tunnel is the default rather than advice: the listener
still binds 0.0.0.0, but `reachable()` serves only connections whose peer AND local
address are loopback or Tailscale's (100.64.0.0/10, fd7a:115c:a1e0::/48); anything else
gets a 403 before the request is parsed. Both ends, because Linux accepts a packet for
the tailnet address off the Wi-Fi interface (the peer gives that away), and a LAN peer
spoofing a 100.x source never sees the SYN-ACK, which routes into the tunnel. Filtering
rather than binding the tailnet address keeps working when Tailscale comes up after
this service. `--lan` / `CCE_REMOTE_LAN=1` restores every interface for a trusted
network.

### HTTPS: the same port, by the first byte

The cce-ui page needs a **secure page** — WebGPU does not exist on plain `http://`, so
there the loader can only show a note saying so. So the server also speaks TLS
on its own port (since 2026-10-17): `Conn::accept` peeks the first byte, and a TLS
ClientHello (0x16, which no HTTP request starts with) gets a rustls session while
anything else stays plain (the debug endpoints, and that note). The page is at
`https://<name>:17017/`. The tailnet gate (`reachable`) runs before either, on the
raw socket, unchanged.

- **The certificate is Let's Encrypt's, proven over DNS**: `scripts/cce-remote-cert`
  runs lego with a DNS-01 challenge through Cloudflare (token in
  `~/.config/cce/cloudflare-dns.token`, Zone:DNS:Edit on that zone only), so the name
  only has to *resolve* to the tailnet address — nothing listens on the internet. The
  name is the user's (`remote.lucas.co`, an A record to the machine's 100.x address);
  the first run names it, later runs renew whatever is in lego's directory.
- **Renewal needs no restart.** `cce-remote-cert.timer` runs the script daily (lego
  renews inside the last 30 days), and `tls::config` re-stats the files per connection
  and reloads when they change — keeping the last good config if a reload fails
  half-written.
- **No certificate, no HTTPS**: `tls::config` is `None`, a ClientHello is refused, and
  the server is exactly the plain-HTTP one it was.
- **The head is read, not peeked**, because a TLS stream cannot be peeked: `read_head`
  reads through the blank line and keeps the bytes, and reads replay them first, so
  tungstenite still sees a WebSocket's whole handshake. The 10s open timeout covers the
  handshake and the head — a client that connects and sends nothing no longer holds a
  thread forever, as the old blocking `peek` let it.
- Pages use `wss://` on an https page and `ws://` otherwise (a browser refuses a plain
  socket from a secure page): `net::url`.

## Framing: the control socket is one-shot

`control_command()` opens a **fresh `UnixStream` per command** and reads the reply to
EOF. That is not wasteful, it is the protocol: the compositor's IPC server is
read → reply → close. An earlier version held one persistent stream and silently raced
reconnects, dropping commands; reading to EOF is also what makes multi-line replies like
`windows --json` work at all.

The page therefore does the coalescing: `queue_flush` (`pad.rs`) batches pointer deltas on a 12ms timer so a fast drag becomes ~80 commands/sec, not one per touch event. One thread is
spawned per accepted connection, uncapped, and a `/stream` connection holds its thread
for the life of the stream.

## The live view: latest-wins delivery, three frame sources

**Delivery and capture are separate concerns since the 2026-08-22 rework.** The
original MJPEG path pushed every frame, in order, into a blocking TCP write; nothing on
this side ever dropped one, so the kernel's send buffer (~10-30 frames) became a queue,
and the moment wifi throughput dipped below the frame rate the view fell seconds behind
and never recovered — "fine at first, unusable after a short time".

The delivery design (`stream.rs`) makes that failure structurally impossible:

- A **`Slot`** holds only the newest frame; the producer overwrites it. Overwriting IS
  the frame-dropping — stale frames cease to exist before they cost encode or network.
- The page's live view rides **`/wstream`**, a dedicated WebSocket (same `auth <pin>`
  first-frame gate): the server sends one frame, the page renders it and acks `n`, and
  only then does the newest frame go out. **At most one frame is ever in flight**, so a
  degraded link costs frame *rate*, never accumulating latency. The ack is sent once
  the frame is *painted* (`net::ack_painted`, from the app's `display_list`), not on
  receipt — so the measured send→ack time
  covers network + decode + paint, which is what the user experiences.
- That measurement drives an **adaptation ladder** (`LADDER`/`adapt()`): resolution up
  to 1400px edge when the link is fast, downgrades immediate, upgrades requiring
  sustained headroom. Encoding happens per *sent* frame at the chosen level. The ladder
  alternates rather than sacrificing one axis first — `(1400,68) → (1120,68) →
  (1120,55) → (840,58) → (840,46) → (560,48)`, a size drop first, and quality rising
  again where size falls. `ladder_prefers_resolution_over_quality` does NOT assert the
  preference its name claims: it only checks that the edge never increases, which a
  ladder that dropped size at every step would also satisfy.
- `/wstream` is deliberately a **separate socket from the input WS**: frames are
  30-150KB and input events are bytes; one TCP stream would head-of-line-block pointer
  motion behind every frame.
- `/stream` (MJPEG over HTTP) survives as the **curl-debuggable endpoint**, thin over
  the same slot at fixed 560/q60. Without acks its TCP buffer can still hold a few
  frames — fine for debugging, which is all it is for now.

Every accepted socket gets `TCP_NODELAY` — before the rework nothing set it, so Nagle
was batching tiny input events behind delayed ACKs.

The ceiling above this design is hardware H.264 + WebCodecs/WebRTC (~5-10× fewer bytes),
at the cost of VAAPI/GStreamer deps and Safari codec quirks. Ack-clocked adaptive JPEG
is the right cost/benefit for a single-window view on a LAN; revisit only if it proves
bandwidth-starved in practice.

### The three frame sources

`spawn_producer` tries each in turn. All three are live code — the fallbacks exist
because the first two have real preconditions.

1. **`winstream`** — subscribe `window focused` on `/tmp/cce-stream-{WAYLAND_DISPLAY}.sock`
   and read `frame <w> <h> <len>` + packed RGBA. Best source: damage is *per window*, it
   follows focus server-side, it streams occluded and off-viewport windows, and a truly
   idle window sends nothing but a ≤15s keepalive. Requires a compositor built with
   `stream_server.rs`; against an older running `cce-fx` the connect fails and we drop to
   screencopy, which is why this is a fallback chain and not a choice.
2. **`screencopy`** — one long-lived `wlr-screencopy` connection, per-frame
   `capture_output_region` of the focused window's rect, throttled by `copy_with_damage`.
   ~11 fps active. Its damage gate is **per output, not per region**, so an idle window
   still wakes on unrelated screen activity.
3. **`grim`** — fork per frame, `-s 0.5 -q 65`. ~2.5 fps. The floor.

Frames are box-downscaled and JPEG'd by the shared `downscale_encode`, at whatever
(edge, quality) the ladder picked for the link — not a fixed size anymore.

**The cursor differs between sources, and the page compensates for the worst case.**
Compositor window-stream frames are surface textures with no cursor composited, so the
page draws its own cyan ring. Since that stream is *damage-driven*, moving the pointer
produces no repaint at all — which is why the marker is predicted client-side from the
finger delta (same `ACCEL` as the sent move) and only *reconciled* by a 120ms
`pl`/`ploc` poll. Polling alone updated it 4×/s and felt broken. The screencopy path
passes `overlay_cursor=1`, so on that fallback you see the real cursor *and* the ring.

`/shot` (the one-shot PNG) screenshots through the compositor and then **deletes the
file** — verified: a `/shot` leaves nothing new in `~/Pictures/screenshots`. Remote
viewing must not accumulate captures on disk.

## The phone keyboard follows desktop text fields: a listen-only input method

A left tap on the trackpad that opens a desktop text field raises the phone keyboard;
a field open by any other route shows a "tap to type" bar that does (both since
2026-10-17). The signal is the compositor's text-input relay
(`input_relay.rs`): it hands `activate`/`deactivate` to the seat's one
`input-method-v2` client, and `textfield.rs` registers as that client at startup. It
**commits nothing, sets no preedit and grabs no keyboard** — typing still goes through
the control socket as keycodes — so apps behave exactly as they would with no input
method. Any cce-ui field that claims (`cce_ui::text_input::claim`) triggers it, as do
GTK, Qt and browser fields.

- **The seat takes one input method.** If another holds it (fcitx, ibus) the compositor
  answers `unavailable`; the watcher logs that once, retries every 10s, and the bar
  simply never shows. Two `cce-remote`s on one session hit this too — the second has no
  bar.
- **Delivery is polled, not pushed.** `handle_ws` reads with a 20ms timeout
  (`FIELD_POLL`) so its loop can send `ti 1`/`ti 0`; tungstenite keeps a frame a
  timeout split. The frames are fixed strings carrying nothing from the client, so
  `translate()` is not involved and the injection surface is unchanged.
- **A close waits 250ms** (`CLOSE_GRACE`, matching `osk.rs`'s `HIDE_DELAY_MS`): moving
  between fields disables one and enables the next, and the bar must not flicker.
  An open is sent at once.
- **The page can raise the keyboard only inside a tap.** iOS (and mostly Android) open
  it only from the user's own gesture handler, never from a WS message — so `ti 1`
  alone shows the bar rather than calling `kb.focus()`.
- **So a left tap's click is a SYNCHRONOUS `POST /click`.** The server performs the
  fixed `b left click` (through `translate()`), then waits on `textfield::wait_opened`
  for an activation, up to `CLICK_FIELD_WAIT` (150ms), and answers `1`/`0`; the page,
  still inside `touchend`, focuses the keyboard on `1`. Activations measured 1-16ms
  after the click, so a `1` costs almost nothing — but **every tap that opens nothing
  freezes the page for the full wait** plus a round trip. That is the price; keep the
  wait short, since a miss only degrades to the bar. The tap's few px of move jitter
  are dropped rather than flushed, because the HTTP click could overtake them on the WS.
- **Only a NEW activation counts** (`Field::opened`, bumped on a `done` whose batch
  carried `activate`; a switch between fields counts). Tapping a field that is already
  open answers `0`, and the bar covers it. "Any update while open" was measured as the
  alternative and rejected: a click inside an open field does produce one, but so does
  a click on a button that makes the app repaint, so the keyboard would pop on ordinary
  clicks in any app whose field stays open (the text editor's always is).
- A close blurs the keyboard **only if a field raised it** (the bar or `/click`); one
  opened with the keyboard button stays, as `osk.rs` leaves a hand-summoned board.

**The field's contents reach the phone too** (since 2026-10-17). An app that reports its
surrounding text — cce-ui's `TextBox` and `DocEditor` (cce-ui `bfafde0` and later; an app
shows it once rebuilt against that), GTK, Qt, browsers — has the compositor relay it with
the activation state; `textfield.rs` keeps it (`contents_since`, a change counter) and
`handle_ws` sends each change as `tt <cursor> <anchor> <text>` (byte offsets; the text is
the rest of the frame, newlines included), or a bare `tt` for none. The page's strip then
shows the field's own line around its caret, the caret and any selection where the desktop
has them (`paint_preview`); with no contents it falls back to the echo of what the phone
typed. Typing still goes out as keycodes; the strip does not wait for the app to report
back, though — **it predicts** (`src/predict.rs`, pure and compiled natively for its
tests): each edit typed on the phone (a character, Backspace, Delete, Left, Right) is
shown applied to the last report at once, and a new report retires the edits it already
reflects (the shortest prefix that turns the old text into the new). A report nothing
explains — the app rewrote, refused or completed — is the truth and drops the guesses;
an edit the prediction cannot follow (Enter, Up/Down, Esc/Tab, anything with a modifier
latched) stops it there, and a guess unreflected after 1.5s gives way. Waiting for the
round trip instead made the strip trail a fast typist visibly; `FIELD_POLL` dropped from
100ms to 20ms for the same reason.
**A field whose content type is a password or PIN, or hints hidden or sensitive text, is
never passed on** (`sensitive`), whatever its app sends — cce-ui's password box reports
nothing anyway, but GTK's and others' are only known by their content type.

Not done, and why: `commit_string` would type Unicode the keycode table cannot, but it
travels on the Wayland connection while Backspace/Enter go through the control socket,
and the two can reorder.

## Geometry assumes one output at 0,0

The region passed to screencopy is in output-local logical coordinates, and the window
rects from `windows --json` are in layout coordinates. Those are the same number only
because there is a single output sitting at the origin — true for this DE's eDP-1 setup,
and the same assumption `grim` ran under. Multi-output would need a real mapping here.

The cursor ring maps the other way: from the pointer's layout position, through the
focused window's rect, into the frame's contain-fit in the pad, then through the
pinch's zoom and pan (`paint_pad`). (Tap-to-spot
mapping is gone: view-mode input is trackpad-identical — taps click where the cursor is.)

## The page: a cce-ui app in the browser

The phone loads `/` — **over HTTPS** (see "HTTPS" above): `web/index.html` (a full-page canvas, the keyboard's textarea) and
its loader `web/app.js`, which checks for WebGPU — **none, and it shows a note** (over
plain HTTP: open it over HTTPS; on a secure page: the browser lacks WebGPU — an iPhone
needs iOS 26), since cce-ui draws with nothing else — then asks for the PIN
if it has none, fetches `/boot` (the font list), `/config` and the fonts, all behind
the PIN, and starts the app. Everything after `start()` is Rust.

**The app paints; it does not read the pad or the keyboard.** cce-ui's browser shell
follows one finger and reads it as a touchscreen does (tap, scroll, held drag), and its
keyboard sink lacks the iOS Backspace fix, so:

- **`pad.rs` owns the trackpad's touches.** It reads the page's touch events itself (all
  fingers, relative motion, the same gestures and constants as the hand-written page
  had), and a
  capture-phase pointer listener on the window **stops the matching pointer events
  before cce-ui's canvas listeners see them**. Which touches are the pad's is decided by
  position against the rects the app publishes in `state` (`pad`, `overlay_rect`) —
  everything else (the keys, an open panel) is cce-ui's as usual. Every touch on the
  canvas is `preventDefault`ed so iOS cannot move focus off the keyboard's textarea
  mid-typing; pointer events come first and are not cancelled by it.
- **`kb.rs` owns the keyboard**: its own hidden `<textarea id="kb">`, not cce-ui's sink.
- **The app's buttons act in cce-ui's `pointerup` dispatch** (`handle_mouse_input` →
  `drain`), synchronously — which is what lets the keyboard button, the type bar and
  the left button raise the phone keyboard (a focus made inside the gesture;
  confirmed on an iPhone, iOS 26.5, along with the sentinel Backspace below).
- **`state` is a thread-local the app and the listeners share.** Never call into the DOM
  inside `state::with`: focusing the textarea raises `focus` synchronously, its listener
  takes the state again, and the nested borrow panics.

What carried over from the hand-written page, and must not be "cleaned up":

- **The hidden textarea keeps sentinel padding** (`········`, cursor at the end, re-armed
  on focus plus a 1s drift-repair timer). iOS never fires `deleteContentBackward` on an
  empty field, so without something to delete, backspace silently does nothing.
  `beforeinput` is used throughout because iOS `keydown` reports keyCode 229.
- **The stream self-heals: reconnect on WS close plus a 30s no-frame watchdog** (the
  frame sources force keepalives ≤20s, so 30s of silence is a dead connection, not an
  idle window). The page only auto-reconnects `/wstream` if that connection *paired
  successfully* — retry-looping a stale PIN would feed the rate limiter and lock the
  phone's address out of the input socket too.
- **A poll reply repaints only when it changed something** (`ploc` 8×/s and `windows`
  every 2s in the live view): the whole canvas is redrawn per frame, and repainting on
  every reply kept a software-WebGPU browser pegged.

The page's look is the desktop's: `/config` is the DE's `config.kdl`, loaded with
`cce_ui::color::reload_colors` before the app starts, and `/boot` lists the faces the
config names (`site::fonts` finds `font="Berkeley Mono 14"`'s regular face in
`$CCE_FONTS_DIR`, default `~/Dropbox/Fonts`), then Noto Sans and JetBrains Mono as
fallbacks. Spacing is the ladder's, with one deliberate step down: **the key rows use
`list_gap()`**, because the controls rung (24px) left seven keys too narrow for a thumb
at a phone's width.

**Every symbol is a cce-icons glyph**: the arrows, windows (`menu`), menu
(`more-horizontal`), keyboard, and the connection mark (`circle` paired,
`circle-outline` otherwise). A browser has no icon directory for `upload_icon`, so the
SVGs are copied into `web/icons/` from `../cce-icons/svg/` and compiled in (`icons.rs`,
tinted as `upload_icon_tinted` tints) — copy again if a glyph is redrawn.

**Caching**: `/` is `no-cache` and names the build's version; the loader, the glue and
the wasm (~1.7 MB gzipped, served with `Content-Encoding: gzip`) are fetched under
`?v=<version>` and marked immutable, so a phone downloads the app once per server build.
The `ver` frame after pairing reloads a page whose server has restarted onto new code,
as before — and the page comes back where it was (`place.rs`): the live view, or the
Agent view's screen and draft, saved to `sessionStorage` just before the reload (the
menu's "Reload page" too) and taken once at start, so a page opened afresh still starts
on the trackpad. Restored before the input socket pairs, the Agent view's first request
is dropped; `agent::reconnected` (on every `auth ok`) clears what was in flight and asks
again — without it, a request lost to any reconnect left the view's timer waiting on it
for good.

`SCROLL = 0.8`, not the 0.045 it started as: axis values reach clients as surface-px
deltas, so near-unity is the trackpad-like 1:1 feel. A 300px swipe used to scroll one line.

## The Agent view: Code Agent's sessions on the phone

The menu's "Code Agent sessions" row swaps the page for the Agent view
(`src/web/agent.rs`): every session on disk by folder (archived ones left out — even one
still open in a window, which the desktop's sidebar keeps listed; one archived from the
phone goes at once), what a
Code Agent window is doing with each (working / needs you / open) or its age, a folder's
"+ new"; a tap opens a session's transcript (replies as wrapped text, each tool call one
row, a failed call's error), a waiting permission prompt's card (Deny / Always /
Allow; a question takes only Skip — it wants answers the page does not offer), and a
prompt strip with Keyboard and Send; a `⋯` menu with Stop (while a turn runs), Show on
desktop (`open`), New session in its folder, and Archive (confirmed). Text draws above
every shape, so rows under the open menu are clipped at its left edge (`uncovered`).
"‹ Trackpad" goes back. While it is on, `app.rs` hands it layout, paint and
events, and publishes the pad as empty, so `pad.rs` takes no touches.

Carets and character widths come from cosmic-text's shaping (`app::shaped`, through
`cce_ui::geometry_font_system`), never `measure_text_width`: that is an inked extent,
short of the advance by the side bearings, and a caret placed by it drifted off the text.

- **The second injection channel.** Code Agent (cce-code-agent) listens on
  `$XDG_RUNTIME_DIR/cce/code-agent/<pid>.sock` per window, one JSON request per
  connection (its `remote.rs`). The page's `ag <json>` frames go there through
  `src/agent.rs`, under the same rule as `translate()`: `parse` takes a fixed set of
  shapes (list, transcript, send, stop, answer, archive, open), every id checked by
  `token`, the prompt capped at 64 KB, and `Op::line` rebuilds the request from the
  checked values alone. A prompt's text does reach Code Agent as text, as typed keys
  reach the desktop: it is what a paired phone is for.
- **Routing** (`agent::run`): a session open in a window is that window's; one that is
  not opens in the newest window (send, open, archive). With no window, the list and a
  transcript come from `cce-code-agent --list` / `--transcript <id>` and the view is
  read-only (the user's choice: nothing starts on the desktop from the phone). The
  frame is answered on a thread of its own, its reply drained by `handle_ws`'s loop
  (`agent_rx`), so the trackpad never waits behind a window's answer.
- **Polling**: every `POLL_MS` (1.2 s) while the view is open, the list or the open
  transcript (with `since`, so an unchanged one is a short reply); read-only, every
  fourth tick, since each answer runs `cce-code-agent` over the session files. A reply
  repaints only when it changed something (the list's raw JSON, the transcript's
  version), as every poll on this page.
- **Typing**: while a prompt is being written (a session or a new one, not read-only),
  `kb.rs` hands its input to `agent::compose`, which edits the draft instead of sending
  keycodes; Enter is a new line, Send sends.
- **Text** wraps by character count (`src/wrap.rs`, tested natively): the face is the
  desktop's monospace one. Markdown shows as written — cce-ui's Markdown layout measures
  with a font set of its own, which on the page would lack the `/boot` fonts.

## Verifying

The awkward part: **there is no WebSocket client on this machine** (no `websocat`, no
`wscat`, no python `websockets`), so the WS path — which is most of the logic — can only
be driven from a real phone, or by writing a throwaway client.

The pure functions are the exception, and they are where the crate's invariants are
actually enforced, so they carry all the tests (`cargo test -p cce-remote`, 43 of them:
21 in `main.rs`, 6 in `stream.rs`, 7 in `textfield.rs`, 3 in `site.rs`, 2 in `conn.rs`,
4 in `predict.rs`) —
`translate()` for what a paired client may say, the three PIN gates for who is paired
at all, in `stream.rs` the `Slot`'s latest-wins semantics plus `adapt()`/`LADDER`, in
`textfield.rs` the field-switch grace (`decide`), the click's activation wait
(`wait_opened`), the contents' change counter and the sensitive-field rule, in `site.rs` the font resolution and the caching headers, and in
`conn.rs` the head replay. They
cover the accepted shapes and — more to the point — everything that
must be refused: unknown verbs *including the compositor's own command names*,
malformed and missing arguments, `wf` targets outside `safe_token`, unwhitelisted `cmd`
names, and the property that no input can make the output span two lines (an embedded
newline would be a second command, since `control_command` appends one). Extend them
when you touch the whitelist; they are much cheaper than the phone.

One of them, `non_finite_coordinates_are_dropped`, guards a hole that was live until
2026-08-22: `f64::from_str` accepts `"NaN"`/`"inf"` and `{:.2}` prints them straight back,
so `m NaN 1` used to reach the compositor's pointer math verbatim. `parse().ok()` is not
sufficient validation for a float — hence `finite()`.

What can be checked from the desktop, and is confirmed working:

```sh
curl -s -o /dev/null -w '%{http_code}\n' http://127.0.0.1:17017/          # 200, the page
curl -s -o /dev/null -w '%{http_code}\n' http://127.0.0.1:17017/shot      # 403, no PIN
curl -D- -o /tmp/shot.png -H "X-Pin: $(cat ~/.config/cce/cce-remote.pin)" \
     http://127.0.0.1:17017/shot                                          # 200 + X-Win rect
```

`X-Win: <id> <x> <y> <w> <h>` in that reply is the focused window's layout rect — the
same rect the frame sources capture and the page places its cursor ring inside, so it
is the quickest check that focus resolution and geometry agree. (The page does not read
this header; nothing in the page fetches `/shot` at all.)

**The page is checked in headless Chrome against a shadow session.** The built-in
browser pane has no WebGPU adapter, and Chrome run *inside* a shadow segfaults; headless
Chrome with SwiftShader works (cce-ui's `scripts/web-probe/browser.mjs` flags:
`--headless=new --enable-unsafe-webgpu --use-webgpu-adapter=swiftshader
--enable-features=Vulkan --use-angle=swiftshader --enable-unsafe-swiftshader
--disable-gpu-compositing --remote-debugging-port=<p>`). Drive it over CDP: phone
emulation (`Emulation.setDeviceMetricsOverride` 390×844 @2, `mobile`, plus
`setTouchEmulationEnabled`) — it lasts only as long as the CDP session, and re-applying
it per step resizes the canvas between steps, so run a scenario in ONE session;
`Input.dispatchTouchEvent` for fingers (multi-touch and pinch work); `Input.insertText`
for typing (it fires `beforeinput`); the PIN pre-seeded with
`Page.addScriptToEvaluateOnNewDocument`. Headless screenshots leave a WebGPU canvas
blank, so the page exports `capture()` (cce-ui's `web::capture`, `[w, h, rgba]`):
`import('/cce_remote.js?v=<v>')` from the page returns the same module instance. CDP
console events are lost across a navigation; hook `console` into a page variable from
the same new-document script instead. Run the server in the shadow on a spare port with
`CCE_FONTS_DIR` set (a shadow's HOME is its own, so the DE's fonts are not found).
Expect software WebGPU to be slow — the live view pegs a dozen cores — which a phone's
GPU is not.

**The Agent view was checked this way** (2026-10-17), against Code Agent run in the
shadow with its fake CLI and a server on a spare port, with two harness findings worth
keeping. Playwright's Node module is not installed (its browsers are, under
`~/.cache/ms-playwright`), so drive that Chromium over CDP from plain Node (`fetch`
`/json/list`, then the page target's `webSocketDebuggerUrl` with Node's own `WebSocket`).
**Click with `Input.dispatchMouseEvent`, not emulated touch**: after a
`dispatchTouchEvent` tap, and after the Agent view had been shown, `capture()` never
resolved (no GPU error, no exception) — so the Agent view was verified by its effects
(console traces of each request and reply, and the session's state read back from
Code Agent's socket) rather than by pixels. A workspace build also failed at start-up
while cce-core had an uncommitted `std::process::id()` on a path the page reaches
(it panics on wasm32); a standalone build (`git archive HEAD` or a copy, built outside
the workspace with its own lock) sidesteps the workspace's patched checkouts.

**Injected input lands in the live session** — pointer moves steer the user's real
cursor and keystrokes go into whatever they have focused. To exercise the input path
safely, point the server at the **shadow session** instead: run it with
`WAYLAND_DISPLAY=` set to the shadow display (see `cce-shadow` in
`../cce-compositor/CLAUDE.md`) on a spare port. Both sockets this crate needs exist
there — `/tmp/cce-{display}.sock` and `/tmp/cce-stream-{display}.sock` — so even the
live-view path is reachable. Note the display name is read from the environment at
startup, so a server is bound to whichever session launched it for its whole life.

## Build and lifecycle

`make install` → `ccebuild install --no-build cce-remote`. Never hand-list binaries in
the Makefile — `cargo metadata` already knows them. `Cargo.lock` is committed here, so
refresh it when dependencies change.

**`build.rs` builds the page** on every native build of the package (any change under
`src/web`, `src/lib.rs` or `web/` reruns it): a nested `cargo build --lib --release
--target wasm32-unknown-unknown` into `<target>/web` (its own target dir — the outer
build holds its own dir's lock), with the outer build's flags stripped and
`--cfg=web_sys_unstable_apis` set (web-sys's WebGPU bindings); then the `wasm-bindgen`
CLI (`$WASM_BINDGEN`, PATH, or `~/.cargo/bin`) binds it into `$OUT_DIR/site`, and the
wasm is gzipped beside itself. So `ccebuild install cce-remote` needs nothing extra —
but it needs the wasm32 target (Arch: `rust-wasm`) and **a wasm-bindgen CLI of exactly
the lock's wasm-bindgen version** (0.2.126 in both the workspace's lock and this
crate's; the standalone lock was seeded from the workspace's to keep them equal —
`cargo update` would move it to whatever is newest and break the build until the CLI
follows). A change to cce-ui itself does not rerun `build.rs`; touch `src/web` to pick it
up. `CCE_REMOTE_SKIP_WEB=1` builds the server without the app (empty placeholders), for
a machine without the toolchain: `/` then only says the page is missing (the sockets
and debug endpoints still work).

**Committing is not publishing — pushing is.** This directory is its own git repository
whose `origin` is GitHub (`https://github.com/lsgalante/cce-remote.git`), and a
post-commit hook pushes each commit there. So the chain is:

```sh
git commit ...                 # the post-commit hook pushes to GitHub
git log origin/master..        # empty means it landed
```

A commit whose push failed (hook missing, network down) looks published on this machine
and is on no remote at all; nothing warns, so the `git log` check is the only safeguard.

**The server does not run in the foreground — it is a user service.** `cce-remote.service`
ships from this crate root and is installed by `ccebuild` (classified as a user unit by
its `WantedBy=cce-session.target`), so it starts with the session and inherits the
session's `WAYLAND_DISPLAY` — which is the point, since that variable is read once at
startup and fixes which session the process can drive for its whole life.

Because installs unlink-before-write, the running process stays on the old inode after
`ccebuild install` and keeps serving the old code until restarted:

```sh
ccebuild restart cce-remote       # only if it is on a replaced binary — see below
systemctl --user restart cce-remote
```

Always name the crate. Bare `ccebuild restart` restarts *every* running `cce*` user
service on a replaced binary, including ones another session has installed but is not
ready to restart (WORKSPACE.md, "Concurrent sessions"). The scoped form reaches this
service because `cce-remote.service` ships from this crate root, and among those units
it restarts only the ones whose `/proc/<pid>/exe` reads `(deleted)`; `--all` restarts
them regardless. Editing the page (`src/web`, `web/`) counts as a code
change for this purpose — it is embedded, so the page only updates once the binary is
rebuilt, reinstalled *and* the service restarted.

`cce-remote-cert.timer` (and the oneshot `.service` it runs) ships from this crate root
too, with `scripts/cce-remote-cert`; `ccebuild` installs both but does not enable the
timer — `systemctl --user enable --now cce-remote-cert.timer` once, after the first
`cce-remote-cert <name>`.

The unit was unversioned until 2026-08-22 — a hand-written file that existed only in
`~/.config/systemd/user/`, in no repo, and so lost on a fresh clone with nothing here to
recreate it. Same failure the `.desktop` entries and `cce-keyring-selftest` had before
they were moved in-repo. It now installs to that same path from this crate.
