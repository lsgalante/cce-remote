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

// An on-screen keyboard covers the page rather than resizing it (iOS):
// follow the visual viewport, so the canvas ends where the keyboard begins
// and the trackpad shrinks to stay whole above it.
//
// Opened from the home screen (navigator.standalone), iOS sizes the viewport
// as if the status bar took room although the page starts under it: 873 of
// a 932px screen, the last 59 (the status bar's height) bare page. There
// the canvas is the screen's height rather than the viewport's (--app-h).
const vv = window.visualViewport;
const fit = () => {
  // A few px of difference is the viewport's rounding, not a keyboard
  // (none is that short): read as one, it left a sliver of bare page.
  const gap = vv ? window.innerHeight - vv.height - vv.offsetTop : 0;
  const kb = gap > 80 ? gap : 0;
  const root = document.documentElement.style;
  root.setProperty('--kb', kb + 'px');
  const portrait = window.innerHeight >= window.innerWidth;
  const screenH = portrait ? Math.max(screen.width, screen.height) : Math.min(screen.width, screen.height);
  if (navigator.standalone && !kb && screenH > window.innerHeight) {
    root.setProperty('--app-h', screenH + 'px');
  } else {
    root.removeProperty('--app-h');
  }
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
