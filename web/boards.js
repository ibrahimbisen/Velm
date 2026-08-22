// The front door: every board on a `velmd` server, and a way into one.
//
// `index.html` opens **one** board, named by `?board=` in its own URL, and it carries a small
// inline list for the case where it is served by the very server that holds the boards. That
// list cannot answer the question this port exists for — *"no matter which computer I am on I
// can access it"* — because it only ever asks its **own origin**, and on a borrowed machine
// the app and the boards are not on the same one. This page is the missing half: it asks for
// a server, remembers it, lists what is there, and hands one board to the viewer.
//
// # ⚠ Where a board actually opens, and why it is not always this origin
//
// The viewer fetches `/api/v1/boards/{id}/snapshot` and `/api/v1/blobs/` **relative to its own
// page** — read `index.html`, there is no `?server=` in it. So a board held by a *remote*
// server cannot be opened by this origin's copy of the client, whatever link is built. It is
// opened by **that server's own copy**, which `velmd serve --web` is already serving. So a
// remote board's link is absolute and leaves this page behind, and a local board's link is
// `./index.html` beside this file. `buildViewerUrl` is the one place that decides, because two
// copies of that rule is a link that lands somewhere the fetch does not.
//
// That is also `docs/08-web.md` §5's rule — *the app and the boards are served from the same
// origin* — arrived at from the other end: this page is allowed to be somewhere else precisely
// because it hands off rather than renders.
//
// # ⚠ Everything below the exports is testable without a browser
//
// `mount()` is the only function that touches a DOM, and it takes its `document`, `window`,
// `fetch` and storage as arguments. Nothing runs at import time. That is not tidiness: it is
// the only way a `node --input-type=module` harness can drive the URL builder, the token
// lookup and the failure classifier — the three pieces most able to be wrong while looking
// right — and A/B them against a deliberately broken line.
//
// # Colours and hit targets are `index.html`'s and `chrome.js`'s
//
// `boards.html` carries the stylesheet; it uses the same `#22282C` / `#656D73` / `#E2E7EA`
// tokens and the same 44px minimum, `touch-action: manipulation`, transparent tap highlight
// and hover-inside-`@media (hover: hover)` that `chrome.js` documents at length. The accent
// `#00A38C` means *selection* or *the active tool* in Velm and a list of boards is neither, so
// it appears on a focus ring and nowhere else.

/** How long to wait for the liveness probe before calling a server unreachable. */
export const HEALTH_TIMEOUT_MS = 8000;

/**
 * How long to wait for the board list.
 *
 * Longer than the probe: the list opens every board file on the server to read its index, so a
 * directory of 58 boards on a spinning disk is legitimately slower than a health check.
 */
export const BOARDS_TIMEOUT_MS = 15000;

const SERVER_KEY = 'velm.server';
const TOKEN_PREFIX = 'velm.token:';

// ─────────────────────────────────────────────────────────────────────────────
// Hosts
// ─────────────────────────────────────────────────────────────────────────────

/** Strip an IPv6 literal's brackets and lower-case, so one spelling reaches the tests below. */
function bareHost(hostname) {
  return String(hostname || '').toLowerCase().replace(/^\[/, '').replace(/\]$/, '');
}

/**
 * Whether a host is loopback, in the sense browsers mean by it.
 *
 * Two rules hang off this and they pull in opposite directions, which is why it is one
 * function: a browser will **not** block `http://localhost` as mixed content, and it **will**
 * grant it a secure context, so `http://localhost:8787` is the one http address where all of
 * this simply works. Everything else on http is a page that loads and draws nothing
 * (`docs/08-web.md` §5).
 */
export function isLoopbackHost(hostname) {
  const host = bareHost(hostname);
  if (host === 'localhost' || host.endsWith('.localhost')) return true;
  if (host === '::1') return true;
  return /^127\.\d{1,3}\.\d{1,3}\.\d{1,3}$/.test(host);
}

/**
 * Whether a host is on a private network.
 *
 * Used for one message only. Chrome 142 enforces Private Network Access with no fallback, so a
 * page on the public internet reaching `192.168.1.20` is refused by the browser in a way that
 * is **indistinguishable from the server being down** — the request never leaves and the
 * rejection arrives as the same `TypeError`. Naming it as a possibility beside "not running"
 * is the honest answer; claiming to know which is not.
 */
