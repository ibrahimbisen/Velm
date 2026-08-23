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
// # The library, and why it is a port rather than a design
//
// *"please make the menu exactly like the mac app please"*. So the arrangement here is not
// chosen, it is **transcribed** from `crates/vellum-ui/src/library.rs` — the sidebar's four
// standing scopes and its folders, the Recent page's three bands, All boards' folder grouping,
// the 236 × 176 card with its 108-point picture well, and the exact arithmetic that decides how
// many cards make one row. Where a number appears below it is that file's number and the line
// it came from is named. **`library.rs` is the authority; this file is a copy of it and must
// lose every argument with it.**
//
// The rules with a reason behind them, so nobody re-derives them:
//
// - **A starred board is drawn twice on the Recent page** — once in *Recently opened* and again
//   under *Pinned* — and that is the user's own overruling of the tidier arrangement.
//   `recentBands` carries the reasoning.
// - **The card's internal leading is 4, not 16.** 16 is the gutter *between* cards; feedback 22
//   spent a round separating the two after an inherited spacing ate 24 points of every card.
// - **The title is not bold.** See `libraryStyle`.
//
// # ⚠ What this client cannot do, and how that is said
//
// Velm's board library is also where a board is starred, filed, renamed, duplicated, trashed
// and restored. **Of those, `velmd` has create and rename (`manage.rs`) and no others** — the whole HTTP surface is `GET`s
// plus `sync`, `import` and a create — so none of them is drawn here. Not drawn *disabled*:
// omitted, except where the omission would itself be confusing, which is the trash. CLAUDE.md
// names describing a gesture the user cannot perform as the worst failure in its own list, and
// a card menu whose every row is greyed out is six of them in a column.
//
// A star is still **shown** on a board that has one. Reading a state is not setting it, and a
// Pinned band with unmarked cards in it would be a section that cannot explain itself.
//
// # ⚠ Everything above `mount` is testable without a browser
//
// `mount()` is the only function that touches a DOM, and it takes its `document`, `window`,
// `fetch` and storage as arguments. Nothing runs at import time. That is not tidiness: it is
// the only way a `node --input-type=module` harness can drive the URL builder, the token
// lookup, the failure classifier and now the **banding** — the pieces most able to be wrong
// while looking right — and A/B them against a deliberately broken line.

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

/**
 * A card's width, and the gutter between two of them.
 *
 * `library.rs:460,605` — `space::of(59)` and `space::of(4)` on a 4-point unit. They are here
 * rather than in the stylesheet because [`columnsThatFit`] has to do arithmetic with them, and
 * a grid whose CSS and whose column count disagree wraps in a place the banding does not expect.
 */
export const CARD_WIDTH = 236;
export const CARD_GUTTER = 16;

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

/**
 * Where a board's preview picture lives, or `null`.
 *
 * ⚠ **This is the one place a passphrase goes into a URL that is not a navigation**, and it is
 * worth being explicit about what that does and does not cost. `/api/v1/blobs/{hash}` is gated
 * like everything under `/api/v1/`, an `<img src>` cannot be handed a header, and `serve.rs`
 * accepts `?token=` on any gated route (`authorised(&headers, query, expected)`). So the choice
 * is a query token or no pictures. It is **not a new exposure on this page**: every card's
 * `href` already carries the same token by the same mechanism, so the secret is already in this
 * document and already reaches history the moment a board is opened. What it must never become
 * is a *cross-origin* leak, and it cannot: the URL is built under `server.base`, and the token
 * handed in is the one [`tokenKey`] already refused to produce for a foreign origin.
 *
 * The hash is checked against 64 hex characters before it is used. `serve.rs`'s own defence is
 * that a blob id parses to 32 bytes or does not exist, which is total — this is the same test
 * applied one hop earlier, so a server that ever sent something else builds no URL rather than
 * an odd one.
 */
