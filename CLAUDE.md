# CLAUDE.md

> This is the `cce-remote` crate, inside the larger **`cce` Cargo workspace** —
> read `../cce-compositor/WORKSPACE.md` first for the multi-repo layout, the
> standalone-build rule, `ccebuild`, and the `cce-ui` toolkit. This file covers only
> what is specific to this crate.

`cce-remote` turns a phone into a trackpad, keyboard, window switcher and live window
viewer for the cce desktop. It is **the odd crate in this workspace**: not a compositor
and not a Wayland GUI client. It has no `cce-ui` dependency, draws nothing, and opens no
Wayland surface of its own (it *does* connect to Wayland, but only as a screencopy
client). It is a headless LAN server — HTTP + WebSocket in, compositor control socket
out — and its entire user interface is one hand-written `index.html` compiled into the
binary with `include_str!`.

Mirroring a sibling app for structure is therefore the wrong instinct here. There is no
`Application` trait, no `Message` enum, no widget tree. Four files:

- **`src/main.rs`** — PIN auth + rate limiting, the HTTP/WS dispatch, the
  frame→command translator.
- **`src/stream.rs`** — live-view delivery: the latest-wins `Slot`, the ack-clocked
  sender and its adaptation ladder, and the producer that picks a frame source.
- **`src/screencopy.rs`** — a persistent `wlr-screencopy` client (frame source #2), and
  `downscale_encode`, shared by both raw frame sources.
- **`src/winstream.rs`** — consumer of the compositor's window-stream socket (source #1).

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
- **`cmd restart-compositor` restarts the user's whole session** from a phone, behind
  nothing but a client-side `confirm()`. Compositor-side it writes
  `/tmp/cce-restart-requested-$USER` and exits cleanly (state is saved, and
  `cce-display-manager`'s daemon relaunches greeter-free). Anything added to that `cmd`
  whitelist gets the same reach.

### What the PIN does and does not buy

A persistent 6-digit PIN (`~/.config/cce/cce-remote.pin`, 0600, generated from
`/dev/urandom` on first run, honoring `XDG_CONFIG_HOME`) must arrive as the **first**
WebSocket frame or the connection closes — with a 10s read timeout so unauthenticated
peers can't sit on a socket. The HTTP frame endpoints are gated separately, and
differently, because of a browser constraint that shaped them when the page still
used both: `/shot` takes an `X-Pin` header, but `/stream` accepts `?pin=` in the query
string, because an `<img src>` cannot carry headers. The page uses NEITHER today — the
live view rides `/wstream` — so both are debug endpoints now, and the split survives
for the curl recipes below rather than for a browser.

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

Be honest about what is left rather than treating the PIN as security: it still travels
over **plain HTTP on 0.0.0.0**, is compared non-constant-time, is cached in
`localStorage`, and for `/stream` it rides in a URL, where it lands in any proxy or
history that sees it. Limiting is per-IP, so a peer with many addresses gets many
budgets. It is pairing — it stops other devices on a trusted LAN from steering the
desktop by accident, and now also stops casual brute force. It is not a defense against
someone who is on that network on purpose. For a hostile network the answer is a tunnel.

## Framing: the control socket is one-shot

`control_command()` opens a **fresh `UnixStream` per command** and reads the reply to
EOF. That is not wasteful, it is the protocol: the compositor's IPC server is
read → reply → close. An earlier version held one persistent stream and silently raced
reconnects, dropping commands; reading to EOF is also what makes multi-line replies like
`windows --json` work at all.

The page therefore does the coalescing: `queueFlush()` batches pointer deltas on a 12ms
timer so a fast drag becomes ~80 commands/sec, not one per touch event. One thread is
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
  degraded link costs frame *rate*, never accumulating latency. The ack is sent after
  `drawImage`, not on receipt — so the measured send→ack time covers network + decode +
  paint, which is what the user experiences.
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

## Geometry assumes one output at 0,0

The region passed to screencopy is in output-local logical coordinates, and the window
rects from `windows --json` are in layout coordinates. Those are the same number only
because there is a single output sitting at the origin — true for this DE's eDP-1 setup,
and the same assumption `grim` ran under. Multi-output would need a real mapping here.

The cursor marker (`placeMarker`) maps the other way, through the `object-fit:
contain` letterbox, and stays correct under pinch-zoom only because the zoom is a
**uniform** CSS transform on an ancestor, so `getBoundingClientRect()` already
reflects it. (Tap-to-spot mapping is gone: view-mode input is trackpad-identical —
taps click where the cursor is.)

## `index.html` is the client, and iOS Safari shaped most of it

The page is versioned here and baked in at compile time, so **a UI change needs a
rebuild, reinstall and restart of the server** — there is no asset path to edit live. It
is vanilla JS, no build step, no dependencies.

Four of its non-obvious constructs are scar tissue. Do not "clean them up":

- **The hidden textarea keeps sentinel padding** (`········`, cursor at the end, re-armed
  on focus plus a 1s drift-repair timer). iOS never fires `deleteContentBackward` on an
  empty field, so without something to delete, backspace silently does nothing.
  `beforeinput` is used throughout because iOS `keydown` reports keyCode 229.
- **The zoom/pan transform lives on `#screenwrap`, not the frame element.** Uniform
  ancestor transform means `getBoundingClientRect` reflects it, keeping the cursor
  ring correctly placed while zoomed. (It also used to dodge an iOS bug where a
  transformed multipart-MJPEG `<img>` stopped repainting; the view is a `<canvas>`
  since the 2026-08-22 rework, but the structure stays.)
- **The `overflow: hidden` clip lives on `#pad`, the non-transformed ancestor.** A clip
  on the transformed element scales with its own content and clips nothing.
- **The stream self-heals: reconnect on WS close plus a 30s no-frame watchdog** (the
  frame sources force keepalives ≤20s, so 30s of silence is a dead connection, not an
  idle window). One guard worth keeping: the page only auto-reconnects `/wstream` if
  that connection *paired successfully* — retry-looping a stale PIN would feed the
  rate limiter and lock the phone's address out of the input socket too.

`SCROLL = 0.8`, not the 0.045 it started as: axis values reach clients as surface-px
deltas, so near-unity is the trackpad-like 1:1 feel. A 300px swipe used to scroll one line.

## Verifying

The awkward part: **there is no WebSocket client on this machine** (no `websocat`, no
`wscat`, no python `websockets`), so the WS path — which is most of the logic — can only
be driven from a real phone, or by writing a throwaway client.

The pure functions are the exception, and they are where the crate's invariants are
actually enforced, so they carry all the tests (`cargo test -p cce-remote`, 25 of them:
19 in `main.rs`, 6 in `stream.rs`) — `translate()` for what a paired client may say, the
three PIN gates for who is paired at all, and in `stream.rs` the `Slot`'s latest-wins
semantics plus `adapt()`/`LADDER`. They cover the accepted shapes and — more to the point —
everything that
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
the Makefile — `cargo metadata` already knows them. No `Cargo.lock` is tracked here, so
dependency changes need no lockfile refresh.

**Committing is not publishing — pushing is.** This directory is its own git repository
whose `origin` is the local *bare* repo `~/git/cce-remote.git` (a real, pushable
remote). `published` is the old fetch-only static mirror
`https://git.lucas.co/cce-remote.git`, kept for reference; it never accepted a push
(dumb HTTP, no receive-pack) and that is exactly why the bare layer exists — see
`~/.local/bin/git-bare-sync.sh`. `repos.conf` lists the **bare** path, and
`gitsite.timer` republishes when a listed bare repo's HEAD moves. So the chain is:

```sh
git commit ...                 # local only
git push origin master         # this is the publishing step
                               # gitsite.timer then mirrors it to git.lucas.co
```

An unpushed commit looks published on this machine and is not on the site. (As of
2026-09-18 that gap was workspace-wide: 21 crates held unpushed commits, because
`git-bare-sync.sh` — which does the pushing in bulk — reads `repos.conf` field 2, and
that field now holds the bare path rather than the work tree, so every listed repo is
skipped as "not a git work tree".)

**The server does not run in the foreground — it is a user service.** `cce-remote.service`
ships from this crate root and is installed by `ccebuild` (classified as a user unit by
its `WantedBy=cce-session.target`), so it starts with the session and inherits the
session's `WAYLAND_DISPLAY` — which is the point, since that variable is read once at
startup and fixes which session the process can drive for its whole life.

Because installs unlink-before-write, the running process stays on the old inode after
`ccebuild install` and keeps serving the old code until restarted:

```sh
ccebuild restart                  # picks it up automatically — see below
systemctl --user restart cce-remote
```

`ccebuild restart` does reach it, but not because the unit ships from here: it
enumerates *running* user services matching `^(cce|gpu-watcher)` and restarts the ones
whose `/proc/<pid>/exe` reads `(deleted)`. Being named `cce-remote.service` is the whole
qualification. Editing `index.html` counts as a code change for this purpose — it is
`include_str!`'d, so the page only updates once the binary is rebuilt, reinstalled *and*
the service restarted.

The unit was unversioned until 2026-08-22 — a hand-written file that existed only in
`~/.config/systemd/user/`, in no repo, and so lost on a fresh clone with nothing here to
recreate it. Same failure the `.desktop` entries and `cce-keyring-selftest` had before
they were moved in-repo. It now installs to that same path from this crate.