export function isPrivateHost(hostname) {
  const host = bareHost(hostname);
  if (isLoopbackHost(host)) return true;
  if (host === 'local' || host.endsWith('.local')) return true;
  if (/^fe80:/.test(host) || /^f[cd][0-9a-f]{2}:/.test(host)) return true;
  const quad = /^(\d{1,3})\.(\d{1,3})\.(\d{1,3})\.(\d{1,3})$/.exec(host);
  if (!quad) return false;
  const a = Number(quad[1]);
  const b = Number(quad[2]);
  if (a === 10) return true;
  if (a === 127) return true;
  if (a === 192 && b === 168) return true;
  if (a === 169 && b === 254) return true;
  return a === 172 && b >= 16 && b <= 31;
}

// ─────────────────────────────────────────────────────────────────────────────
// The server address
// ─────────────────────────────────────────────────────────────────────────────

/**
 * Turn whatever somebody typed into a base URL, or refuse.
 *
 * ⚠ **The scheme test requires `://`, and that is not pedantry.** `/^[a-z][a-z0-9+.-]*:/`
 * matches `localhost:8787` — `localhost` is a perfectly good scheme name — so a bare host and
 * port would have been parsed as a protocol and thrown away the port. Requiring the slashes is
 * what tells a scheme from a port.
 *
 * ⚠ **Only `http:` and `https:` survive.** `?server=` is in a link anybody can send, and the
 * result of this function ends up in an `href`. A `javascript:` URL that reached that
 * attribute is script execution on this page; refusing every other scheme here is what makes
 * that structurally impossible rather than carefully avoided. Credentials are stripped for the
 * neighbouring reason — `https://evil.example@real.example/` reads as `evil.example` to a
 * person and resolves to `real.example`.
 *
 * A missing scheme takes **the page's own**, so a hostname typed on an https app site is https
 * rather than an http address the browser will refuse to load. Somebody who really does mean
 * http types it, and `isMixedContent` then names exactly what will happen.
 */
export function normalizeServer(raw, pageProtocol = 'https:') {
  const text = String(raw ?? '').trim();
  if (!text) return { ok: false, reason: 'empty' };
  const scheme = /^[a-z][a-z0-9+.-]*:\/\//i.test(text);
  // ⚠ The page's protocol is only borrowable when it is one of the two this can use. A dist
  // opened off a disk is a `file:` page, and inheriting that turns every schemeless address
  // into `file://host` — refused two lines below, with a message telling the reader that a
  // perfectly good address is not http or https.
  const inherited = pageProtocol === 'http:' || pageProtocol === 'https:' ? pageProtocol : 'https:';
  const candidate = scheme ? text : `${inherited}//${text.replace(/^\/+/, '')}`;
  let url;
  try {
    url = new URL(candidate);
  } catch {
    return { ok: false, reason: 'unparseable' };
  }
  if (url.protocol !== 'http:' && url.protocol !== 'https:') return { ok: false, reason: 'scheme' };
  if (!url.hostname) return { ok: false, reason: 'unparseable' };
  url.username = '';
  url.password = '';
  url.search = '';
  url.hash = '';
  // A trailing slash makes the result a *base*: `new URL('api/v1/health', base)` then resolves
  // inside it rather than beside it, which is what lets a reverse proxy mount velmd at a
  // subpath without every call site knowing.
  if (!url.pathname.endsWith('/')) url.pathname += '/';
  return { ok: true, base: url.href, origin: url.origin };
}

/** One of velmd's routes, under a base from [`normalizeServer`]. */
export function apiUrl(base, path) {
  return new URL(`api/v1/${path}`, base).href;
}

/**
 * Where a stored token lives, **keyed by origin**.
 *
 * ⚠ This is the security property of this whole file, not a naming convention. `?server=` is
 * attacker-supplied, so a link can point this page at a host of somebody else's choosing; a
 * token saved for the user's own server must never be attached to a request to that host. A
 * per-origin key makes that structural — there is no lookup that could return it — where a
 * single `velm.token` would have made it a matter of remembering to check.
 *
 * The **origin**, not the base: a path prefix is not a security boundary, and a browser will
 * happily send a same-origin request to any path.
 */
export function tokenKey(base) {
  return `${TOKEN_PREFIX}${new URL(base).origin}`;
}

// ─────────────────────────────────────────────────────────────────────────────
// What the browser will and will not do
// ─────────────────────────────────────────────────────────────────────────────

/**
 * Whether a request from this page to that server is blocked before it leaves.
 *
 * ⚠ Checked **before** fetching, because afterwards it cannot be. A mixed-content refusal
 * arrives as `TypeError: Failed to fetch`, byte for byte what a dead server produces, and
 * telling somebody to check whether their server is running when the browser refused to ask is
 * the confidently-wrong error message this file exists to avoid.
 */