export function thumbnailUrl(server, board, token) {
  const hash = board && typeof board.thumbnail === 'string' ? board.thumbnail : '';
  if (!/^[0-9a-f]{64}$/i.test(hash)) return null;
  const url = new URL(apiUrl(server.base, `blobs/${hash}`));
  if (token) url.searchParams.set('token', token);
  return url.href;
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
// The library, as data — every function below this line is pure
// ─────────────────────────────────────────────────────────────────────────────

/**
 * A timestamp from the wire, in milliseconds.
 *
 * ⚠ **`modified` is milliseconds and `trashed` is seconds**, because they come from two places:
 * `boards_json` in `crates/velmd/src/serve.rs` sends `duration_since(UNIX_EPOCH).as_millis()`,
 * while the deletion time originates in the desktop's `library.json` sidecar, whose stamps are
 * whole seconds. Getting that wrong is not subtle in one direction and is invisible in the
 * other: a seconds value read as milliseconds lands in **January 1970** and every card in the
 * trash reads *"deleted 56 years ago"*, while a milliseconds value read as seconds lands
 * somewhere around the year 57000 and `relativeTime` answers *"just now"* for ever, which looks
 * perfectly reasonable and is wrong.
 *
 * So the width is decided from the number rather than from the field name: a real timestamp in
 * seconds is ten digits until the year 2286, and the same instant in milliseconds is thirteen.
 * `1e12` sits between the two with more than two centuries of clearance on either side.
 */
export function toMillis(value) {
  const n = Number(value);
  if (!Number.isFinite(n) || n <= 0) return 0;
  return n < 1e12 ? n * 1000 : n;
}

/**
 * "just now", "12 minutes ago", "3 days ago".
 *
 * A straight port of `library.rs:362`, months at 30 days and years at 365, including the branch
 * for a stamp in the future — a copied file or a clock change reads as current rather than as a
 * negative age.
 *
 * Relative rather than absolute for that function's own reason: the question this line answers
 * is *"is this the one I had open yesterday"*, not *"what was the date"*. The exact date is on
 * the card as a tooltip, which costs nothing and is there when the relative answer is not enough.
 */
export function relativeTime(atMs, nowMs) {
  const at = Number(atMs);
  const now = Number(nowMs);
  if (!Number.isFinite(at) || !Number.isFinite(now) || at <= 0) return '';
  const seconds = Math.floor((now - at) / 1000);
  if (seconds < 60) return 'just now';
  const plural = (n, unit) => (n === 1 ? `1 ${unit} ago` : `${n} ${unit}s ago`);
  if (seconds < 3600) return plural(Math.floor(seconds / 60), 'minute');
  if (seconds < 86400) return plural(Math.floor(seconds / 3600), 'hour');
  if (seconds < 2592000) return plural(Math.floor(seconds / 86400), 'day');
  if (seconds < 31536000) return plural(Math.floor(seconds / 2592000), 'month');
  return plural(Math.floor(seconds / 31536000), 'year');
}

/** `library.rs:454` — "1 item" for one, "N items" otherwise. */
export function itemCountLabel(count) {
  const n = Number(count);
  if (!Number.isFinite(n)) return '';
  return n === 1 ? '1 item' : `${n} items`;
}

/**
 * Both wire shapes, as one thing to draw.
 *
 * ⚠ **`GET /api/v1/library` is newer than some servers this page will meet.** An older `velmd`
 * has no such route, so the request falls through `route` to `static_file`, which answers a
 * plain **404** — and a 404 there is unambiguous, because the token gate runs *before* routing,
 * so a missing passphrase is a 401 and can never arrive here dressed as an old server. The
 * caller retries `GET /api/v1/boards`, which every version has, and this normalises the flat
 * array it returns into the same shape with no stars, no folders and nothing in the trash. The
 * page then draws one grid: `legacy` is what the sidebar reads to leave out four scopes it has
 * no data for. **An old server must produce a plainer page, never a broken one.**
 *
 * Everything is re-derived rather than trusted. `id` decides whether a row exists at all,
 * because it is the only field that has to be right — it is the file stem the snapshot route
 * matches on — and every other field degrades to a sensible absence. `space` is a **name**: the
 * wire carries no paths, deliberately (`boards_json`'s own ⚠), so nothing on this page can ever
 * put somebody's home directory in a URL bar or a screenshot.
 */
export function normalizeLibrary(payload) {
  const legacy = Array.isArray(payload);
  const rawBoards = legacy ? payload : payload && Array.isArray(payload.boards) ? payload.boards : null;
  if (!rawBoards) return null;
  const rawSpaces = !legacy && payload && Array.isArray(payload.spaces) ? payload.spaces : [];

  const boards = [];
  for (const row of rawBoards) {
    if (!row || typeof row.id !== 'string' || !row.id) continue;
    boards.push({
      id: row.id,
      title: typeof row.title === 'string' && row.title ? row.title : row.id,
      items: Number.isFinite(Number(row.items)) ? Number(row.items) : null,
      modified: toMillis(row.modified),
      starred: row.starred === true,
      space: typeof row.space === 'string' && row.space ? row.space : null,
      trashed: row.trashed == null ? null : toMillis(row.trashed) || null,
      thumbnail: typeof row.thumbnail === 'string' ? row.thumbnail : null,
    });
  }

  const spaces = [];
  for (const row of rawSpaces) {
    if (!row || typeof row.name !== 'string' || !row.name) continue;
    // `boards` is a count on the wire. Tolerating an array as well costs one line and means a
    // server that sends the fuller shape later does not draw "NaN boards" on every folder.
    const count = Array.isArray(row.boards) ? row.boards.length : Number(row.boards);
    spaces.push({ name: row.name, pinned: row.pinned === true, boards: Number.isFinite(count) ? count : 0 });
  }

  return { boards, spaces, legacy };
}

/** `library.rs:354` — title only, case-insensitive, a plain substring. */
export function matchesSearch(board, query) {
  const needle = String(query || '').trim().toLowerCase();
  if (!needle) return true;
  return String(board.title || '').toLowerCase().includes(needle);
}

/**
 * The boards one scope shows, in that scope's own order.
 *
 * A port of `LibraryState::in_scope` (`library.rs`) and the sort above it, and the two rules
 * that matter are both in the Rust:
 *
 * - **A deleted board is in exactly one scope.** Filtered before anything else, so it is out of
 *   Recent, All boards, Starred *and the folder it is still filed under* — while keeping that
 *   filing, which on the desktop is the difference between *restore* and *make a new board with
 *   the same name*. Here it only decides what the trash card says its folder is, and it is
 *   ported anyway: a divergence in a filter is how two pages come to disagree about a count.
 * - **All boards never filters.** It is the scope somebody falls back to when they cannot
 *   remember where they put something.
 *
 * Recent is newest first; every other scope is case-insensitive title order. The server already
 * sends the list newest first, and this sorts anyway — a page whose banding silently depends on
 * somebody else's ordering breaks the day that ordering is tuned.
 */
export function boardsInScope(boards, scope, search = '') {
  const kind = scope && scope.kind ? scope.kind : 'recent';
  const visible = boards.filter((board) => {
    if (board.trashed) return kind === 'trash';
    if (kind === 'trash') return false;
    if (kind === 'starred') return board.starred;
    if (kind === 'space') return board.space === scope.name;
    return true;
  }).filter((board) => matchesSearch(board, search));

  if (kind === 'recent') {
    return visible.slice().sort((a, b) => b.modified - a.modified);
  }
  if (kind === 'trash') {
    // The trash's own question is "how long have I got", so it is ordered by when a board went
    // in. The desktop sorts it by title with everything else; this is a deliberate divergence
    // and the only one in this function, because the desktop's trash *card* already leads with
    // the deletion time and a list ordered by something it does not show is a list you scan.
    return visible.slice().sort((a, b) => (b.trashed || 0) - (a.trashed || 0));
  }
  return visible.slice().sort((a, b) => a.title.toLowerCase().localeCompare(b.title.toLowerCase()));
}

/**
 * How many cards fit across `available` pixels, at `gutter` between them.
 *
 * `library.rs:394`, unchanged, and the whole reason the first band is *one row* rather than a
 * fixed number of boards: *"the amount of boards on that row will depend on the width of the
 * viewport"*. A count chosen in advance is either short of the window on a wide one or wrapped
 * onto a second row on a narrow one, and a "row" that wraps is not a row.
 *
 * `n` cards need `n` widths and `n - 1` gutters, which is why the gutter is added to both sides
 * of the division rather than only to the card. Never zero: a window narrower than one card
 * still draws the card rather than drawing nothing and looking broken.
 *
 * ⚠ The CSS this pairs with is `display: flex; flex-wrap: wrap; gap: 16px` over fixed 236px
 * children, which wraps at exactly `n × 236 + (n − 1) × 16 ≤ available` — the same inequality.
 * A `grid-template-columns: repeat(auto-fill, …)` would wrap at a *different* count, and the
 * top band would then hold a number of cards that is not one row.
 */
export function columnsThatFit(available, gutter = CARD_GUTTER) {
  const width = Number(available);
  if (!Number.isFinite(width)) return 1;
  return Math.max(1, Math.floor((width + gutter) / (CARD_WIDTH + gutter)));
}

/**
 * The three bands the Recent page is laid out in, in the user's own description:
 *
 * > *"on the top there would be one row that is the most recent boards that i opened … and then
 * > the next row will be pinned boards if i have any pinned boards or folders, if there is none
 * > pinned it will be every other … by the regular recent boards"*
 *
 * A port of `recent_bands` (`library.rs:439`), pure for that function's own reason: which board
 * lands in which band is the part with rules in it, and none of those rules needs a DOM.
 *
 * **A starred board is in Pinned even when it is also in the top row**, and it is the one place
 * a board is deliberately drawn twice. The desktop was written the other way first, on the
 * reasoning that two cards for one board makes the page lie about how many boards there are;
 * the user overruled it — *"the boards that are starred, even if they are under recently
 * opened, if they are starred they should still be under pinned"* — and they are right about
 * what the section is for. Pinned is where you go to find the things you pinned. A starred
 * board silently missing from it because it happens to have been opened this morning makes the
 * section unreliable, and an unreliable section is worse than a repeated card: you stop looking
 * there.
 *
 * The tail is exclusive of both, so a board is in at most **two** bands and never three.
 */
export function recentBands(visible, columns) {
  const bands = { firstRow: [], pinned: [], rest: [] };
  visible.forEach((board, index) => {
    if (board.starred) bands.pinned.push(board);
    if (index < columns) bands.firstRow.push(board);
    else if (!board.starred) bands.rest.push(board);
  });
  return bands;
}

/**
 * All boards, broken into folders.
 *
 * `library.rs`'s `(Scope::All, Grouping::Folders, false)` arm, rule for rule:
 *
 * - one section per folder, **in the order the server supplied**, which is the order the user
 *   arranged them in and is not ours to re-sort;
 * - a folder with nothing visible in it is **skipped whole**, not drawn as an empty heading;
 * - the unfiled boards come last, and are headed *"Not in a folder"* **only when something
 *   above them was**. With no folders at all that label is on every board you own.
 */
export function groupedSections(visible, spaces) {
  const sections = [];
  let filed = 0;
  for (const space of spaces) {
    const held = visible.filter((board) => board.space === space.name);
    if (held.length === 0) continue;
    filed += held.length;
    sections.push({ name: space.name, boards: held });
  }
  const unfiled = visible.filter((board) => !board.space);
  return { sections, unfiled, headUnfiled: filed > 0 };
}

/**
 * The heading over the grid.
 *
 * `LibraryState::title()`. A folder keeps its own name as the heading; there is no *Settings*
 * here, because there is nothing on this page to set — see the file header.
 */
export function scopeTitle(scope) {
  switch (scope && scope.kind) {
    case 'all':
      return 'All boards';
    case 'starred':
      return 'Starred';
    case 'trash':
      return 'Recently deleted';
    case 'space':
      return scope.name;
    default:
      return 'Recent';
  }
}

/**
 * What a scope with nothing in it says.
 *
 * `LibraryState::nothing_here()`, verbatim. The trash's line is stated as a **promise** rather
 * than as an absence, which is that function's own note: this is the one screen where being
 * empty is good news, and where the question is not *"where are my boards"* but *"is the one I
 * deleted still here"*.
 */
export function nothingHere(scope, search = '') {
  if (String(search || '').trim()) return 'No boards match that search';
  switch (scope && scope.kind) {
    case 'starred':
      return 'No starred boards yet';
    case 'trash':
      return 'Nothing deleted — boards you delete wait here until you empty it';
    case 'space':
      return 'Nothing in this folder yet';
    default:
      return 'No boards here';
  }
}

/**
 * The `id` attribute a card carries, discriminated by the band it is drawn in.
 *
 * ⚠ **A board can be on this page twice**, on purpose — see [`recentBands`] — and two nodes for
 * one board must not collide. The desktop's hazard is egui's: two widgets sharing an `Id` hand
 * the interaction to whichever registered *last*, so the hover ring lights on one card while
 * the click lands on the other. A DOM's hazard is not the same shape but it is the same cause:
 * a duplicate `id` attribute makes `getElementById`, `aria-labelledby`, a `<label for>` and the
 * fragment in a URL all resolve to whichever came first, silently, and every one of those is a
 * pointer that now aims at a card the reader is not looking at.
 *
 * So the fix is the desktop's fix — put the band in the key — and the reason to keep doing it
 * is the desktop's reason: `card_id`'s doc once read *"a board appears once per frame, so there
 * is nothing to collide with"*, which was true when it was written and stopped being true two
 * sections later.
 */
export function cardDomId(band, id) {
  return `velm-card-${band}-${encodeURIComponent(id)}`;
}

/**
 * How many items, and when it changed if the server says.
 *
 * Used as the card's **tooltip**, where an exact date is worth having; the card's own line says
 * *"3 days ago"*, because `library.rs`'s metadata row does and because the question it answers
 * is which board this is rather than what the date was.
 *
 * `modified` is milliseconds since the epoch, formatted in the reader's own locale rather than
 * the server's. `boards_json` in `crates/velmd/src/serve.rs` sends it.
 *
 * A missing or zero value renders **no date at all** rather than 1970: a clock the file predates
 * is a fact about the filesystem, not about the board, and an obviously wrong date is worse than
 * none. That is why this degrades instead of falling back.
 *
 * ⚠ This doc said the field *was not sent* and told the next reader the branch was dead and
 * could be deleted — written against the server as it was that morning, and stale by the same
 * commit, which added it. A comment that instructs somebody to delete working code is the
 * `locked: false` trap pointed at the reader instead of at the compiler.
 */
function describeBoard(board) {
  const parts = [];
  const label = itemCountLabel(board?.items);
  if (label) parts.push(label);
  const modified = Number(board?.modified);
  if (Number.isFinite(modified) && modified > 0) {
    const when = new Date(modified);
    if (!Number.isNaN(when.getTime())) {
      parts.push(when.toLocaleDateString(undefined, { year: 'numeric', month: 'short', day: 'numeric' }));
    }
  }
  return parts.join(' · ');
}

// ─────────────────────────────────────────────────────────────────────────────
// The library's own paint
// ─────────────────────────────────────────────────────────────────────────────

const STYLE_ID = 'velm-library-style';

/**
 * Icons, drawn as a set on a 20 grid with a 1.5 stroke and square caps.
 *
 * The same construction `chrome.js` uses and for the same reason — `docs/05` §3's *drawn as a
 * set, not collected* — and the same source: each path is `crates/vellum-ui/src/icon.rs`'s own
 * primitive list for that `Icon`, scaled from its unit square to this grid.
 *
 * The mark is the exception and is not from `icon.rs`: four corner brackets around a frame that
 * is never drawn, `assets/logo/mark.svg` scaled from its 48 grid, which is what `paint_mono`
 * puts in a card with no picture.
 */
const ICON = {
  search: 'M8.8 8.8m-5.2 0a5.2 5.2 0 1 0 10.4 0a5.2 5.2 0 1 0 -10.4 0M12.6 12.6L16.8 16.8',
  folder: 'M16.7 16.7H3.3a1.7 1.7 0 0 1-1.7-1.7V4.2a1.7 1.7 0 0 1 1.7-1.7h3.3a1.7 1.7 0 0 1 1.4.8l.7 1a1.7 1.7 0 0 0 1.4.7h6.6a1.7 1.7 0 0 1 1.7 1.7V15a1.7 1.7 0 0 1-1.7 1.7z',
  grid: 'M6.8 2v16M13.2 2v16M2 6.8h16M2 13.2h16',
  list: 'M7.6 5.2h9.2M7.6 10h9.2M7.6 14.8h9.2M3.2 5.2h.1M3.2 10h.1M3.2 14.8h.1',
  pin: 'M10 3.2a3.6 3.6 0 1 1 0 7.2a3.6 3.6 0 0 1 0-7.2M6 10.4h8M10 10.4v6.8',
  star: 'M10 1.6L12 7.25L17.99 7.4L13.23 11.05L14.94 16.8L10 13.4L5.06 16.8L6.77 11.05L2.01 7.4L8 7.25Z',
  // Fit-to-board's own icon in `chrome.js`, which is the mark: a bounded view onto something
  // with no edges. Here it is a board that has no picture yet.
  mark: 'M3.5 8.5V3.5H8.5M11.5 3.5H16.5V8.5M16.5 11.5V16.5H11.5M8.5 16.5H3.5V11.5',
  frame: 'M3 3h14v14H3zM3 6.6h14',
};

/**
 * One icon, as an `<svg>`.
 *
 * ⚠ Built with `innerHTML` from a **module constant**, never from anything off the wire. That
 * is the same line `chrome.js` draws: its own SVG is markup it wrote, a board's title is a
 * string somebody else wrote, and the two never meet in the same call.
 */
function icon(doc, name, { filled = false, size = 20 } = {}) {
  const node = doc.createElementNS('http://www.w3.org/2000/svg', 'svg');
  node.setAttribute('viewBox', '0 0 20 20');
  node.setAttribute('width', String(size));
  node.setAttribute('height', String(size));
  node.setAttribute('aria-hidden', 'true');
  node.setAttribute('focusable', 'false');
  const path = doc.createElementNS('http://www.w3.org/2000/svg', 'path');
  path.setAttribute('d', ICON[name]);
  path.setAttribute('fill', filled ? 'currentColor' : 'none');
  path.setAttribute('stroke', 'currentColor');
  path.setAttribute('stroke-width', '1.5');
  path.setAttribute('stroke-linecap', 'square');
  path.setAttribute('stroke-linejoin', 'miter');
  node.append(path);
  return node;
}

/**
 * The library's stylesheet.
 *
 * ⚠ **Nothing here *defines* a custom property; every one is *read* with a fallback.** The page
 * already has `--ink`, `--line`, `--card` and the rest, and its import panel reads two —
 * `--edge` and `--raised` — that nothing defines, so it renders on their fallbacks. Defining
 * either of those on `:root` from here would silently restyle a panel this file does not own.
 * Reading with a fallback is the opposite: the library follows the page wherever the page has an
 * opinion, and falls back to `theme.rs`'s own value where it has none.
 *
 * Every fallback below is the Mac's token, named, from `crates/vellum-ui/src/theme.rs`. Where
 * the page's equivalent differs it differs by two or three of 255 — `--line` `#E2E7EA` against
 * `FROST` `#E5EAED` — and the page's wins, deliberately: this design draws its structure with a
 * hairline where others use a shadow, so **two nearly-identical hairlines on one screen read as
 * a mistake rather than as a system**, and one page with one hairline beats one screen that
 * matches a second application to the byte.
 *
 * ⚠ `[hidden] { display: none !important }`. An author rule such as `.btn { display: inline-flex }`
 * beats the user agent's `[hidden] { display: none }`, so every control this file hides by
 * setting `.hidden = true` stays on screen — and this page drives *every* visibility decision
 * that way. `web/find.js` and `web/tools.js` both carry this line for the same reason. It can
 * only ever hide something already marked hidden, so it cannot take a visible control away.
 *
 * The geometry is `library.rs`'s, and the two numbers worth stating are the ones a rewrite gets
 * wrong first:
 *
 * - **The card *is* its slot.** 236 × 176 including its border, an 8-point pad and a 1px
 *   hairline drawn *inside* — `Frame::paint`'s `StrokeKind::Inside`, which is `box-sizing:
 *   border-box`. The content box is 218 × 158 and the stack inside it is 108 + 4 + 19 + 4 + 20.
 * - **The internal leading is 4, and the 16 is the gutter *between* cards.** Feedback 22 spent a
 *   round separating those two: an inner `Ui` inherited the grid's 16 and every gap inside a
 *   card became 16 too, which is 24 points of each card spent on air.
 */
const CSS = `
[hidden] { display: none !important; }

.velm-lib { display: flex; align-items: flex-start; gap: 24px; }
.velm-lib-body { min-width: 0; flex: 1 1 auto; }

/* SIDEBAR_WIDTH, theme.rs:1025. A panel on a page rather than a window's edge, so it takes the
   card's own fill and hairline; the desktop's sits flush and needs neither. */
.velm-lib-side {
  flex: none;
  width: 192px;
  box-sizing: border-box;
  padding: 12px;
  border: 1px solid var(--line, #E5EAED);
  border-radius: 10px;
  background: var(--card, #FCFDFE);
}
.velm-lib-wordmark {
  display: flex; align-items: center; gap: 8px;
  margin: 0 0 12px; font-size: 15px; font-weight: 600; letter-spacing: -.01em;
  color: var(--ink, #1A1D1F);
}
.velm-lib-search {
  display: block; width: 100%; box-sizing: border-box;
  min-height: 36px; padding: 0 10px; margin: 0 0 8px;
  border: 1px solid var(--line, #E5EAED); border-radius: 6px;
  background: var(--wash, #F2F5F6); color: inherit;
  /* ⚠ 16px, not 13px. iOS zooms the whole page in when a field smaller than this takes focus,
     and it does not zoom back out — the same figure and the same reason as the connect form. */
  font-family: inherit; font-size: 16px; line-height: 1.4;
}
.velm-lib-search:focus-visible { outline: 2px solid var(--focus, #6FD6E6); outline-offset: -1px; }

/* space::of(6) = 24, library.rs's own row height for a scope and for a folder. */
.velm-lib-scope, .velm-lib-space {
  display: flex; align-items: center; gap: 8px;
  width: 100%; box-sizing: border-box; min-height: 24px; padding: 0 8px;
  border: 0; border-radius: 4px; background: none; color: var(--ink, #1A1D1F);
  font: inherit; font-size: 13px; text-align: left; cursor: pointer;
  -webkit-tap-highlight-color: transparent; touch-action: manipulation;
  transition: background-color 120ms ease-out;
}
/* ⚠ 24 points is a mouse's row and a finger cannot hit it. Every other control on this page is
   44px — Apple's own minimum, the figure \`chrome.js\` sizes to — so the row grows on a coarse
   pointer rather than everywhere: matching the desktop pixel for pixel on a tablet would be
   matching it into a control nobody can press. */
@media (pointer: coarse) {
  .velm-lib-scope, .velm-lib-space { min-height: 44px; }
}
@media (hover: hover) {
  .velm-lib-scope:hover, .velm-lib-space:hover { background: var(--hover, #DCEBEF); }
}
.velm-lib-scope[aria-current], .velm-lib-space[aria-current] {
  background: var(--accent-soft, #CAEBE7);
  color: var(--on-accent-soft, #0C675B);
}
.velm-lib-scope:focus-visible, .velm-lib-space:focus-visible,
.velm-lib-toggle:focus-visible { outline: 2px solid var(--focus, #6FD6E6); outline-offset: 1px; }
.velm-lib-count {
  margin-left: auto; flex: none;
  font: 12px/1 ui-monospace, SFMono-Regular, Menlo, monospace;
  font-variant-numeric: tabular-nums;
  color: var(--ink-faint, #8B959B);
}
.velm-lib-space .velm-lib-pin { flex: none; color: var(--ink-faint, #8B959B); }
.velm-lib-name { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.velm-lib-rule { height: 1px; margin: 8px 0; background: var(--line, #E5EAED); }

/* section_label, theme.rs:1045 — uppercased, 11pt, muted, letterspaced. */
.velm-lib-section, .velm-lib-caption {
  margin: 12px 0 4px;
  font-size: 11px; font-weight: 500; letter-spacing: .7px; text-transform: uppercase;
  color: var(--ink-muted, #5C656B);
}
.velm-lib-caption { margin: 0 0 4px; }
.velm-lib-side .velm-lib-note { margin: 8px 0 0; font-size: 11px; color: var(--ink-faint, #8B959B); }

.velm-lib-head { display: flex; align-items: center; gap: 12px; margin: 0 0 12px; min-height: 28px; }
.velm-lib-heading { margin: 0; font-size: 20px; font-weight: 600; letter-spacing: -.01em; }
.velm-lib-tools { margin-left: auto; display: flex; align-items: center; gap: 4px; }
.velm-lib-toggle {
  display: inline-flex; align-items: center; justify-content: center;
  width: 28px; height: 28px; padding: 0;
  border: 0; border-radius: 4px; background: none; color: var(--ink, #1A1D1F);
  cursor: pointer; -webkit-tap-highlight-color: transparent; touch-action: manipulation;
}
@media (pointer: coarse) { .velm-lib-toggle { width: 44px; height: 44px; } }
@media (hover: hover) { .velm-lib-toggle:hover { background: var(--hover, #DCEBEF); } }
.velm-lib-toggle[aria-pressed="true"] {
  background: var(--accent-soft, #CAEBE7); color: var(--on-accent-soft, #0C675B);
  box-shadow: inset 0 0 0 1px var(--accent, #00A38C);
}

/* ⚠ flex-wrap, not grid. It wraps at exactly n x 236 + (n-1) x 16 <= available, which is
   \`columnsThatFit\`'s own inequality; \`repeat(auto-fill, …)\` wraps at a different count and the
   top band would then hold a number of cards that is not one row. */
.velm-lib-grid { display: flex; flex-wrap: wrap; align-items: flex-start; gap: 16px; }

.velm-lib-card {
  box-sizing: border-box; width: 236px; height: 176px;
  display: flex; flex-direction: column; gap: 4px;
  padding: 8px; overflow: hidden;
  border: 1px solid var(--line, #E5EAED); border-radius: 5px;
  background: var(--card, #FCFDFE); color: inherit; text-decoration: none;
  transition: border-color 120ms ease-out;
  -webkit-tap-highlight-color: transparent; touch-action: manipulation;
}
/* Hover replaces the hairline in its own band rather than adding a ring outside it — the
   desktop draws it on the frame's own rect at the frame's own radius, \`StrokeKind::Inside\`. */
@media (hover: hover) {
  a.velm-lib-card:hover, button.velm-lib-card:hover { border-color: var(--info, #6FD6E6); }
}
a.velm-lib-card:focus-visible, button.velm-lib-card:focus-visible {
  outline: 2px solid var(--accent, #00A38C); outline-offset: 2px;
}
.velm-lib-card--flat { cursor: default; }

/* space::of(27) = 108. Filled \`canvas\` and not \`raised\`: this slot is a picture of a board, so
   the bars beside a board that does not fit must be the colour a board is. */
.velm-lib-well {
  position: relative; flex: none; height: 108px;
  display: flex; align-items: center; justify-content: center;
  border-radius: 4px; overflow: hidden;
  background: var(--well, #F2F2F2);
  box-shadow: inset 0 0 0 1px var(--line, #E5EAED);
  color: var(--line, #E5EAED);
}
/* ⚠ \`contain\`, and never upscaled — \`scale.min(1.0)\` in \`preview()\`. A board is any aspect
   ratio at all, and cropping a wide one to a card-shaped hole hides the part that identifies
   it. Intrinsic size with a cap rather than \`object-fit\`, because \`object-fit: contain\` on a
   stretched box would magnify a small thumbnail to fill it. */
.velm-lib-thumb { max-width: 100%; max-height: 100%; width: auto; height: auto; display: block; }
.velm-lib-mark { position: absolute; }

/* ROW_BUTTON = space::of(5) = 20, inset space::UNIT = 4 from the well's top right. Not a
   button: see the file header. */
.velm-lib-star {
  position: absolute; top: 4px; right: 4px;
  width: 20px; height: 20px;
  display: flex; align-items: center; justify-content: center;
  border-radius: 4px;
  background: var(--accent-soft, #CAEBE7);
  box-shadow: inset 0 0 0 1px var(--accent, #00A38C);
  color: var(--on-accent-soft, #0C675B);
}

/* ⚠ Regular weight, and that is not an oversight. \`library.rs\` asks for \`.strong()\` and egui
   0.35's \`strong\` is consulted only by \`get_text_color\`, which checks an explicit \`.color()\`
   first — so the call is inert here, and no bold face is installed in the desktop chrome
   anyway. A 600 here would not match the application this is a port of. */
.velm-lib-title {
  font-size: 13px; line-height: 19px; font-weight: 400;
  color: var(--ink, #1A1D1F);
  overflow: hidden; text-overflow: ellipsis; white-space: nowrap;
}
.velm-lib-meta {
  display: flex; align-items: center; gap: 4px; min-width: 0;
  height: 20px; font-size: 11px; color: var(--ink-faint, #8B959B);
}
.velm-lib-meta .velm-lib-items {
  flex: none;
  font: 12px/1 ui-monospace, SFMono-Regular, Menlo, monospace;
  font-variant-numeric: tabular-nums;
}
.velm-lib-when { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
/* Laid out from the **opposite edge**, so the count and the date keep the same column on every
   card whether or not there is a folder to name. Icon then name, which is the reading order the
   desktop's \`right_to_left\` sub-layout produces. */
.velm-lib-folder {
  margin-left: auto; flex: 0 1 auto; min-width: 0;
  display: flex; align-items: center; gap: 4px;
  color: var(--ink-muted, #5C656B);
}
.velm-lib-folder svg { flex: none; color: var(--ink-faint, #8B959B); }

/* A pinned folder, at a card's own size — the size is the point: it shares the Pinned row with
   starred boards, and a tile of another height would break the row the band exists to be. */
.velm-lib-tile .velm-lib-well { background: var(--raised, #EBEEF0); color: var(--ink-muted, #5C656B); }

.velm-lib-list { display: flex; flex-direction: column; }
.velm-lib-row {
  display: flex; align-items: center; gap: 12px;
  min-height: 32px; padding: 0 8px;
  border-bottom: 1px solid var(--line, #E5EAED);
  color: inherit; text-decoration: none; font-size: 13px;
  -webkit-tap-highlight-color: transparent; touch-action: manipulation;
}
@media (pointer: coarse) { .velm-lib-row { min-height: 44px; } }
@media (hover: hover) { a.velm-lib-row:hover { background: var(--hover, #DCEBEF); } }
.velm-lib-row .velm-lib-title { flex: 1 1 auto; }
.velm-lib-row .velm-lib-items, .velm-lib-row .velm-lib-when { flex: none; color: var(--ink-faint, #8B959B); }
.velm-lib-row .velm-lib-folder { margin-left: 0; flex: none; width: 96px; }
.velm-lib-row .velm-lib-star {
  position: static; width: 16px; height: 16px; background: none; box-shadow: none;
  color: var(--accent, #00A38C);
}
.velm-lib-row-icon { flex: none; color: var(--ink-faint, #8B959B); }

.velm-lib-empty { margin: 24px 0 0; color: var(--ink-muted, #5C656B); font-size: 13px; }
.velm-lib-note { margin: 4px 0 12px; color: var(--ink-muted, #5C656B); font-size: 12px; max-width: 68ch; }

@media (max-width: 720px) {
  .velm-lib { display: block; }
  .velm-lib-side { width: auto; margin: 0 0 16px; }
}
@media (prefers-reduced-motion: reduce) {
  .velm-lib-card, .velm-lib-scope, .velm-lib-space { transition: none; }
}
`;

function ensureStyle(doc) {
  if (doc.getElementById(STYLE_ID)) return;
  const style = doc.createElement('style');
  style.id = STYLE_ID;
  style.textContent = CSS;
  (doc.head ?? doc.documentElement).append(style);
}

// ─────────────────────────────────────────────────────────────────────────────
// The page
// ─────────────────────────────────────────────────────────────────────────────

/**
 * The nodes `boards.html` must carry.
 *
 * ⚠ Missing one throws, loudly, before a single listener is attached. A renamed id would
 * otherwise make one control silently do nothing, which is this repository's signature defect —
 * code that compiles, passes its tests and is not reachable.
 */
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
  importOpen: 'velm-import-open',
  importPanel: 'velm-import',
  importDrop: 'velm-import-drop',
  importName: 'velm-import-name',
  importGo: 'velm-import-go',
  importSaid: 'velm-import-said',
  where: 'velm-where',
  change: 'velm-change',
  forget: 'velm-forget',
  retry: 'velm-retry',
};

