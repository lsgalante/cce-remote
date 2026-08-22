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
`Application` trait, no `Message` enum, no widget tree. Three files, ~880 lines:

- **`src/main.rs`** — PIN auth, the HTTP/WS dispatch, the frame→command translator.
- **`src/screencopy.rs`** — a persistent `wlr-screencopy` client (frame source #2), and
  `downscale_encode`, shared by both live frame sources.
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

- **`translate()` is the single chokepoint — keep it that way.** It returns a *list* of
  commands precisely so the one frame that means two (`tap`/`tapr`: an absolute move
  then a click) has no reason to live inline at the call site. It used to, along with
  `wl`/`pl`, and that inline branch was just as reachable from the network while being
  untestable without a live socket. The only frames still handled in `handle_ws` are
  `wl` and `pl`, which produce a *reply* and send fixed commands carrying no
  caller-supplied content. A new message that carries any part of the frame into a
  command belongs in `translate()`, where the tests can see it.
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
differently, because of a browser constraint: `/shot` takes an `X-Pin` header (the page
`fetch()`es it), but `/stream` accepts `?pin=` in the query string, because an
`<img src>` cannot carry headers.

Be honest about the resulting model rather than treating the PIN as security: it is
**~20 bits, compared with `==` (not constant-time), over plain HTTP on 0.0.0.0**, cached
in `localStorage`, and for `/stream` it travels in a URL — where it lands in any proxy
or browser history that sees it. It is pairing, i.e. it stops the other devices on a
trusted LAN from steering the desktop by accident. It is not a defense against someone
who is on that network on purpose. For a hostile network the answer is a tunnel, not a
longer PIN.

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

## Three frame sources, in preference order

`handle_stream` tries each in turn and falls through on error. All three are live code —
the fallbacks exist because the first two have real preconditions.

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

Frames are box-downscaled to `MAX_EDGE` 560 and JPEG'd at quality 60 (~29KB/frame) by the
shared `downscale_encode` — tuned for wifi latency and phone-side decode, not fidelity.

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

Tap mapping (`mapToWindow`) goes the other way, through the `object-fit: contain`
letterbox, and stays correct under pinch-zoom only because the zoom is a **uniform**
CSS transform on an ancestor, so `getBoundingClientRect()` already reflects it.

## `index.html` is the client, and iOS Safari shaped most of it

The page is versioned here and baked in at compile time, so **a UI change needs a
rebuild, reinstall and restart of the server** — there is no asset path to edit live. It
is vanilla JS, no build step, no dependencies.

Four of its non-obvious constructs are scar tissue. Do not "clean them up":

- **The hidden textarea keeps sentinel padding** (`········`, cursor at the end, re-armed
  on focus plus a 1s drift-repair timer). iOS never fires `deleteContentBackward` on an
  empty field, so without something to delete, backspace silently does nothing.
  `beforeinput` is used throughout because iOS `keydown` reports keyCode 229.
- **The zoom/pan transform lives on `#screenwrap`, never on the `<img>`.** iOS Safari
  stops repainting a GPU-promoted layer when its `multipart/x-mixed-replace` `<img>`
  updates — the live view goes black.
- **The `overflow: hidden` clip lives on `#pad`, the non-transformed ancestor.** A clip
  on the transformed element scales with its own content and clips nothing.
- **The stream `<img>` src carries a nonce, plus an `error` handler and a 20s no-frame
  watchdog.** MJPEG in an `<img>` goes blank when its connection ends, and a browser will
  not re-request an unchanged src, so a network blip left the view dead forever.

`SCROLL = 0.8`, not the 0.045 it started as: axis values reach clients as surface-px
deltas, so near-unity is the trackpad-like 1:1 feel. A 300px swipe used to scroll one line.

## Verifying

The awkward part: **there is no WebSocket client on this machine** (no `websocat`, no
`wscat`, no python `websockets`), so the WS path — which is most of the logic — can only
be driven from a real phone, or by writing a throwaway client.

`translate()` is the exception, and it is where the crate's one invariant is actually
enforced, so it carries the crate's only tests (`cargo test -p cce-remote`, 10 of them,
in `main.rs`). They cover the accepted shapes and — more to the point — everything that
must be refused: unknown verbs *including the compositor's own command names*,
malformed and missing arguments, `wf` targets outside `safe_token`, unwhitelisted `cmd`
names, and the property that no input can make the output span two lines (an embedded
newline would be a second command, since `control_command` appends one). A partial tap
must emit **nothing** — not a bare move, and above all not a click at whatever position
the pointer already had. Extend them when you touch the whitelist; they are much
cheaper than the phone.

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
same numbers the page maps taps through, so it is the quickest check that focus
resolution and geometry agree.

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
the Makefile — `cargo metadata` already knows them. This directory is its own git
repository with a fetch-only origin; committing locally is publishing, via gitsite
(it is listed in `repos.conf`). No `Cargo.lock` is tracked here, so dependency changes
need no lockfile refresh.

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