export function isMixedContent(pageProtocol, base) {
  if (pageProtocol !== 'https:') return false;
  let url;
  try {
    url = new URL(base);
  } catch {
    return false;
  }
  if (url.protocol !== 'http:') return false;
  // Loopback is exempt: browsers treat it as a potentially trustworthy origin, so an https
  // page may reach `http://localhost` and an `http://localhost` page is itself secure.
  return !isLoopbackHost(url.hostname);
}

/**
 * Whether the **viewer** will have `navigator.gpu` when it opens on that server.
 *
 * `navigator.gpu` is `[SecureContext]`. This is why a board can list perfectly and then open to
 * a blank page: the list is an ordinary fetch and the board is a GPU. Warned about here rather
 * than discovered there, where the only symptom is a wasm error nobody can read.
 */
export function viewerIsSecureContext(base) {
  let url;
  try {
    url = new URL(base);
  } catch {
    return false;
  }
  if (url.protocol === 'https:') return true;
  return isLoopbackHost(url.hostname);
}

/**
 * The link that opens one board in the viewer.
 *
 * `server.local` is the whole decision — see this file's header. A local board opens in the
 * copy of `index.html` sitting beside this page; a remote board opens in the remote's own copy,
 * because the viewer only ever fetches its own origin.
 *
 * The token rides in the query string and there is no alternative: this is a **navigation**,
 * and a navigation cannot carry a header. `serve.rs` accepts `?token=` for exactly this reason
 * and calls it the weaker of the two forms, which it is — the URL lands in history. What makes
 * it acceptable is that it is one person's own server.
 */
export function buildViewerUrl(server, id, token, pageHref) {
  const target = server.local
    ? new URL('./index.html', pageHref)
    : new URL('index.html', server.base);
  target.search = '';
  target.hash = '';
  target.searchParams.set('board', String(id));
  if (token) target.searchParams.set('token', token);
  return target.href;
}

// ─────────────────────────────────────────────────────────────────────────────
// Storage
// ─────────────────────────────────────────────────────────────────────────────

/**
 * The remembered server and passphrase.
 *
 * ⚠ **The address is `localStorage` and the passphrase is `sessionStorage` unless it is asked
 * for.** The case this page is written for is a borrowed Windows machine, and a passphrase that
 * outlives the tab on a borrowed machine is a copy of somebody's boards left behind. A session
 * ends when the tab closes, which is exactly the lifetime a borrowed machine has. The address
 * is not a secret and re-typing it is the friction that makes people write it down, so it
 * persists — and *Forget this device* clears both, from both stores, for every origin.
 *
 * Every access is wrapped: reading `localStorage` **throws** in Safari's private mode and
 * wherever storage is disabled, and a picker that will not render because it could not read a
 * preference is worse than one that forgets.
 */
export function createStore(stores = {}) {
  const local = stores.local ?? null;
  const session = stores.session ?? null;
  const read = (s, k) => {
    try {
      return s ? s.getItem(k) : null;
    } catch {
      return null;
    }
  };
  const write = (s, k, v) => {
    try {
      if (s) s.setItem(k, v);
    } catch {
      /* private mode, a full quota — a forgotten preference is not a failure */
    }
  };
  const drop = (s, k) => {
    try {
      if (s) s.removeItem(k);
    } catch {
      /* as above */
    }
  };
  return {
    getServer() {
      return read(local, SERVER_KEY) || read(session, SERVER_KEY);
    },
    setServer(base) {
      write(local, SERVER_KEY, base);
    },
    getToken(base) {
      const key = tokenKey(base);
      // Session first: a passphrase typed in this tab is a more recent statement of intent
      // than one remembered months ago, and the two differing is exactly the rotate-it case.
      return read(session, key) || read(local, key);
    },
    setToken(base, token, remember) {
      const key = tokenKey(base);
      drop(session, key);
      drop(local, key);
      if (!token) return;
      write(remember ? local : session, key, token);
    },
    isRemembered(base) {
      return read(local, tokenKey(base)) !== null;
    },
    forget() {
      for (const store of [local, session]) {
        if (!store) continue;
        const keys = [];
        try {
          for (let i = 0; i < store.length; i += 1) {
            const key = store.key(i);
            if (key) keys.push(key);
          }
        } catch {
          continue;
        }
        // ⚠ Collected first, then removed. Removing inside the walk shifts every later index
        // down and skips half the keys — and half a forgotten passphrase is not forgotten.
        for (const key of keys) {
          if (key === SERVER_KEY || key.startsWith(TOKEN_PREFIX)) drop(store, key);
        }
      }
    },
  };
}