/**
 * Nodes the library would like and can do without.
 *
 * ⚠ **These are not in [`IDS`], and the difference is deliberate.** Every id above is one the
 * markup on disk already carries, so a missing one is a mistake and throwing is right. These
 * three are *new* — the two-column layout the desktop library has and this page did not — and
 * `boards.html` is owned by somebody else. Throwing on a coordination gap would blank the whole
 * page over a sidebar, which is a far worse failure than the one it would be reporting. So the
 * library builds what it cannot find, and says on the console that it did: the page works either
 * way and the mismatch is still visible to whoever is looking.
 *
 * - `velm-sidebar` — the scopes and the folders. Built and inserted before `#velm-boards`.
 * - `velm-title`   — the heading that names the scope. Driven if present; drawn in the
 *                    library's own header row if not.
 */
const OPTIONAL_IDS = {
  sidebar: 'velm-sidebar',
  title: 'velm-title',
  // `boards.html` ships the filter hidden, because a search field that answers nothing is the
  // one control on the page whose failure is silent. Unhiding it is this file saying it answers.
  search: 'velm-search',
  searchField: 'velm-search-field',
  // Optional, like the search field, and for the same reason: `boards.html` ships it visible
  // and this file hides it against a server that has no create route. A button whose only
  // outcome is a 405 is the described-gesture failure with an HTTP status attached.
  newBoard: 'velm-new-board',
};

