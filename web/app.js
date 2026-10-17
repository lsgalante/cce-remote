// The page's start-up: WebGPU (or a note why not), the pairing PIN, the
// DE's config and fonts, then the app (src/web, bound by wasm-bindgen as
// cce_remote.js). Everything after start() is the app's.

// This script's own ?v= is the server build's version; the app's code is
// fetched under it, so a new build is a new URL and an old one stays cached.
const V = new URL(import.meta.url).searchParams.get('v') || '';
const PIN_KEY = 'cce-remote-pin';

const note = (text) => {
  const el = document.getElementById('note');
  el.textContent = text;
  el.style.display = 'grid';
};

// An on-screen keyboard covers the page rather than resizing it (iOS, in a
// Safari tab): follow the visual viewport, so the canvas ends where the
// keyboard begins and the trackpad shrinks to stay whole above it (--kb).
//
// Opened from the home screen, iOS 26 sizes the viewport as if the status
// bar took room although the page starts under it (WebKit 301108; 873 of a
// 932px screen here), and draws nothing below it, not even a canvas sized
// past it. A manifest does not help: apps with one report the same. What
// does is the document's own height: `html { height: 100vh }` (index.html,
// under data-standalone) gives the page the whole screen. Where that fails,
// the band left under the canvas is painted the app's colour
// (src/web/mod.rs) and its bottom safe area is not kept clear (--safe-b).
//
// The full-screen app's viewport shrinks with the keyboard (a tab's does
// not), so "the keyboard is up" (--kb-up, src/web/app.rs) is the visible
// height against the tallest seen at this width, not against innerHeight.
const vv = window.visualViewport;
const standalone = navigator.standalone === true || matchMedia('(display-mode: standalone)').matches;
if (standalone) document.documentElement.dataset.standalone = 'true';
// The screen's corner radius, which no web API gives: by the screen's size
// in points, from the values iOS reports for its own (UIScreen's private
// _displayCornerRadius, as collected by ScreenCorners and device_geometry).
// Where a size is shared by phones of two radii, the nearer to both.
const SCREEN_CORNERS = {
  '375x812': 41, // X, XS, 11 Pro (39); 12 mini, 13 mini (44)
  '414x896': 41.5, // XR, 11, XS Max, 11 Pro Max
  '390x844': 47.33, // 12, 12 Pro, 13, 13 Pro, 14
  '428x926': 53.33, // 12 Pro Max, 13 Pro Max, 14 Plus
  '393x852': 55, // 14 Pro, 15, 15 Pro, 16
  '430x932': 55, // 14 Pro Max, 15 Plus, 15 Pro Max, 16 Plus
  '402x874': 62, // 16 Pro, 17, 17 Pro
  '440x956': 62, // 16 Pro Max, 17 Pro Max
};
const screenCorner = SCREEN_CORNERS[Math.min(screen.width, screen.height) + 'x' + Math.max(screen.width, screen.height)];
let tallest = { w: 0, h: 0 };
const fit = () => {
  const visible = vv ? vv.height + vv.offsetTop : window.innerHeight;
  if (window.innerWidth !== tallest.w) tallest = { w: window.innerWidth, h: 0 };
  tallest.h = Math.max(tallest.h, window.innerHeight, visible);
  // A few px of difference is the viewport's rounding, not a keyboard
  // (none is that short): read as one, it left a sliver of bare page.
  const covered = window.innerHeight - visible;
  const root = document.documentElement.style;
  root.setProperty('--kb', (covered > 80 ? covered : 0) + 'px');
  root.setProperty('--kb-up', tallest.h - visible > 80 ? '1' : '0');
  const portrait = window.innerHeight >= window.innerWidth;
  const screenH = portrait ? Math.max(screen.width, screen.height) : Math.min(screen.width, screen.height);
  if (standalone && screenH - tallest.h > 20) {
    root.setProperty('--safe-b', '0px');
  } else {
    root.removeProperty('--safe-b');
  }
  // The root plate's corners where the canvas meets the screen's (app.rs,
  // screen_corners): a home-screen app's top ones, and its bottom ones while
  // the canvas reaches the screen's bottom (not above the keyboard).
  const corner = (on, name) => on && screenCorner ? root.setProperty(name, screenCorner + 'px') : root.removeProperty(name);
  corner(standalone, '--corner-top');
  corner(standalone && Math.abs(window.innerHeight - screenH) <= 2, '--corner-bottom');
  if (window.scrollY) window.scrollTo(0, 0);
};
fit();
if (vv) {
  vv.addEventListener('resize', fit);
  vv.addEventListener('scroll', fit);
}
window.addEventListener('resize', fit);

const askPin = (msg) => {
  const pin = window.prompt(msg || 'cce-remote pairing PIN (shown in the server log):');
  return pin ? pin.trim() : null;
};

// The DE's look, behind the PIN: a wrong PIN is asked again, a throttled
// address told to wait (the server's rate limiter; asking again would only
// feed it).
async function boot() {
  let pin = localStorage.getItem(PIN_KEY) || askPin();
  for (;;) {
    if (!pin) {
      note('cce-remote needs its pairing PIN. Reload to enter it.');
      return null;
    }
    const r = await fetch('/boot', { headers: { 'X-Pin': pin }, cache: 'no-store' });
    if (r.ok) {
      localStorage.setItem(PIN_KEY, pin);
      return { pin, boot: await r.text() };
    }
    if (r.status === 429) {
      note('Too many wrong PINs from here. Wait half a minute, then reload.');
      return null;
    }
    localStorage.removeItem(PIN_KEY);
    pin = askPin('Wrong PIN — try again:');
  }
}

const adapter = navigator.gpu && await navigator.gpu.requestAdapter().catch(() => null);
if (!adapter) {
  // cce-ui draws with nothing else. Over plain HTTP the browser hides WebGPU
  // altogether — the usual reason, and the one worth spelling out.
  note(window.isSecureContext
    ? 'The remote draws with WebGPU, which this browser does not offer. On an iPhone it needs iOS 26 or later.'
    : 'Open the remote over HTTPS: a browser offers WebGPU, which it draws with, only on a secure page.');
} else {
  try {
    const got = await boot();
    if (got) {
      const get = (path) => fetch(path, { headers: { 'X-Pin': got.pin } });
      let families = '';
      const fontUrls = [];
      for (const line of got.boot.split('\n')) {
        if (line.startsWith('families ')) families = line.slice(9);
        else if (line.startsWith('font ')) fontUrls.push(line.slice(5));
      }
      const [config, fonts, glue] = await Promise.all([
        get('/config').then((r) => r.text()),
        Promise.all(fontUrls.map(async (u) => new Uint8Array(await (await get(u)).arrayBuffer()))),
        import(`/cce_remote.js?v=${V}`),
      ]);
      await glue.default({ module_or_path: `/cce_remote_bg.wasm?v=${V}` });
      await glue.start(document.getElementById('app'), fonts, families, config, got.pin);
    }
  } catch (e) {
    console.error(e);
    note('The remote failed to start: ' + e);
  }
}