/** `localStorage` and `sessionStorage`, or `null` where touching them throws. */
function safeStorage(win, name) {
  try {
    const store = win[name];
    // Reading the property is itself what throws in a browser with storage disabled; a probe
    // write catches the quota-zero case that only fails later.
    const probe = '__velm_probe__';
    store.setItem(probe, '1');
    store.removeItem(probe);
    return store;
  } catch {
    return null;
  }
}

// ─────────────────────────────────────────────────────────────────────────────
// Saying what went wrong
// ─────────────────────────────────────────────────────────────────────────────

/**
 * Turn a failure into something worth reading.
 *
 * Every branch names a **different thing to do**, which is the test of whether a message is
 * worth having. The hard cases are the ones a browser reports identically:
 *
 * - **Mixed content** and a dead server are the same `TypeError`, so the protocol comparison
 *   decides it rather than the error.
 * - **A missing `--app-origin`** and a dead server are also the same `TypeError` — the response
 *   exists and this page is not allowed to look at it. `reachable` is the answer from a
 *   `no-cors` probe, which resolves whenever *something* replied, and is the only way to tell
 *   the two apart from inside a page.
 * - **Private Network Access** cannot be told from either, so it is named as a possibility and
 *   never as a diagnosis.
 *
 * `want` is what the page should offer next, so the caller never has to re-derive the branch:
 * `'token'` raises the passphrase field, `'server'` raises the address field, `'retry'` offers
 * the same request again.
 */
export function describeFailure(context) {
  const {
    phase = 'boards',
    status = null,
    reason = null,
    implicit = false,
    pageProtocol = 'https:',
    pageOrigin = '',
    base = '',
    reachable = null,
    hasToken = false,
  } = context;

  let host = base;
  try {
    host = new URL(base).host;
  } catch {
    /* keep the raw string; it is only ever used as a label */
  }

  if (status === 401) {
    return {
      want: 'token',
      title: hasToken ? 'That passphrase is not right.' : 'This server needs a passphrase.',
      detail: hasToken
        ? `${host} answered 401. The passphrase is the VELMD_TOKEN the server was started with.`
        : `${host} is holding boards behind a passphrase — the VELMD_TOKEN it was started with.`,
    };
  }

  if (reason === 'not-velmd') {
    return {
      want: 'server',
      title: 'That address answered, but it is not a Velm server.',
      detail: `${host} did not answer /api/v1/health with a velmd version. Check the address, and the port — velmd's own default is 8787.`,
    };
  }

  if (typeof status === 'number' && status >= 400) {
    return {
      want: 'retry',
      title: `${host} answered ${status}.`,
      detail:
        phase === 'boards'
          ? 'The server is running and reachable; it refused to list its boards.'
          : 'The server is reachable and something between here and it is unhappy.',
    };
  }

  if (isMixedContent(pageProtocol, base)) {
    return {
      want: 'server',
      title: 'A page on https cannot reach a server on http.',
      detail: `This page is served over https and ${host} is http, so the browser blocks the request before it is sent. Give the server https, or open this page from the server itself — which is what velmd serve --web does.`,
    };
  }

  if (reason === 'timeout') {
    return {
      want: 'retry',
      title: `${host} did not answer in time.`,
      detail: 'It may be starting up, or reading a large board directory. Trying again is worth one go before changing the address.',
    };
  }

  if (reachable === true) {
    return {
      want: 'retry',
      title: 'The server answered, and this page is not allowed to read it.',
      detail: `${host} replied, but without a header naming ${pageOrigin || 'this page'} the browser hides the answer. Start velmd with --app-origin ${pageOrigin || 'https://this-page'} — or open the boards from the server's own address, where no such header is needed.`,
    };
  }

  if (implicit && phase === 'health') {
    return {
      want: 'server',
      title: 'This page is not being served by a Velm server.',
      detail: 'It is being served as plain files, so there are no boards here. Enter the address your velmd server is on.',
    };
  }

  let detail = `Nothing answered at ${host}. Check the address and the port, and that velmd serve is running on it.`;
  let hostname = '';
  try {
    hostname = new URL(base).hostname;
  } catch {
    /* as above */
  }
  if (hostname && isPrivateHost(hostname) && !isPrivateHost(originHost(pageOrigin))) {
    detail += ' A page on the public internet is also blocked from reaching a private address by the browser itself, which looks exactly like this — reaching that server needs a name and a certificate of its own.';
  }
  return { want: 'server', title: `Could not reach ${host}.`, detail };
}