/**
 * Draw the picker.
 *
 * Everything it needs from the outside is an argument, so a harness can drive it; nothing runs
 * until it is called, so importing this module is free.
 */
/**
 * Read a JSON body under its own deadline.
 *
 * ⚠ **`request`'s timeout does not cover this, and its own comment said it did.** `fetch`
 * resolves when the *headers* arrive; `request` clears the abort timer in a `finally` at that
 * moment, so every `await response.json()` after it ran with a live signal that could never
 * fire. A captive portal, a buffering proxy, wifi dropping mid-response, or velmd killed after
 * it writes the head all leave the page on *"Asking…"* for ever — with the retry button still
 * hidden, on a tablet with no console. That is the exact class `docs/08-web.md` §6 records
 * three times, standing under the line that promises it cannot happen.
 *
 * `null` on anything that is not a JSON object, so every caller's existing "answered something
 * this page could not read" branch keeps working unchanged.
 */
async function readJson(response, timeoutMs, win) {
  let timer = null;
  try {
    return await Promise.race([
      response.json(),
      new Promise((_, reject) => {
        timer = win.setTimeout(() => reject(new Error('body timed out')), timeoutMs);
      }),
    ]);
  } catch {
    return null;
  } finally {
    if (timer !== null) win.clearTimeout(timer);
  }
}

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
  for (const [name, id] of Object.entries(OPTIONAL_IDS)) {
    el[name] = doc.getElementById(id) ?? null;
  }

  ensureStyle(doc);
  layOutTheLibrary();

  const params = new URLSearchParams(win.location.search);
  let server = resolveServer(win, store, params.get('server'));
  const storedToken = store.getToken(server.base) || null;
  let token = params.get('token') || storedToken || null;
  wireImport();
  wireNewBoard();

  // ⚠ A token that arrived in the URL is taken out of the address bar, and is stored **only
  // when there is nothing to overwrite**.
  //
  // Scrubbing is the easy half: `chrome.js`'s Back button delivers the passphrase that way on
  // every return from a board, so without it the picker's URL carries the secret for the life
  // of the tab — on the borrowed machine this page is written for, that is the one thing worth
  // not leaving on screen. The cost is stated rather than hidden: a new tab has no session
  // storage and asks again.
  //
  // Storing it unconditionally was the wrong half, and the case that hurts is not the crafted
  // link — it is the ordinary one. Rotate the server's token, type the new passphrase here
  // with *Remember* ticked, then press Back in a board tab you opened before the rotation:
  // that tab's URL still carries the **old** token, and it was written straight over the new
  // one. Every visit afterwards 401s for a passphrase already entered correctly, with nothing
  // on screen to say why.
  //
  // It also ran *before* `needsServerConfirmation` had decided whether the `?server=` in the
  // same link may be used at all — so the gate that exists to make a stranger's link safe did
  // not cover this write.
  //
  // A URL token still works for this visit; it simply does not replace something the user
  // typed. `connect` below is the only path that overwrites, and that one is a person typing.
  if (params.get('token')) {
    if (!storedToken) {
      store.setToken(server.base, token, store.isRemembered(server.base));
    }
    scrubToken(win);
  }

  /** Everything on the wire, once a server has answered. */
  let library = { boards: [], spaces: [], legacy: true };
  /** What the reader is looking at. In memory only: it is a view, not a preference. */
  const state = { scope: { kind: 'recent' }, grouping: 'folders', layout: 'grid', search: '' };
  let columns = 1;
  /** Whether the filter's `input` listener is attached. It is attached exactly once. */
  let searchWired = false;
  // ⚠ Whether a server has answered and the library is on screen. The resize listener and
  // nothing else reads it: without it a window dragged wider while the page is showing *"Could
  // not reach that server"* would re-band an empty library over the top of the failure, and the
  // reader would watch their error message be replaced by "No boards yet".
  let drawn = false;

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

  // ⚠ The column count follows the window, so the top band has to be re-banded when it changes
  // — and **only** then. A redraw per resize event rebuilds every card sixty times a second
  // while a window is being dragged; comparing the count first means a resize that does not
  // cross a card boundary costs one integer division.
  win.addEventListener('resize', () => {
    if (!drawn) return;
    if (columnsNow() === columns) return;
    drawBoards();
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

  // ── the two columns ───────────────────────────────────────────────────────

  /**
   * Put a sidebar beside the board list.
   *
   * Idempotent, and it does as little as it can get away with: if `boards.html` already supplies
   * `#velm-sidebar` the markup is taken as authoritative and nothing is moved. Otherwise the nav
   * is built and `#velm-boards` is moved into a flex row beside it — which is a structural edit
   * to a file this one does not own, so it is confined to this function and announced.
   *
   * ⚠ `#velm-boards` loses its `grid` class either way. It used to *be* the grid; it is a column
   * of sections now, each of which is its own grid, and leaving `display: grid` on the container
   * would lay every section heading out as a grid cell.
   */
  function layOutTheLibrary() {
    el.boards.classList.remove('grid');
    el.boards.classList.add('velm-lib-body');
    if (el.sidebar) return;
    if (win.console && typeof win.console.info === 'function') {
      win.console.info('velm: boards.html has no #velm-sidebar, so the library built its own.');
    }
    const nav = doc.createElement('nav');
    nav.id = OPTIONAL_IDS.sidebar;
    nav.setAttribute('aria-label', 'Board library');
    const row = doc.createElement('div');
    row.className = 'velm-lib';
    el.boards.replaceWith(row);
    row.append(nav, el.boards);
    el.sidebar = nav;
    el.sidebar.className = 'velm-lib-side';
  }

  function columnsNow() {
    // `clientWidth` is 0 in a detached container and in a harness; `columnsThatFit` floors at
    // one, which is the desktop's own answer for a window narrower than a card.
    return columnsThatFit(el.boards.clientWidth || 0);
  }

  // ── the request ───────────────────────────────────────────────────────────

  async function connect() {
    hide(el.message);
    hide(el.warning);
    hide(el.connect);
    drawn = false;
    el.boards.replaceChildren();
    if (el.sidebar) {
      // ⚠ **The body, not the nav.** This used to be `el.sidebar.replaceChildren()`, and it ran
      // *before* `drawSidebar` ever got to keep anything — so `boards.html`'s sticky wrapper,
      // its wordmark and its search field were all gone by the time the one function that
      // decides what survives was asked. Two wipes, one of them fixed: the sibling is where
      // this repository's fixes go wrong, and it went wrong here.
      sidebarBody();
      el.sidebar.hidden = true;
    }
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
    // places it has to be a query parameter are the board link, which is a navigation, and a
    // preview picture, which is an `<img>` — see `thumbnailUrl`.
    const headers = token ? { authorization: `Bearer ${token}` } : undefined;
    let list = await request(apiUrl(server.base, 'library'), { cache: 'no-store', headers }, BOARDS_TIMEOUT_MS);

    // ⚠ **404 on `/api/v1/library` means an older server, and it cannot mean anything else.**
    // A velmd without that route falls through to `static_file`, which answers 404 — while the
    // token gate runs *before* routing, so a missing or wrong passphrase is a 401 and can never
    // arrive here wearing an old server's clothes. `GET /api/v1/boards` is unchanged and every
    // version has it, so the retry costs one round trip on an old server and nothing on a new
    // one. What it buys is that an old server draws a **plainer** page — one flat grid, no
    // stars, no folders, no trash — rather than a broken one.
    if (!list.error && list.response.status === 404) {
      list = await request(apiUrl(server.base, 'boards'), { cache: 'no-store', headers }, BOARDS_TIMEOUT_MS);
    }

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
      payload = await readJson(list.response, BOARDS_TIMEOUT_MS, win);
    } catch {
      payload = null;
    }
    // A 200 carrying something that is neither shape — a captive portal's login page, a proxy's
    // error shape — passes every check above and then throws out of the render, leaving a blank
    // page. `index.html` learned this the same way. `normalizeLibrary` answers `null` for both
    // an array that is not one and an object with no `boards` in it.
    const parsed = normalizeLibrary(payload);
    if (!parsed) {
      return failed({ ...context, phase: 'boards', reason: 'not-velmd' });
    }

    library = parsed;
    // A scope named in a link or left over from another server may not exist here. The desktop
    // keeps a deleted folder's heading, because a heading that changed the instant a folder was
    // deleted would read as a rendering fault; there is no delete on this page, so a folder that
    // is not in the list means a *different server*, and falling back to Recent is the honest
    // answer rather than an empty page titled with somebody else's folder.
    if (state.scope.kind === 'space' && !library.spaces.some((space) => space.name === state.scope.name)) {
      state.scope = { kind: 'recent' };
    }
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
      const body = await readJson(response, HEALTH_TIMEOUT_MS, win);
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
    const held = library.boards.filter((board) => !board.trashed).length;
    el.sub.textContent =
      held === 0
        ? 'This server is not holding any boards yet.'
        : `${held} ${held === 1 ? 'board' : 'boards'}, most recently changed first.`;
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

    drawLibrary();

    // The importer is offered only once a server has actually answered — it is the only verb
    // on this page that writes, and offering it beside a failed connection would be a button
    // whose one outcome is an error.
    if (el.importOpen) el.importOpen.hidden = false;
    if (el.newBoard) el.newBoard.hidden = false;

    if (!server.local) checkRemoteClient();
  }

  /** Everything the library draws, from `library` and `state`. */
  function drawLibrary() {
    drawSidebar();
    drawBoards();
  }

  /**
   * The board column alone.
   *
   * Separate from the sidebar because typing filters the grid and changes nothing in the
   * sidebar: the counts there are totals, deliberately, so that a number never disagrees with
   * the page it labels. A keystroke therefore has no reason to rebuild four scope rows, a
   * folder list and their `aria-current` bookkeeping.
   *
   * ⚠ **This is cheapness, not the protection**, and the distinction is worth keeping straight
   * because the first version of this comment claimed otherwise. What actually stops the
   * `<input>` being typed into from being destroyed and recreated — which on a tablet dismisses
   * the software keyboard, and for an IME throws away the node mid-composition — is
   * `sidebarBody`'s keep rule, which holds even when the whole library is redrawn. Measured:
   * with this handler calling `drawLibrary`, the field is still the same node. A comment that
   * credits the wrong line for a guarantee is how the next person removes the right one.
   */
  function drawBoards() {
    columns = columnsNow();
    drawn = true;
    el.boards.replaceChildren();
    el.boards.hidden = false;
    el.boards.append(libraryHead());

    const visible = boardsInScope(library.boards, state.scope, state.search);

    if (library.boards.length === 0) {
      // The whole library is empty, which is a different sentence from a scope that filtered to
      // nothing. `library.rs`'s `empty_state` puts the import steps here; this page already has
      // them, under a button that `render` has just revealed.
      el.boards.append(noteLine('velm-lib-empty', 'No boards yet. Start one in Velm, or bring a Miro board across with the button below.'));
      return;
    }

    if (state.scope.kind === 'trash') {
      // ⚠ Says where the verb lives rather than offering one that is not here. Restore and
      // *Delete permanently* are both desktop-only, and this is the one scope where leaving
      // them out silently would be confusing — a page of boards you deleted with nothing to do
      // about them reads as broken until somebody says so.
      el.boards.append(noteLine('velm-lib-note', 'Nothing here has been removed — these boards are still on your server. Restoring one, and emptying this for good, are done in the Velm app.'));
    }

    if (visible.length === 0) {
      el.boards.append(noteLine('velm-lib-empty', nothingHere(state.scope, state.search)));
      return;
    }

    if (state.layout === 'list') {
      el.boards.append(listOf(visible));
      return;
    }

    const searching = Boolean(state.search.trim());

    // **The Recent page's three bands.** Not while searching: a search is a question about every
    // board at once, and slicing the answer into "recent", "pinned" and "the rest" hides matches
    // under headings the query had nothing to do with.
    if (state.scope.kind === 'recent' && !searching) {
      const bands = recentBands(visible, columns);
      const folders = library.spaces.filter((space) => space.pinned);

      el.boards.append(sectionHeader('Recently opened'), gridOf(bands.firstRow, 'recent'));

      // Omitted whole when there is nothing pinned, rather than drawn empty: a heading with
      // nothing under it reads as something that failed to load.
      if (folders.length > 0 || bands.pinned.length > 0) {
        const grid = doc.createElement('div');
        grid.className = 'velm-lib-grid';
        // Folders first, then the starred boards. A folder is the bigger thing and holds the
        // boards; putting it after them would read as an afterthought.
        for (const space of folders) grid.append(folderTile(space));
        for (const board of bands.pinned) grid.append(card(board, 'pinned'));
        el.boards.append(sectionHeader('Pinned'), grid);
      }

      if (bands.rest.length > 0) {
        el.boards.append(sectionHeader('All boards'), gridOf(bands.rest, 'boards'));
      }
      return;
    }

    // **All boards, grouped.** One section per folder in the server's own order, then the
    // unfiled boards under a heading of their own — so a board with no folder is somewhere
    // rather than nowhere.
    if (state.scope.kind === 'all' && state.grouping === 'folders' && !searching) {
      const grouped = groupedSections(visible, library.spaces);
      for (const section of grouped.sections) {
        el.boards.append(sectionHeader(section.name), gridOf(section.boards, 'boards'));
      }
      if (grouped.unfiled.length > 0) {
        if (grouped.headUnfiled) el.boards.append(sectionHeader('Not in a folder'));
        el.boards.append(gridOf(grouped.unfiled, 'boards'));
      }
      return;
    }

    el.boards.append(gridOf(visible, 'boards'));
  }

  /** The heading, and the controls that change how the grid is arranged. */
  function libraryHead() {
    const head = doc.createElement('div');
    head.className = 'velm-lib-head';

    const title = scopeTitle(state.scope);
    if (el.title) {
      el.title.textContent = title;
    } else {
      const heading = doc.createElement('h2');
      heading.className = 'velm-lib-heading';
      heading.textContent = title;
      head.append(heading);
    }

    const tools = doc.createElement('div');
    tools.className = 'velm-lib-tools';

    // Only on All boards, and that is the point rather than a shortcut: every other scope either
    // has its own arrangement or *is* one folder, so a grouping control there would be a switch
    // that changes nothing — which is worse than no switch, because it invites the question of
    // why it did not work. It is also out while the server has no folders to group by.
    if (state.scope.kind === 'all' && library.spaces.length > 0) {
      tools.append(
        toggle('grid', 'Every board in one grid', state.grouping === 'flat', () => {
          state.grouping = 'flat';
          drawLibrary();
        }),
        toggle('folder', 'Group by folder', state.grouping === 'folders', () => {
          state.grouping = 'folders';
          drawLibrary();
        }),
      );
      const gap = doc.createElement('span');
      gap.style.width = '8px';
      tools.append(gap);
    }

    tools.append(
      toggle('list', 'List', state.layout === 'list', () => {
        state.layout = 'list';
        drawLibrary();
      }),
      toggle('grid', 'Grid', state.layout === 'grid', () => {
        state.layout = 'grid';
        drawLibrary();
      }),
    );

    // ⚠ **Into the page header, ahead of the buttons — not into this section's own head.**
    // The Mac puts the view control, Import from Miro and New board on one line to the right
    // of the scope's name, and a second row of controls under the first is the difference a
    // person notices before they notice anything else. Rebuilt each draw rather than kept and
    // mutated, because which toggles exist depends on the scope: the grouping pair is on All
    // boards alone, so a persistent node would have to be emptied and refilled anyway.
    const actions = doc.querySelector('.velm-head-actions');
    if (actions) {
      const stale = actions.querySelector('.velm-lib-tools');
      if (stale) stale.remove();
      actions.prepend(tools);
    } else {
      head.append(tools);
    }
    return head;
  }

  function toggle(name, hint, pressed, run) {
    const button = doc.createElement('button');
    button.type = 'button';
    button.className = 'velm-lib-toggle';
    button.title = hint;
    button.setAttribute('aria-label', hint);
    button.setAttribute('aria-pressed', pressed ? 'true' : 'false');
    button.append(icon(doc, name));
    button.addEventListener('click', run);
    return button;
  }

  /**
   * The sidebar: a search box, the four standing scopes, and the folders.
   *
   * ⚠ What is **not** here, and why. The desktop's sidebar carries a `+` that makes a folder, a
   * pin toggle and a `⋮` on every folder row offering *Rename…*, *Pin to top* and *Delete
   * folder…*, and a *Settings* row pinned to the panel floor. `velmd` has no route for any of
   * them and this page has no settings to hold, so none is drawn. A row of controls that all
   * answer *"not here"* is six broken promises in a column; the one line at the foot says the
   * same thing once, and says where the verbs actually are.
   */
  function drawSidebar() {
    if (!el.sidebar) return;
    const host = sidebarBody();
    el.sidebar.hidden = false;
    wireSearch();

    // Deleted boards are not counted anywhere but in the trash. A "Recent 6" that includes two
    // boards you deleted is a number that disagrees with the page it labels, which is how a
    // count stops being read at all.
    const total = library.boards.filter((board) => !board.trashed).length;
    const starred = library.boards.filter((board) => board.starred && !board.trashed).length;
    const deleted = library.boards.filter((board) => board.trashed).length;

    host.append(scopeRow({ kind: 'recent' }, 'Recent', total));
    // *All boards* always shows everything, whatever is filed where: it is the scope a user
    // falls back to when they cannot remember where they put something.
    host.append(scopeRow({ kind: 'all' }, 'All boards', total));

    // ⚠ An older server sends a flat array with no stars, no folders and no trash, so three of
    // the four scopes would be permanently empty and the folder list permanently absent. They
    // are left out rather than drawn as three empty pages: `normalizeLibrary`'s `legacy` is the
    // one flag that says which page this is.
    if (!library.legacy) {
      host.append(scopeRow({ kind: 'starred' }, 'Starred', starred));
      // Last of the standing scopes, and **always shown** rather than appearing when something
      // is in it: a trash you can only find once you have lost something is one nobody knows
      // exists at the moment they need it.
      host.append(scopeRow({ kind: 'trash' }, 'Recently deleted', deleted));

      const rule = doc.createElement('div');
      rule.className = 'velm-lib-rule';
      host.append(rule, caption('Folders'));

      // Pinned first, then the rest in the order the server supplied — which is the order the
      // user arranged them in, and is not ours to re-sort. A stable sort on a boolean, so both
      // halves keep their relative order.
      const order = library.spaces
        .map((space, index) => ({ space, index }))
        .sort((a, b) => Number(b.space.pinned) - Number(a.space.pinned) || a.index - b.index);
      for (const { space } of order) host.append(spaceRow(space));
      if (library.spaces.length === 0) host.append(noteLine('velm-lib-note', 'No folders yet'));
    }

    host.append(
      noteLine(
        'velm-lib-note',
        library.legacy
          ? 'This server is an older velmd, so it cannot say which boards are starred or filed. Updating it fills in the rest of this list.'
          : 'Starring a board, filing it in a folder and restoring one are done in the Velm app. This page reads them.',
      ),
    );
  }

  /**
   * The part of the sidebar this file rebuilds, emptied and ready for rows.
   *
   * ⚠ **`replaceChildren` on the whole nav was wrong, and `boards.html` is what showed it.**
   * That file wraps its sidebar in a sticky `.velm-sidebar-inner` and leads with the Velm mark
   * and the wordmark — `library.rs`'s own row 1 — and a blanket wipe took both, so the sidebar
   * lost its mark and stopped sticking while nothing failed and no test could see it. Two nodes
   * are therefore kept if they are there, and only those two:
   *
   * - **the wordmark**, because it is a row of the library rather than decoration, and
   * - **the search field**, because it is the one node in here that must survive a redraw: it
   *   is the node being typed into. Keeping it outside the region this function empties is what
   *   makes that structural rather than a focus-restoring dance (see `drawBoards`).
   *
   * A page that supplies neither still gets both — the mark is drawn here and the field by
   * `wireSearch` — so this file does not depend on markup it does not own.
   */
  function sidebarBody() {
    const host = el.sidebar.querySelector('.velm-sidebar-inner') ?? el.sidebar;
    // ⚠ **One predicate, read twice.** The keep loop and the is-there-one-already test below
    // used to spell this differently — one checked both class names and the other only
    // `boards.html`'s — so on a page that brings no wordmark this file drew its own, kept it on
    // the next pass, failed to recognise it, and drew a second. Two tests of the same question
    // that can disagree is the bug; a named function is the fix.
    const isWordmark = (node) =>
      Boolean(node.classList) &&
      (node.classList.contains('velm-wordmark') || node.classList.contains('velm-lib-wordmark'));
    const keep = [];
    for (const child of Array.from(host.children ?? [])) {
      if (isWordmark(child)) keep.push(child);
      // By **id**, which is why the field this file builds for itself is given one: it has to be
      // the same node to the rule that keeps it, or it is destroyed on the first redraw while
      // `el.searchField` goes on pointing at the orphan and the filter silently disappears.
      else if (child.id === OPTIONAL_IDS.search || child.id === OPTIONAL_IDS.searchField) keep.push(child);
    }
    host.replaceChildren(...keep);
    // ⚠ Not `host.prepend?.(mark) ?? host.append(mark)`. `prepend` returns `undefined` on every
    // real DOM there is, so `??` fires on success and the wordmark is drawn **twice** — a
    // nullish coalesce reads as a fallback and is a sequence whenever the left side returns
    // nothing, which is most of the DOM.
    if (!keep.some(isWordmark)) {
      const mark = wordmark();
      if (typeof host.prepend === 'function') host.prepend(mark);
      else host.append(mark);
    }
    return host;
  }

  /** The mark and the word, when the page did not bring its own. */
  function wordmark() {
    const line = doc.createElement('p');
    line.className = 'velm-lib-wordmark';
    line.append(icon(doc, 'mark', { size: 18 }));
    const word = doc.createElement('span');
    word.textContent = 'velm';
    line.append(word);
    return line;
  }

  /**
   * The filter, wired once.
   *
   * `boards.html` ships the field hidden — *"a search field that answers nothing is the one
   * control on the page whose failure is silent"* — so unhiding it **is** the statement that it
   * now answers. Its own field is used where it exists rather than a second one being built
   * beside it: it is already styled, already labelled, and already outside the region
   * `sidebarBody` empties, so it survives every redraw with its caret and an open software
   * keyboard intact.
   */
  function wireSearch() {
    if (searchWired) return;
    if (!el.searchField) {
      const field = doc.createElement('input');
      field.type = 'search';
      field.className = 'velm-lib-search';
      // The id `boards.html` would have given it, so that everything downstream — `sidebarBody`'s
      // keep rule most of all — cannot tell this field from the page's own.
      field.id = OPTIONAL_IDS.searchField;
      field.placeholder = 'Search boards';
      field.setAttribute('aria-label', 'Search boards');
      field.autocomplete = 'off';
      field.spellcheck = false;
      const host = el.sidebar.querySelector('.velm-sidebar-inner') ?? el.sidebar;
      host.append(field);
      el.searchField = field;
    }
    if (el.search) el.search.hidden = false;
    el.searchField.value = state.search;
    // A closure flag rather than an expando on the node: the element belongs to `boards.html`,
    // and a property hung on somebody else's node is a contract nobody wrote down.
    searchWired = true;
    el.searchField.addEventListener('input', () => {
      state.search = el.searchField.value;
      // `drawBoards` rather than `drawLibrary`: nothing in the sidebar answers to the filter,
      // so a keystroke has no rows to rebuild. The field itself is safe either way — see
      // `drawBoards`, and `sidebarBody`, which is the line that actually keeps it.
      drawBoards();
    });
  }

  function scopeRow(scope, name, count) {
    const button = doc.createElement('button');
    button.type = 'button';
    button.className = 'velm-lib-scope';
    const current = state.scope.kind === scope.kind;
    // ⚠ `page`, not `true`. Both are valid ARIA and only one is the right word for a row in a
    // set of navigation links — and it is the spelling `boards.html`'s own fallback stylesheet
    // selects on, so a build where this file's injected sheet never arrives still marks the row
    // somebody is looking at. The rule below matches both spellings for the same reason.
    if (current) button.setAttribute('aria-current', 'page');
    const label = doc.createElement('span');
    label.className = 'velm-lib-name';
    label.textContent = name;
    const tally = doc.createElement('span');
    tally.className = 'velm-lib-count';
    tally.textContent = String(count);
    button.append(label, tally);
    button.addEventListener('click', () => {
      state.scope = scope;
      drawLibrary();
    });
    return button;
  }

  function spaceRow(space) {
    const button = doc.createElement('button');
    button.type = 'button';
    button.className = 'velm-lib-space';
    const current = state.scope.kind === 'space' && state.scope.name === space.name;
    if (current) button.setAttribute('aria-current', 'page');
    if (space.pinned) {
      // A pin rather than a dot: `docs/04-ui-reference.md` §5 shows pinned folders marked, and a
      // mark that looks like punctuation reads as a typo. Read-only here, like the star.
      const pin = icon(doc, 'pin', { size: 12 });
      pin.classList.add('velm-lib-pin');
      button.append(pin);
      button.title = `${space.name} — pinned`;
    }
    const label = doc.createElement('span');
    label.className = 'velm-lib-name';
    // ⚠ `textContent`, never `innerHTML`. A folder's name arrives over the wire and is a string
    // somebody else wrote, exactly as a board's title is.
    label.textContent = space.name;
    const tally = doc.createElement('span');
    tally.className = 'velm-lib-count';
    tally.textContent = String(space.boards);
    button.append(label, tally);
    button.addEventListener('click', () => {
      state.scope = { kind: 'space', name: space.name };
      drawLibrary();
    });
    return button;
  }

  // ── the cards ─────────────────────────────────────────────────────────────

  function gridOf(boards, band) {
    const grid = doc.createElement('div');
    grid.className = 'velm-lib-grid';
    for (const board of boards) grid.append(card(board, band));
    return grid;
  }

  /**
   * One board, at `library.rs`'s own size.
   *
   * ⚠ **An `<a href>`, not a `<div>` with a click handler** — the same reasoning `chrome.js`
   * records over the Back button. An anchor is what makes ⌘-click open a second board, what
   * makes middle-click work, and what puts the destination in the corner of the window before
   * the press. A trashed board is the exception and is deliberately **not** a link: the desktop
   * restores on that click and this page cannot, and opening a board somebody deleted would
   * quietly start syncing edits into it.
   */
  function card(board, band) {
    const trashed = Boolean(board.trashed);
    const node = doc.createElement(trashed ? 'div' : 'a');
    node.className = trashed ? 'velm-lib-card velm-lib-card--flat' : 'velm-lib-card';
    node.id = cardDomId(band, board.id);
    if (!trashed) {
      node.href = buildViewerUrl(server, board.id, token, win.location.href);
    }
    const exact = describeBoard(board);
    if (exact) node.title = `${board.title} — ${exact}`;

    node.append(well(board), titleLine(board.title), metaRow(board));
    return node;
  }

  /** The picture, or the mark where there is none. */
  function well(board) {
    const box = doc.createElement('div');
    box.className = 'velm-lib-well';

    // `paint_mono` in the desktop: the Velm mark at space::of(7) = 28, monochrome, in the
    // border colour. It is under the picture rather than instead of it, so a thumbnail that
    // fails to decode leaves the placeholder it was drawn over.
    const mark = icon(doc, 'mark', { size: 28 });
    mark.classList.add('velm-lib-mark');
    box.append(mark);

    const src = thumbnailUrl(server, board, token);
    if (src) {
      const img = doc.createElement('img');
      img.className = 'velm-lib-thumb';
      img.alt = '';
      img.loading = 'lazy';
      img.decoding = 'async';
      // ⚠ A blob that is not there answers 404, and an `<img>` with a 404 draws a broken-image
      // glyph in most browsers — which is worse than no picture, because it reads as a fault
      // rather than as an absence. Removing it on error puts the mark back.
      img.addEventListener('error', () => img.remove());
      img.addEventListener('load', () => mark.remove());
      img.src = src;
      box.append(img);
    }

    if (board.starred) {
      // ⚠ **Shown, not settable.** There is no route that stars a board, so this is a `<span>`
      // and not a button: it takes no focus, answers no click and says what it means on hover.
      // The desktop's own note applies unchanged — the filled star is the state, not the
      // colour, because `docs/05` §6 requires the accent never to be the only carrier of
      // meaning.
      const star = doc.createElement('span');
      star.className = 'velm-lib-star';
      star.title = 'Starred in the Velm app';
      star.setAttribute('role', 'img');
      star.setAttribute('aria-label', 'Starred');
      star.append(icon(doc, 'star', { filled: true, size: 11 }));
      box.append(star);
    }
    return box;
  }

  function titleLine(text) {
    const node = doc.createElement('div');
    node.className = 'velm-lib-title';
    // ⚠ `textContent`, never `innerHTML`. This is a string somebody else wrote, arriving over
    // the wire from the board list — `chrome.js` carries the same warning over the same field.
    node.textContent = text;
    return node;
  }

  function metaRow(board) {
    const row = doc.createElement('div');
    row.className = 'velm-lib-meta';

    const items = doc.createElement('span');
    items.className = 'velm-lib-items';
    items.textContent = itemCountLabel(board.items);
    row.append(items);

    // ⚠ **When it was deleted, not when it was last touched.** On this one screen the question
    // is not "how fresh is this" but "how long have I got", and a trashed board's modified time
    // is frozen at whatever it was before it went in — so showing it would put an unchanging
    // date under every card in the trash.
    const when = doc.createElement('span');
    when.className = 'velm-lib-when';
    when.textContent = board.trashed
      ? `deleted ${relativeTime(board.trashed, Date.now())}`
      : relativeTime(board.modified, Date.now());
    row.append(when);

    if (board.space) {
      const chip = doc.createElement('span');
      chip.className = 'velm-lib-folder';
      chip.title = `In ${board.space}`;
      const name = doc.createElement('span');
      name.className = 'velm-lib-name';
      name.textContent = board.space;
      chip.append(icon(doc, 'folder', { size: 11 }), name);
      row.append(chip);
    }
    return row;
  }

  /**
   * A pinned folder, drawn in the grid at a card's own size.
   *
   * The size is the point: it sits in the Pinned band beside starred boards, and a tile that was
   * a different height would break the row the band exists to be. It reads as a *container*
   * rather than as a board through what fills it — a folder mark where a board has its picture,
   * on the raised tint rather than a picture's own, and a count of boards where a board has a
   * count of items.
   */
  function folderTile(space) {
    const node = doc.createElement('button');
    node.type = 'button';
    node.className = 'velm-lib-card velm-lib-tile';
    node.title = `Open ${space.name}`;

    const box = doc.createElement('div');
    box.className = 'velm-lib-well';
    box.append(icon(doc, 'folder', { size: 36 }));

    const count = doc.createElement('div');
    count.className = 'velm-lib-meta';
    const tally = doc.createElement('span');
    tally.className = 'velm-lib-items';
    tally.textContent = space.boards === 1 ? '1 board' : `${space.boards} boards`;
    count.append(tally);

    node.append(box, titleLine(space.name), count);
    node.addEventListener('click', () => {
      state.scope = { kind: 'space', name: space.name };
      drawLibrary();
    });
    return node;
  }

  /** The list layout: `library.rs`'s `list_row`, at 32 points a row. */
  function listOf(boards) {
    const list = doc.createElement('div');
    list.className = 'velm-lib-list';
    for (const board of boards) {
      const trashed = Boolean(board.trashed);
      const row = doc.createElement(trashed ? 'div' : 'a');
      row.className = 'velm-lib-row';
      row.id = cardDomId('list', board.id);
      if (!trashed) row.href = buildViewerUrl(server, board.id, token, win.location.href);

      const glyph = icon(doc, 'frame', { size: 16 });
      glyph.classList.add('velm-lib-row-icon');
      row.append(glyph, titleLine(board.title));

      const folder = doc.createElement('span');
      folder.className = 'velm-lib-folder';
      folder.textContent = board.space || '—';
      row.append(folder);

      const items = doc.createElement('span');
      items.className = 'velm-lib-items';
      items.textContent = itemCountLabel(board.items);

      const when = doc.createElement('span');
      when.className = 'velm-lib-when';
      when.textContent = board.trashed
        ? `deleted ${relativeTime(board.trashed, Date.now())}`
        : relativeTime(board.modified, Date.now());
      row.append(items, when);

      if (board.starred) {
        const star = doc.createElement('span');
        star.className = 'velm-lib-star';
        star.title = 'Starred in the Velm app';
        star.setAttribute('role', 'img');
        star.setAttribute('aria-label', 'Starred');
        star.append(icon(doc, 'star', { filled: true, size: 14 }));
        row.append(star);
      }
      list.append(row);
    }
    return list;
  }

  function sectionHeader(text) {
    const node = doc.createElement('h3');
    node.className = 'velm-lib-section';
    node.textContent = text;
    return node;
  }

  function caption(text) {
    const node = doc.createElement('div');
    node.className = 'velm-lib-caption';
    node.textContent = text;
    return node;
  }

  function noteLine(className, text) {
    const node = doc.createElement('p');
    node.className = className;
    node.textContent = text;
    return node;
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

  /**
   * Bringing a board across from Miro.
   *
   * ⚠ **The payload is taken from a real paste event, and it is the `text/html` flavour.**
   * Miro's clipboard payload is delimited — `<--(miro-data-v1)…(/miro-data-v1)-->` — and it
   * rides in HTML, so reading `text/plain` gets a page of words and no board. Taking it from
   * the event rather than from `navigator.clipboard.read()` also means no permission prompt:
   * a paste *is* the person's consent, expressed as a gesture rather than as a dialog.
   *
   * The `contenteditable` is a receptacle for the event and nothing else — its own DOM is
   * never read, and the paste is prevented from ever landing in it, because a 1.1 MB board
   * rendered as HTML in the page is megabytes of layout for something nobody looks at.
   */
  let pasted = null;
  /**
   * Making an empty board — the desktop's accent button, and the same verb.
   *
   * ⚠ **The name is typed before anything is created, not after.** The desktop raises a dialog
   * with the title already selected (feedback 16), because a board called *Untitled* that you
   * meant to name is a board you have to find again to rename. `prompt` is the only modal a
   * page has without building one, and building one here would be a second dialog system for
   * a single field.
   *
   * The body is the name as plain UTF-8 and nothing else: a request format with exactly one
   * field does not need a grammar. The answer carries the id, because the server chooses it —
   * four of this user's boards are called *Untitled*, so `free_stem` appends `-2`, and a client
   * that guessed the id from the name would open the wrong board or none.
   */
  function wireNewBoard() {
    if (!el.newBoard) return;
    el.newBoard.addEventListener('click', async () => {
      const name = (win.prompt('Name the new board', 'Untitled') || '').trim();
      if (!name) return;
      el.newBoard.disabled = true;
      const headers = { 'content-type': 'text/plain; charset=utf-8' };
      if (token) headers.authorization = `Bearer ${token}`;
      const answer = await request(
        apiUrl(server.base, 'boards'), { method: 'POST', headers, body: name }, BOARDS_TIMEOUT_MS,
      );
      el.newBoard.disabled = false;
      if (answer.error || !answer.response.ok) {
        // 405 means this server predates the create route — so the button goes away rather
        // than staying to be pressed again. Everything else is worth reporting and retrying.
        if (!answer.error && answer.response.status === 405) {
          el.newBoard.hidden = true;
          el.sub.textContent = 'This server is too old to make boards. Update velmd on it.';
          return;
        }
        el.sub.textContent = answer.timedOut
          ? 'That took too long — the board may still have been made. Reload to see.'
          : 'Could not make the board.';
        return;
      }
      let made = null;
      made = await readJson(answer.response, BOARDS_TIMEOUT_MS, win);
      if (!made || typeof made.id !== 'string') {
        el.sub.textContent = 'Your server answered something this page could not read.';
        return;
      }
      // Straight into it, which is what the desktop does — a New board button that leaves you
      // looking at a list is a button that has made you find your own board.
      win.location.href = buildViewerUrl(server, made.id, token, win.location.href);
    });
  }

  function wireImport() {
    if (!el.importOpen || !el.importPanel) return;
    const said = (message, bad) => {
      if (!el.importSaid) return;
      el.importSaid.textContent = message;
      el.importSaid.dataset.bad = bad ? 'true' : 'false';
    };
    el.importOpen.addEventListener('click', () => {
      el.importPanel.hidden = false;
      el.importOpen.hidden = true;
      if (el.importDrop) el.importDrop.focus();
    });
    if (el.importDrop) {
      el.importDrop.addEventListener('paste', (event) => {
        event.preventDefault();
        const data = event.clipboardData;
        if (!data) return;
        const html = data.getData('text/html') || '';
        // The delimiter, checked here so the person hears about a wrong paste immediately
        // rather than after a round trip. The server checks it too — this is a courtesy, not
        // the gate, and it says so because a check in a page is never a check.
        if (!html.includes('miro-data-v1')) {
          pasted = null;
          said('That paste does not look like a Miro board. Select the board in Miro with ⌘A, then ⌘C.', true);
          return;
        }
        pasted = html;
        said(`Ready — ${Math.round(html.length / 1024)} KB of board. Give it a name.`, false);
      });
    }
    if (el.importGo) {
      el.importGo.addEventListener('click', async () => {
        if (!pasted) {
          said('Nothing pasted yet.', true);
          return;
        }
        const name = (el.importName && el.importName.value.trim()) || 'Miro import';
        el.importGo.disabled = true;
        said('Bringing it across — this reads every widget, so it takes a moment.', false);
        // ⚠ The passphrase in a header, never the query string, for the reason the board list
        // gives: a URL is in history and in every access log between here and the server.
        const headers = { 'content-type': 'text/html; charset=utf-8' };
        if (token) headers.authorization = `Bearer ${token}`;
        const url = `${apiUrl(server.base, 'import')}?title=${encodeURIComponent(name)}`;
        // Its own timeout, and a generous one: the server decodes ~600 widgets and writes a
        // board. The page-wide BOARDS_TIMEOUT_MS is for a list and would abort a real import
        // that was working perfectly.
        const answer = await request(url, { method: 'POST', headers, body: pasted }, 120_000);
        el.importGo.disabled = false;
        if (answer.error) {
          said(answer.timedOut
            ? 'That took too long. The board may still have arrived — reload this page to see.'
            : 'Could not reach your server.', true);
          return;
        }
        if (!answer.response.ok) {
          said(`Your server refused it (${answer.response.status}).`, true);
          return;
        }
        let made = null;
        try { made = await answer.response.json(); } catch { made = null; }
        if (!made || typeof made.id !== 'string') {
          said('Your server answered something this page could not read.', true);
          return;
        }
        const short = made.degraded
          ? `${made.items} items, ${made.degraded} of them simplified`
          : `${made.items} items`;
        said(`Brought "${made.title}" across — ${short}. Reloading the list…`, false);
        pasted = null;
        // Reloaded rather than a card appended by hand: the list is drawn from the server's
        // own answer, and building a second way to add a row to it is a second thing that can
        // be wrong about what is on the server.
        win.setTimeout(() => win.location.reload(), 1200);
      });
    }
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
  return {
    connect,
    get server() { return server; },
    get boards() { return library.boards; },
    get library() { return library; },
    get state() { return state; },
    /**
     * Change scope, grouping or layout the way a click does, so a harness can drive the page.
     *
     * Named `view` and not `show` because there is already a `show(node, {title, detail})` in
     * this closure that does something else entirely.
     */
    view(next = {}) {
      Object.assign(state, next);
      drawLibrary();
    },
  };
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
