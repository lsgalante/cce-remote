// The page's start-up: WebGPU or the classic page, the pairing PIN, the
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
const vv = window.visualViewport;
if (vv) {
  const fit = () => {
    const kb = Math.max(0, window.innerHeight - vv.height - vv.offsetTop);
    document.documentElement.style.setProperty('--kb', kb + 'px');
    if (window.scrollY) window.scrollTo(0, 0);
  };
  vv.addEventListener('resize', fit);
  vv.addEventListener('scroll', fit);
}

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
  // No WebGPU (cce-ui draws with nothing else): the hand-written page.
  location.replace('/classic');
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
    note('The remote failed to start: ' + e + '\n\nThe classic page is at /classic.');
  }
}