function originHost(origin) {
  try {
    return new URL(origin).hostname;
  } catch {
    return '';
  }
}

// ─────────────────────────────────────────────────────────────────────────────
// The page
// ─────────────────────────────────────────────────────────────────────────────

const IDS = {
  sub: 'velm-sub',
  message: 'velm-message',
  messageTitle: 'velm-message-title',
  messageDetail: 'velm-message-detail',
  warning: 'velm-warning',
  connect: 'velm-connect',
  server: 'velm-server',
  passphrase: 'velm-passphrase',
  remember: 'velm-remember',
  // Looked up and deliberately never bound: the form's own `submit` covers the button, the
  // Enter key and an on-screen keyboard's Go alike. It is in this list so that removing the
  // button is a loud failure rather than a form nobody without a keyboard can send.
  submit: 'velm-connect-go',
  boards: 'velm-boards',
  where: 'velm-where',
  change: 'velm-change',
  forget: 'velm-forget',
  retry: 'velm-retry',
};

/**
 * Draw the picker.
 *
 * Everything it needs from the outside is an argument, so a harness can drive it; nothing runs
 * until it is called, so importing this module is free.
 */
export function mount(options = {}) {
  const doc = options.document ?? document;
  const win = options.window ?? window;
  const fetchImpl = options.fetch ?? ((...args) => win.fetch(...args));
  const store =
    options.store ??
    createStore({ local: safeStorage(win, 'localStorage'), session: safeStorage(win, 'sessionStorage') });

  const el = {};
  for (const [name, id] of Object.entries(IDS)) {
    el[name] = doc.getElementById(id);
    // ⚠ Loud. A renamed id would otherwise make one control silently do nothing, which is this
    // repository's signature defect — code that compiles, passes its tests and is not reachable.
    if (!el[name]) throw new Error(`boards.html is missing #${id}`);
  }

  const params = new URLSearchParams(win.location.search);
  let server = resolveServer(win, store, params.get('server'));
  let token = params.get('token') || store.getToken(server.base) || null;

  // ⚠ A token that arrived in the URL is put away and taken out of the address bar.
  //
  // `chrome.js`'s Back button delivers it that way on every return from a board, so without
  // this the picker's own URL carries the passphrase for the life of the tab — on the borrowed
  // machine this page is written for, that is the one thing worth not leaving on screen. The
  // cost is stated rather than hidden: a *new* tab has no session storage and asks again.
  if (params.get('token')) {
    store.setToken(server.base, token, store.isRemembered(server.base));
    scrubToken(win);
  }

  let boards = [];

  el.connect.addEventListener('submit', (event) => {
    event.preventDefault();
    const typed = normalizeServer(el.server.value, win.location.protocol);
    if (!typed.ok) {
      show(el.message, {
        title:
          typed.reason === 'empty'
            ? 'Enter the address your server is on.'
            : 'That does not read as a web address.',
        detail:
          typed.reason === 'scheme'
            ? 'Only http:// and https:// addresses can hold boards.'
            : 'Something like https://boards.example.com, or 192.168.1.20:8787 on your own network.',
      });
      el.server.focus();
      return;
    }
    const remember = Boolean(el.remember.checked);
    server = { base: typed.base, local: typed.origin === win.location.origin, implicit: false, fromQuery: false };
    // ⚠ **An empty field means "leave the passphrase alone", not "clear it".** A passphrase
    // reaches this page in a link as often as it is typed — `chrome.js`'s Back button carries
    // one on every return from a board, and a bookmark carries one on every visit — and it is
    // deliberately never written into this field, because a secret in a DOM node is a secret in
    // the page. Reading the field as authoritative therefore threw away the working passphrase
    // the moment somebody pressed Connect to confirm a server, and the next request was a 401
    // for a passphrase that had just been deleted. Clearing one is what *Forget this device* is
    // for, and it says so.
    const typed_passphrase = el.passphrase.value.trim();
    token = typed_passphrase || store.getToken(server.base) || null;
    store.setServer(server.base);
    // Re-written even when it did not change, so that ticking *Remember* over a passphrase
    // already held for this session promotes it rather than doing nothing.
    store.setToken(server.base, token, remember);
    el.passphrase.value = '';
    connect();
  });

  el.change.addEventListener('click', () => openConnectForm());
  el.retry.addEventListener('click', () => connect());
  el.forget.addEventListener('click', () => {
    store.forget();
    // Back to a bare page rather than a redraw: the point of forgetting is that nothing about
    // this person survives in the tab, and the address bar is part of the tab.
    win.location.replace(new URL(win.location.pathname, win.location.href).href);
  });

  const remembered = store.getServer();
  const rememberedBase = remembered ? (normalizeServer(remembered, win.location.protocol).base ?? null) : null;
  if (needsServerConfirmation(server, rememberedBase)) {
    el.sub.textContent = 'Not connected.';
    show(el.message, {
      title: `This link points Velm at ${label(server.base)}.`,
      detail: 'That is not a server this browser has used before. Check the address before connecting — every board on the list below would open as a page that server serves.',
    });
    openConnectForm();
    el.forget.hidden = false;
  } else {
    connect();
  }

  // ── the request ───────────────────────────────────────────────────────────

  async function connect() {
    hide(el.message);
    hide(el.warning);
    hide(el.connect);
    el.boards.replaceChildren();
    el.retry.hidden = true;
    el.sub.textContent = `Asking ${label(server.base)}…`;
    el.where.textContent = '';

    const context = {
      implicit: server.implicit,
      pageProtocol: win.location.protocol,
      pageOrigin: win.location.origin,
      base: server.base,
      hasToken: Boolean(token),
    };

    // ⚠ Before any fetch. See `isMixedContent`.
    if (isMixedContent(win.location.protocol, server.base)) {
      return failed({ ...context, phase: 'health', reason: 'network' });
    }

    // Health is ungated on purpose — `serve.rs` keeps it outside the token so "not running" and
    // "wrong passphrase" stay distinguishable — so it is asked **without** the passphrase. That
    // keeps it a pure reachability signal: a 401 here would mean the gate moved, not that the
    // passphrase is wrong.
    const health = await request(apiUrl(server.base, 'health'), { cache: 'no-store' }, HEALTH_TIMEOUT_MS);
    if (health.error) {
      const reachable = server.local ? null : await probeReachable(server.base);
      return failed({
        ...context,
        phase: 'health',
        reason: health.timedOut ? 'timeout' : 'network',
        reachable,
      });
    }
    if (!health.response.ok) {
      return failed({
        ...context,
        phase: 'health',
        status: health.response.status,
        reason: health.response.status === 404 ? 'not-velmd' : null,
      });
    }
    const version = await velmdVersion(health.response);
    if (version === null) {
      return failed({ ...context, phase: 'health', reason: 'not-velmd' });
    }

    // ⚠ The passphrase goes in a header, not the query string. Cross-origin that costs a
    // preflight, which `serve.rs` answers 204 with the CORS headers on it; what it buys is a
    // passphrase that is never in a URL, never in history and never in an access log. The only
    // place it has to be a query parameter is the board link, which is a navigation.
    const headers = token ? { authorization: `Bearer ${token}` } : undefined;
    const list = await request(apiUrl(server.base, 'boards'), { cache: 'no-store', headers }, BOARDS_TIMEOUT_MS);
    if (list.error) {
      const reachable = server.local ? null : await probeReachable(server.base);
      return failed({
        ...context,
        phase: 'boards',
        reason: list.timedOut ? 'timeout' : 'network',
        reachable,
      });
    }
    if (!list.response.ok) {
      return failed({ ...context, phase: 'boards', status: list.response.status });
    }

    let payload;
    try {
      payload = await list.response.json();
    } catch {
      payload = null;
    }
    // A 200 carrying something that is not an array — a captive portal's login page, a proxy's
    // error shape — passes every check above and then throws `boards is not iterable` out of
    // the render, leaving a blank page. `index.html` learned this the same way.
    if (!Array.isArray(payload)) {
      return failed({ ...context, phase: 'boards', reason: 'not-velmd' });
    }

    boards = payload;
    if (!server.implicit) store.setServer(server.base);
    render(version);
  }

  async function request(url, init, timeoutMs) {
    const controller = new AbortController();
    // ⚠ Every await on a browser is bounded. `docs/08-web.md` §6 records three separate hangs
    // in the probe from missing exactly this, and a page stuck on "Asking…" on a tablet is
    // indistinguishable from the blank screen it exists to explain.
    const timer = win.setTimeout(() => controller.abort(), timeoutMs);
    try {
      return { response: await fetchImpl(url, { ...init, signal: controller.signal }) };
    } catch (error) {
      return { error, timedOut: controller.signal.aborted };
    } finally {
      win.clearTimeout(timer);
    }
  }

  /**
   * Did *anything* answer at that address?
   *
   * `mode: 'no-cors'` gives an opaque response that says nothing about the status — and that is
   * the whole point: it resolves whenever the server replied at all, so it separates "the
   * server is not there" from "the server is there and this page may not read it". Run only
   * after a cross-origin failure, and only against the ungated health route, because a `no-cors`
   * request may not carry the passphrase header anyway.
   */
  async function probeReachable(base) {
    const probe = await request(apiUrl(base, 'health'), { mode: 'no-cors', cache: 'no-store' }, HEALTH_TIMEOUT_MS);
    return !probe.error;
  }

  async function velmdVersion(response) {
    try {
      const body = await response.json();
      if (body && typeof body === 'object' && typeof body.velmd === 'string') return body.velmd;
    } catch {
      /* fall through */
    }
    return null;
  }

  // ── drawing ───────────────────────────────────────────────────────────────

  function failed(context) {
    const answer = describeFailure(context);
    show(el.message, answer);
    el.sub.textContent = 'Not connected.';
    el.where.textContent = '';
    el.retry.hidden = answer.want !== 'retry';
    if (answer.want === 'server') openConnectForm();
    if (answer.want === 'token') openConnectForm({ focus: 'passphrase' });
    el.change.hidden = answer.want === 'server' || answer.want === 'token';
    el.forget.hidden = false;
  }

  function render(version) {
    el.sub.textContent =
      boards.length === 0
        ? 'This server is not holding any boards yet.'
        : `${boards.length} ${boards.length === 1 ? 'board' : 'boards'}, most recently changed first.`;
    el.where.textContent = `${label(server.base)} · velmd ${version}`;
    el.change.hidden = false;
    el.forget.hidden = false;

    // ⚠ Named before it is discovered. The list is an ordinary fetch and the board is a GPU, so
    // an http server on a LAN lists perfectly and then opens to a blank canvas.
    if (!viewerIsSecureContext(server.base)) {
      show(el.warning, {
        title: 'A board opened from here will not draw.',
        detail: `Boards open at ${label(server.base)}, which is http. A browser only gives WebGPU to https and to localhost, so the page will load and the board will stay blank. Reaching that server over https, or over http://localhost on the machine running it, is what fixes it.`,
      });
    }

    for (const board of boards) {
      const id = typeof board?.id === 'string' ? board.id : null;
      if (!id) continue;
      el.boards.append(card(board, id));
    }

    if (!server.local) checkRemoteClient();
  }

  function card(board, id) {
    const link = doc.createElement('a');
    link.className = 'velm-card';
    link.href = buildViewerUrl(server, id, token, win.location.href);

    const name = doc.createElement('strong');
    // ⚠ `textContent`, never `innerHTML`. This is a string somebody else wrote, arriving over
    // the wire from `/api/v1/boards` — `chrome.js` carries the same warning over the same field.
    name.textContent = typeof board.title === 'string' && board.title ? board.title : id;
    link.append(name);

    const meta = doc.createElement('span');
    meta.className = 'velm-card-meta';
    meta.textContent = describeBoard(board);
    link.append(meta);
    return link;
  }

  /**
   * The last check, and it is allowed to fail.
   *
   * A remote `velmd serve` started without `--web` holds boards and serves no client, so every
   * link on this page is a 404 nobody can diagnose. One `HEAD` names it. Wrapped and ignored on
   * error: a check that cannot run must not take the list down with it.
   */
  async function checkRemoteClient() {
    const viewer = buildViewerUrl(server, '', null, win.location.href);
    const probe = await request(viewer, { method: 'HEAD', cache: 'no-store' }, HEALTH_TIMEOUT_MS);
    if (probe.error || probe.response.ok) return;
    // ⚠ Added to whatever is already there rather than written over it. An http server on a LAN
    // with no `--web` raises both warnings, and replacing the first would report the smaller of
    // two problems as though it were the only one.
    show(
      el.warning,
      {
        title: 'That server is holding boards but is not serving Velm.',
        detail: `${label(server.base)} answered ${probe.response.status} for the viewer, so opening a board from here has nowhere to land. Restart it with --web pointed at the built client: velmd serve --web web/dist`,
      },
      !el.warning.hidden,
    );
  }

  function openConnectForm(opts = {}) {
    el.connect.hidden = false;
    if (!el.server.value) el.server.value = server.implicit ? '' : displayBase(server.base);
    el.remember.checked = store.isRemembered(server.base);
    el.change.hidden = true;
    const field = opts.focus === 'passphrase' ? el.passphrase : el.server;
    try {
      field.focus();
    } catch {
      /* a harness has no focus */
    }
  }

  function show(node, { title, detail }, alsoKeepWhatIsThere = false) {
    if (node === el.message) {
      el.messageTitle.textContent = title;
      el.messageDetail.textContent = detail;
    } else {
      if (!alsoKeepWhatIsThere) node.replaceChildren();
      const strong = doc.createElement('strong');
      strong.textContent = title;
      const paragraph = doc.createElement('p');
      paragraph.textContent = detail;
      node.append(strong, paragraph);
    }
    node.hidden = false;
  }

  function hide(node) {
    node.hidden = true;
  }

  // Handed back so a harness can drive the page without a click, and so `boards.html` could
  // reconnect after a visibility change if it ever wants to.
  return { connect, get server() { return server; }, get boards() { return boards; } };
}

/**
 * How many items, and when it changed if the server says.
 *
 * `modified` is milliseconds since the epoch, formatted in the reader's own locale rather
 * than the server's. `boards_json` in `crates/velmd/src/serve.rs` sends it, and the listing
 * is sorted by it, newest first — which the heading says, because the ordering is the thing
 * a reader actually navigates by.
 *
 * A missing or zero value renders **no date at all** rather than 1970: a clock the file
 * predates is a fact about the filesystem, not about the board, and an obviously wrong date
 * is worse than none. That is why this degrades instead of falling back.
 *
 * ⚠ This doc said the field *was not sent* and told the next reader the branch was dead and
 * could be deleted — written against the server as it was that morning, and stale by the same
 * commit, which added it. A comment that instructs somebody to delete working code is the
 * `locked: false` trap pointed at the reader instead of at the compiler.
 */
function describeBoard(board) {
  const count = Number(board?.items);
  const parts = [];
  if (Number.isFinite(count)) parts.push(count === 1 ? '1 item' : `${count} items`);
  const modified = Number(board?.modified);
  if (Number.isFinite(modified) && modified > 0) {
    const when = new Date(modified);
    if (!Number.isNaN(when.getTime())) {
      parts.push(when.toLocaleDateString(undefined, { year: 'numeric', month: 'short', day: 'numeric' }));
    }
  }
  return parts.join(' · ');
}

/** The server to ask, most specific first. */
function resolveServer(win, store, queryServer) {
  const protocol = win.location.protocol;
  const candidates = [
    [queryServer, true],
    [store.getServer(), false],
  ];
  for (const [raw, fromQuery] of candidates) {
    if (!raw) continue;
    const parsed = normalizeServer(raw, protocol);
    if (parsed.ok) {
      return {
        base: parsed.base,
        local: parsed.origin === win.location.origin,
        implicit: false,
        fromQuery,
      };
    }
  }
  // Nothing chosen: this page's own origin, which is right whenever velmd is serving it — the
  // one-origin arrangement `docs/08-web.md` §5 argues for. `implicit` is what lets a failure
  // here say "this page is not a Velm server" instead of "your server is down".
  return { base: new URL('/', win.location.href).href, local: true, implicit: true, fromQuery: false };
}

/**
 * Whether a `?server=` should be shown to the reader before it is used.
 *
 * ⚠ **`?server=` is in a link anybody can send.** Refusing to attach a stored passphrase to a
 * foreign origin (see [`tokenKey`]) stops the secret leaving, and it does not stop the other
 * half: a page wearing Velm's own chrome, listing a stranger's boards, whose every link leads
 * to a page that stranger serves — which is where a convincing request for the passphrase
 * would be made. So an address arriving in a link is *offered* rather than used.
 *
 * **Once**, though, not every time. A server this browser has already accepted is the one it
 * remembered, so a bookmark or a QR code pointed at your own machine costs one confirmation
 * ever, and an address on this page's own origin costs none — there is nowhere new to go.
 */
export function needsServerConfirmation(server, rememberedBase) {
  if (!server.fromQuery || server.local) return false;
  return server.base !== rememberedBase;
}

function scrubToken(win) {
  try {
    const url = new URL(win.location.href);
    if (!url.searchParams.has('token')) return;
    url.searchParams.delete('token');
    win.history.replaceState(null, '', `${url.pathname}${url.search}${url.hash}`);
  } catch {
    /* no history, or a harness — the token is already stored, which is the part that matters */
  }
}

/** A server's address as a person would say it: no scheme, no trailing slash. */
function label(base) {
  try {
    const url = new URL(base);
    return `${url.host}${url.pathname === '/' ? '' : url.pathname.replace(/\/$/, '')}`;
  } catch {
    return base;
  }
}

/** A server's address as it should go back into the field: with its scheme. */
function displayBase(base) {
  try {
    const url = new URL(base);
    return `${url.origin}${url.pathname === '/' ? '' : url.pathname.replace(/\/$/, '')}`;
  } catch {
    return base;
  }
}
