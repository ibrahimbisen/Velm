// The way in: sign in to a `velmd` server, set one up for the first time, or join one with an
// invite code.
//
// One page, three states. Two of them are decided by a single `GET /api/v1/whoami` on load and
// the third is reached by pressing a button — the reasoning for both is in `signin.html`'s own
// header and is not repeated here. This file is the half that asks, classifies the answer, and
// puts one of four sentences on the screen.
//
// # ⚠ The invite code is a secret, and it is handled exactly like the password
//
// It is read out of the field inside the submit handler, put into one JSON body, and dropped.
// It is never written into a URL, a link, `localStorage`, `sessionStorage` or a log line, and
// it never reaches a `?` on this page. Everything the section below says about the password
// applies to it word for word — including what cannot be promised about the heap.
//
// ⚠ **This file does not validate the code, and that is deliberate.** The server folds case,
// drops dashes and spaces, and maps the read-alike characters the alphabet leaves out. A
// client that re-implemented any of that would be a second copy of a rule with nothing keeping
// the two in step, and the way it fails is a page refusing a code the server would have taken.
// So the field is trimmed, checked for being empty, and sent.
//
// # ⚠ The password is never stored, and "never" has an exact meaning here
//
// It is read out of the field inside the submit handler, put into one JSON body, and the field
// is cleared the moment the server accepts it. It is not written to `localStorage`, not to
// `sessionStorage`, not to a module-level variable, not into a URL, and not into any object
// that outlives the handler — which is the whole of what this file can promise. What it
// **cannot** promise is that the string is gone from the JavaScript heap: strings are immutable
// and there is no `memset` in this language, so the bytes live until the collector gets to
// them. Saying so is worth more than a comment claiming otherwise, and it is why the only
// mitigation that matters is the one above: nothing here ever *keeps* it.
//
// # ⚠ Why the failure message does not say which half was wrong
//
// *"That username and password do not match"*, never *"there is no such user"*. A message that
// distinguishes the two is an **account enumeration oracle**: anyone who can reach this page
// can then ask it, one name at a time, who has an account on this server — and a username is
// half a credential. This server holds one person's irreplaceable boards and its whole
// audience is a handful of people who already know each other's names, so the cost of being
// vague is nil and the benefit is that the page cannot be used as a directory.
//
// ⚠ **This is only half the defence, and the other half is not in this file.** A server that
// answers a missing username in 2ms and a wrong password in 300ms has told the attacker which
// it was, whatever the wording. `velmd` must do the same Argon2id work for a username it has
// never heard of as for one it has — a verify against a fixed dummy hash. That is reported to
// the server's owner rather than solved here, because it cannot be solved here.
//
// # ⚠ The `VELMD_TOKEN` passphrase is untouched by all of this
//
// Accounts are an **additional** way to be authorised, never a replacement. `vellum-app`'s
// sync sends `Authorization: Bearer` and has nowhere to keep a cookie; `boards.js` sends the
// same header; the wasm client carries `?token=` because a module fetch cannot be handed a
// header. Breaking any of those breaks sync on the user's own Mac. Nothing in this file
// touches them: it only ever adds a cookie, and a cookie is a third arm on the server's
// `authorised()`, not a new gate in front of the other two.
//
// # ⚠ Everything above `mount` is testable without a browser
//
// `mount()` is the only function that touches a DOM, and it takes its `document`, `window` and
// `fetch` as arguments. Nothing runs at import time. That is not tidiness — it is the only way
// a `node --input-type=module` harness can drive the redirect validator, the setup-flag reader
// and the failure classifier, which are the three pieces most able to be wrong while looking
// right, and A/B them against a deliberately broken line.

/**
 * How long to wait for `whoami` before calling the server unreachable.
 *
 * The same 8s `boards.js` gives its health probe, and for the same reason: this is a
 * reachability question, and a server that is up answers it out of memory.
 */
export const WHOAMI_TIMEOUT_MS = 8000;

/**
 * How long to wait for a sign-in or an account creation.
 *
 * ⚠ Much longer than the probe, **on purpose**: the thing on the other end is Argon2id, which
 * is slow by design — that is the entire point of choosing it over a fast hash. A hundred
 * milliseconds of deliberate work on a Mac can be a second or more on whatever small machine
 * somebody parks this on, and a timeout that fires while the server is still hashing would
 * report *"could not reach your server"* about a server that is working perfectly.
 */
export const SUBMIT_TIMEOUT_MS = 20000;

/**
 * The shortest password this page will offer to the server.
 *
 * **Twelve, and length is the only rule.** The reasoning, since a floor picked without one is
 * a floor somebody later lowers:
 *
 * - The hashes live in `accounts.json`, in the same directory as the boards. So a copy of the
 *   data directory is a copy of the hashes, and the threat is **offline** guessing, not online
 *   — which is the case Argon2id is chosen for and the case a short password loses anyway.
 *   Eight characters of human-chosen password is around 30 bits of real entropy; Argon2id
 *   makes each guess expensive, and 2^30 expensive guesses is still a weekend.
 * - **No composition rules**, deliberately, and NIST SP 800-63B says the same: demanding a
 *   capital and a symbol reliably produces `Password1!`, which is shorter in practice than
 *   what the same person picks when asked only for length.
 * - The floor is asked for **once**, when the account is made. It is not re-checked at sign-in
 *   — an existing account whose password predates a future change to this number must still be
 *   able to get in, and a sign-in form that refuses a password the server would have accepted
 *   is a lockout written by the client.
 */
export const MIN_PASSWORD = 12;

/**
 * The longest this page will send.
 *
 * ⚠ **Advisory only.** `maxlength` on an input is a typing limit, not a limit — a request
 * built by hand ignores it entirely — so the server must cap the length it is willing to hash
 * or a megabyte password is a way to spend a core. 128 is far above anything a person or a
 * manager produces and far below anything that costs the server real work.
 */
export const MAX_PASSWORD = 128;

/**
 * How long a session lasts, when the server does not say.
 *
 * Fourteen days, absolute rather than sliding. The point of this whole port is casual access
 * from whatever device is to hand, so a lifetime measured in hours means signing in every time
 * and is how people end up choosing a password they can type quickly. Fourteen days is long
 * enough to be invisible in ordinary use, and short enough that a browser left signed in on a
 * borrowed machine stops being a way in within a fortnight.
 *
 * ⚠ **Absolute, not sliding**, and that is the half worth defending: a sliding window never
 * expires for anyone who keeps using it, which is exactly the borrowed-machine case it would
 * need to cover.
 *
 * ⚠ **The server is the authority and this is only the fallback.** If `whoami` reports
 * `session_days`, that number is what the page says — otherwise a server that picks 30 leaves
 * this page telling people 14, which is the class of quiet lie this codebase has been burned
 * by before (`locked: false`).
 */
export const DEFAULT_SESSION_DAYS = 14;

/** Where a signed-in person is sent. `serve.rs` maps `/` here too; this is its own name. */
export const BOARDS_PATH = '/boards.html';

/** This page's own path, so a `?next=` cannot point back at it and make a loop. */
export const SIGNIN_PATH = '/signin';

/** The floor and ceiling on a `Retry-After`. See [`retryAfterMs`]. */
const MIN_WAIT_MS = 1000;
const MAX_WAIT_MS = 300000;

/** How long the button stays disabled after a 429 that named no wait at all. */
const DEFAULT_WAIT_MS = 30000;

/** How much of a server's own sentence is worth repeating. See [`serverReason`]. */
const MAX_REASON_CHARS = 200;

// ─────────────────────────────────────────────────────────────────────────────
// Pure helpers
// ─────────────────────────────────────────────────────────────────────────────

/**
 * Where to go after signing in — `?next=` if it is safe, the boards otherwise.
 *
 * ⚠ **This is an open-redirect gate, and it is the reason the parameter is validated rather
 * than used.** A sign-in page that will forward anywhere is the classic phishing amplifier:
 * `…/signin?next=https://evil.example/velm` produces a link that wears this server's own
 * address, asks for the password on this server's own page, and then hands the person to a
 * page somebody else wrote — which is where the *second* password prompt would be. The
 * defence has to be structural rather than a list of bad hosts:
 *
 * - It must be spelled as a **path**, so `https://…` and `//evil.example` are both refused
 *   before anything is parsed.
 * - A leading slash-backslash is refused **by name**, because the URL parser treats a
 *   backslash as a slash in the authority position for http(s) — so that string resolves to
 *   `//evil.example` and is a cross-origin URL wearing a path's clothes.
 * - And the parsed result's origin is compared anyway, which is belt and braces: the two
 *   checks above should already make it impossible, and a redirect gate is the wrong place to
 *   be clever about which check is redundant.
 *
 * It also refuses **this page**, or signing in would land back on the form it just left.
 *
 * ⚠ Nothing produces a `?next=` today. It is validated here so that whoever adds the hop —
 * `boards.js`'s 401 branch is the obvious one — adds it to a page that already refuses the
 * dangerous half, rather than adding the parameter and the vulnerability together.
 */
export function safeNext(raw, origin) {
  if (typeof raw !== 'string' || raw === '') return null;
  if (raw[0] !== '/') return null;
  if (raw[1] === '/' || raw[1] === '\\') return null;
  let url;
  try {
    url = new URL(raw, origin);
  } catch {
    return null;
  }
  if (url.origin !== origin) return null;
  if (url.pathname === SIGNIN_PATH || url.pathname === `${SIGNIN_PATH}.html`) return null;
  return `${url.pathname}${url.search}${url.hash}`;
}

/**
 * Does this server have no accounts on it yet?
 *
 * The contract is one key on the body of `whoami`'s 401: `{"setup": true}` when the accounts
 * file is empty, `{"setup": false}` when it is not.
 *
 * ⚠ **Anything else is read as "there are accounts", and the direction of that degradation is
 * chosen rather than incidental.** The two ways to be wrong are not symmetrical:
 *
 * - Guessing *setup* on a server that has accounts would offer a stranger a form headed
 *   *"the first account owns this server"* on a server that is already somebody's. The server
 *   refuses the POST, so nothing is lost but the trust of whoever read it.
 * - Guessing *sign in* on a first-run server is a dead end — a form nobody can satisfy — but
 *   it is an honest one, and it is also the **correct** answer for the case that actually
 *   produces an unreadable body: an older `velmd` with no accounts in it at all, whose
 *   `/api/v1/whoami` is a 404 or whose 401 is `text/plain`. There is no setting up to be done
 *   on that server; there is a passphrase, on the boards page.
 *
 * So the missing-flag case resolves to the answer that is right for the reason the flag is
 * missing. That is what makes it a degradation rather than a guess.
 */
export function readSetupFlag(body) {
  return Boolean(body && typeof body === 'object' && body.setup === true);
}

/**
 * How long a session lasts, from whatever the server said.
 *
 * Refuses a value that is not a finite number of days in a range a person could mean, so a
 * `null`, a string, or a server that sends milliseconds by mistake falls back rather than
 * putting *"You will stay signed in for 1209600000 days"* on the screen.
 */
export function sessionDaysFrom(body, fallback = DEFAULT_SESSION_DAYS) {
  const raw = body && typeof body === 'object' ? body.session_days : undefined;
  if (typeof raw !== 'number' || !Number.isFinite(raw)) return fallback;
  if (raw < 1 || raw > 3650) return fallback;
  return Math.round(raw);
}

/** The sentence under the sign-in button. One place, so the number cannot be said twice. */
export function sessionSentence(days) {
  const span = days === 1 ? 'a day' : `${days} days`;
  return `This device stays signed in for ${span}. Sign out when you are done on a computer that is not yours.`;
}

/**
 * Turn a `Retry-After` into a number of milliseconds to wait.
 *
 * The header has two legal spellings — a count of seconds, or an HTTP date — and both are
 * handled, because which one arrives depends on whatever is in front of the server as much as
 * on the server.
 *
 * ⚠ **Clamped at both ends.** A value in the past (a clock skew of a few seconds between here
 * and the server is ordinary) would otherwise re-enable the button instantly and make the page
 * look like it ignored the refusal; and a `Retry-After: 86400` — which a reverse proxy will
 * emit without being asked — would disable the only button on the page for a day with nothing
 * on screen offering a way back. Five minutes is the ceiling: past that, letting the person
 * press it and be refused again is more honest than a page that has decided for them.
 */
export function retryAfterMs(header, nowMs = Date.now()) {
  if (typeof header !== 'string') return null;
  const trimmed = header.trim();
  if (trimmed === '') return null;
  if (/^\d+$/.test(trimmed)) return clampWait(Number(trimmed) * 1000);
  const when = Date.parse(trimmed);
  if (Number.isNaN(when)) return null;
  return clampWait(when - nowMs);
}

function clampWait(ms) {
  if (!Number.isFinite(ms)) return null;
  return Math.min(Math.max(ms, MIN_WAIT_MS), MAX_WAIT_MS);
}

/**
 * What happened, and — the load-bearing half — what to do about it.
 *
 * ⚠ **Four causes reach the one message box and each needs a different next action**, so a
 * single *"something went wrong"* would be four failures wearing one coat. The person in front
 * of a 429 must wait; in front of a 500 must go and read the server's terminal, because
 * nothing they type here will help; in front of an unreachable server must go and look at the
 * server or the network, because *"try again"* is advice that cannot work until something
 * changes; and in front of a 401 must type again, which is the only one of the four where the
 * form is the answer.
 *
 * `want` is what the caller does with it:
 *   `form`    leave the form up; the person tries again
 *   `wait`    disable the button for `waitMs`, then give it back
 *   `retry`   no form is useful — offer *Try again*
 *   `restart` the page's whole premise has changed; re-ask `whoami`
 */
export function describeFailure(context = {}) {
  const { phase, status, network, timedOut, waitMs, reason } = context;

  if (network) {
    // ⚠ These two are one action and two sentences on purpose. A timeout means something is
    // there and is not answering — a machine asleep, a network that has gone away mid-request
    // — and a hard failure means nothing answered at all. The advice is the same and the thing
    // to go and look at is not, so saying "could not reach" about a request that waited eight
    // seconds would send somebody to check a cable that is fine.
    return {
      tone: 'bad',
      want: 'retry',
      title: timedOut ? 'Your server did not answer in time.' : 'Could not reach your server.',
      detail:
        'Check that velmd serve is still running, and that this device is on the same network as it. ' +
        'Trying again will not help until one of those changes.',
    };
  }

  // ⚠ The sign-in worked and the browser did not keep the session. This is its own sentence
  // because it is the one failure where **nothing is wrong with what the person typed** and
  // every other message on this page would send them to type it again — which would work, and
  // land here again, for ever. The cause is almost always the same one, and it is nameable.
  if (phase === 'verify') {
    return {
      tone: 'bad',
      want: 'retry',
      title: 'Your server accepted that password, and this browser did not keep the session.',
      detail:
        'That happens on a plain http:// address that is not localhost: the session cookie is ' +
        'marked Secure, and a browser will not store one of those over http. Reach this server ' +
        'over https, or over http://localhost. A browser set to block all cookies does it too.',
    };
  }

  // ⚠ **The invite form's own arms, and they come first because the generic ones below would
  // otherwise be wrong on this form rather than merely vague.** A 403 there means the code was
  // refused, and *"that username and password do not match"* would send somebody to retype two
  // things that were both right. A 409 there means the username is taken, not that the server
  // has just been set up by somebody else, so `restart` would throw the form away.
  if (phase === 'invite' && (status === 401 || status === 403)) {
    return {
      tone: 'bad',
      want: 'form',
      title: 'That invite code is not one this server is waiting for.',
      detail:
        'Check every character and try again. A code works once, and it stops working after a ' +
        'while, so ask whoever gave you this one for another if it keeps being refused.',
    };
  }

  if (phase === 'invite' && status === 409) {
    // ⚠ The second sentence is a fact, not reassurance: the server checks the name **after**
    // the code and spends the code only once everything else has passed, so a taken username
    // leaves the code open. Saying so is what stops somebody going away for a new one.
    return {
      tone: 'bad',
      want: 'form',
      title: 'Somebody on this server already has that username.',
      detail: 'Choose a different one. Your code has not been used, so it still works.',
    };
  }

  if (status === 401 || status === 403) {
    // ⚠ One sentence for both halves. Never "no such user" — see this file's header for why,
    // and note that 403 is folded in here rather than given its own wording: a server that
    // distinguishes "wrong password" from "that account may not sign in" by status code has
    // reopened the oracle the message was written to close.
    return {
      tone: 'bad',
      want: 'form',
      title: 'That username and password do not match.',
      detail: 'Check both and try again. This page will not say which of the two was wrong.',
    };
  }

  if (status === 429) {
    const seconds = Math.max(1, Math.round((waitMs ?? DEFAULT_WAIT_MS) / 1000));
    return {
      tone: 'bad',
      want: 'wait',
      // ⚠ A refused invite code spends the same per-address budget a wrong password does, so
      // this arm is reachable from the invite form too — and *"sign-in attempts"* would be a
      // lie there. The advice is identical, which is why it is one arm and one ternary.
      title: phase === 'invite' ? 'Too many attempts.' : 'Too many sign-in attempts.',
      detail:
        `Your server has stopped checking for a moment. Wait about ${seconds} ` +
        `${seconds === 1 ? 'second' : 'seconds'} and try again. Asking sooner only makes the wait longer.`,
    };
  }

  if (status === 409) {
    // Someone else created the first account between this page loading and this button being
    // pressed. Rare, and the honest answer is not an error — it is that the page is now
    // looking at a different kind of server and should go and ask again.
    return {
      tone: 'note',
      want: 'restart',
      title: 'This server already has an account on it.',
      detail: 'Someone set it up first. Sign in with that account instead.',
    };
  }

  if (status === 404) {
    // An older `velmd`: no accounts, and no route to make one. Not a fault, and the useful
    // thing to say is where the door actually is on that build.
    return {
      tone: 'note',
      want: 'retry',
      title: 'This server does not have accounts.',
      detail:
        'It is an older velmd, which asks for a single passphrase instead: the VELMD_TOKEN it ' +
        'was started with. Open the boards page and it will ask you for it.',
    };
  }

  if (typeof status === 'number' && status >= 500) {
    return {
      tone: 'bad',
      want: 'retry',
      title: 'Your server answered, and something went wrong inside it.',
      detail:
        `Nothing is wrong with what you typed. The fault is on the server (${status}), and the error ` +
        'will be on the terminal where velmd serve is running; that is the thing to go and read.',
    };
  }

  if (status === 400) {
    return {
      tone: 'bad',
      want: 'form',
      title: 'Your server refused those details.',
      // ⚠ The server's own sentence, and it reaches the page through `textContent` — never
      // `innerHTML`. It is the one string on this page that comes off the wire, and a
      // `<script>` in it would be script execution on the page that holds the password field.
      detail: reason
        ? `It said: ${reason}`
        : 'It may have a stricter rule for names or passwords than this page does.',
    };
  }

  return {
    tone: 'bad',
    want: 'retry',
    title: 'Your server answered something unexpected.',
    detail:
      `It replied ${typeof status === 'number' ? status : 'in a way this page did not recognise'}. ` +
      'The terminal where velmd serve is running will have more.',
  };
}

/**
 * Whether a character must never reach the page.
 *
 * ⚠ This is `serve.rs`'s `is_unprintable` arriving from the other direction, and it is written
 * out as code points rather than as a regular expression **on purpose**: a literal U+202E in a
 * source file is invisible in every editor and reverses the line it sits on, so a character
 * class spelled with the characters themselves is a hazard in the file that exists to defuse
 * one.
 *
 * `char::is_control`'s equivalent — anything below U+0020 and the U+007F..U+009F band — is not
 * enough on its own. Category **Cf** carries the bidirectional overrides and isolates: a string
 * containing one renders reversed from that point on, so a server's refusal could be forged to
 * read as though this page had said it. The zero-width characters hide a segment outright.
 * Joiners are deliberately left alone — U+200C and U+200D are orthography and alter neither the
 * order nor the visibility of what follows.
 */
function isUnprintable(code) {
  if (code < 0x20 || (code >= 0x7f && code <= 0x9f)) return true;
  if (code >= 0x200b && code <= 0x200f) return true;
  if (code === 0x2028 || code === 0x2029) return true;
  if (code >= 0x202a && code <= 0x202e) return true;
  if (code >= 0x2066 && code <= 0x2069) return true;
  return false;
}

/**
 * A server's own sentence, made safe to repeat.
 *
 * Control and direction-altering characters become spaces (see [`isUnprintable`]), runs of
 * whitespace collapse, and there is a hard cap: a 400 whose body is an entire HTML error page
 * is not a sentence, and pasting one into the message box makes the box the whole screen.
 *
 * ⚠ Making it *safe to repeat* is not the same as making it safe to trust, and only one of
 * those is this function's job. What stops a `<script>` in a refusal being run is that the
 * caller writes it with `textContent`; this only stops it being unreadable or misleading.
 */
export function serverReason(text) {
  if (typeof text !== 'string') return null;
  let value = text;
  const trimmed = text.trim();
  if (trimmed.startsWith('{')) {
    try {
      const parsed = JSON.parse(trimmed);
      const named =
        parsed && typeof parsed === 'object' ? (parsed.reason ?? parsed.error ?? parsed.message) : null;
      value = typeof named === 'string' ? named : '';
    } catch {
      value = '';
    }
  }
  const clean = Array.from(value)
    .map((ch) => (isUnprintable(ch.codePointAt(0)) ? ' ' : ch))
    .join('')
    .replace(/\s+/g, ' ')
    .trim();
  if (clean === '') return null;
  return clean.length > MAX_REASON_CHARS ? `${clean.slice(0, MAX_REASON_CHARS)}…` : clean;
}

// ─────────────────────────────────────────────────────────────────────────────
// The page
// ─────────────────────────────────────────────────────────────────────────────

/**
 * Every id `signin.html` promises, and `mount` **throws** when one is missing.
 *
 * Loud on purpose: a renamed id would otherwise make one control silently do nothing, which is
 * this repository's signature defect — code that is written, tested, and unreachable.
 */
const IDS = {
  title: 'velm-title',
  sub: 'velm-sub',
  message: 'velm-message',
  messageTitle: 'velm-message-title',
  messageDetail: 'velm-message-detail',
  signin: 'velm-signin',
  signinUser: 'velm-signin-user',
  signinPass: 'velm-signin-pass',
  signinUserError: 'velm-signin-user-error',
  signinPassError: 'velm-signin-pass-error',
  signinGo: 'velm-signin-go',
  lifetime: 'velm-lifetime',
  setup: 'velm-setup',
  setupUser: 'velm-setup-user',
  setupPass: 'velm-setup-pass',
  setupPass2: 'velm-setup-pass2',
  setupUserError: 'velm-setup-user-error',
  setupPassError: 'velm-setup-pass-error',
  setupPass2Error: 'velm-setup-pass2-error',
  setupGo: 'velm-setup-go',
  // The invite state. Same naming as the other two: `velm-<form>-<field>` and a matching
  // `-error` line under each field.
  invite: 'velm-invite',
  inviteCode: 'velm-invite-code',
  inviteUser: 'velm-invite-user',
  invitePass: 'velm-invite-pass',
  invitePass2: 'velm-invite-pass2',
  inviteCodeError: 'velm-invite-code-error',
  inviteUserError: 'velm-invite-user-error',
  invitePassError: 'velm-invite-pass-error',
  invitePass2Error: 'velm-invite-pass2-error',
  inviteGo: 'velm-invite-go',
  // The two buttons that move between the sign-in form and the invite form. They are listed
  // here, and not treated as optional, because a state nobody can reach is the same defect as
  // a control that does nothing: the page would simply never offer the third form.
  haveCode: 'velm-have-code',
  inviteBack: 'velm-invite-back',
  retry: 'velm-retry',
};

export function mount(options = {}) {
  const doc = options.document ?? document;
  const win = options.window ?? window;
  const fetchImpl = options.fetch ?? ((...args) => win.fetch(...args));

  const el = {};
  for (const [name, id] of Object.entries(IDS)) {
    el[name] = doc.getElementById(id);
    if (!el[name]) throw new Error(`signin.html is missing #${id}`);
  }

  let sessionDays = DEFAULT_SESSION_DAYS;
  let busy = false;
  let waitTimer = null;

  // ⚠ **One authority for the length rule — and `signin.html` carries two more spellings of
  // it.** The markup declares `minlength="12"` and `maxlength="128"` so that a build where this
  // module fails to load still refuses a short password, and the hint under the field says the
  // number in words. Neither of those can read a constant from here, so the attributes are
  // re-stamped from the constants at mount: that makes the *constraint* single-sourced and
  // leaves exactly one thing to keep in step by hand, the sentence in `signin.html`. It is
  // written down because a number living in three places with nothing checking them is how
  // `locked: false` and `THEME_BORDER` both happened in this repository.
  el.setupPass.minLength = MIN_PASSWORD;
  el.setupPass2.minLength = MIN_PASSWORD;
  el.setupPass.maxLength = MAX_PASSWORD;
  el.setupPass2.maxLength = MAX_PASSWORD;
  // The invite form makes an account too, so it carries the same floor and the same ceiling.
  el.invitePass.minLength = MIN_PASSWORD;
  el.invitePass2.minLength = MIN_PASSWORD;
  el.invitePass.maxLength = MAX_PASSWORD;
  el.invitePass2.maxLength = MAX_PASSWORD;
  // ⚠ No `minLength` on the sign-in field, deliberately. An account whose password predates a
  // change to the floor must still be able to get in; a sign-in form that refuses a password
  // the server would have accepted is a lockout written by the client.
  el.signinPass.maxLength = MAX_PASSWORD;


  // ── requests ──────────────────────────────────────────────────────────────

  async function request(url, init, timeoutMs) {
    const controller = new AbortController();
    // ⚠ Every await on a browser is bounded. `docs/08-web.md` §6 records three separate hangs
    // in the probe from missing exactly this, and a page stuck on "Checking…" on a tablet is
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
   * Read a body once, as text, bounded — and hand back both spellings of it.
   *
   * One read rather than `response.json()` with a `response.text()` fallback, because a body
   * can only be consumed once and velmd answers a refusal as `text/plain` on some routes and
   * as JSON on others. Parsing the text we already hold costs nothing and cannot fail halfway.
   */
  async function readBody(response, timeoutMs) {
    let timer = null;
    try {
      const text = await Promise.race([
        response.text(),
        new Promise((_, reject) => {
          timer = win.setTimeout(() => reject(new Error('body timed out')), timeoutMs);
        }),
      ]);
      let json = null;
      try {
        json = JSON.parse(text);
      } catch {
        /* not JSON, and that is a legal answer */
      }
      return { text, json };
    } catch {
      return { text: '', json: null };
    } finally {
      if (timer !== null) win.clearTimeout(timer);
    }
  }

  function ask(url, timeoutMs) {
    return request(
      url,
      {
        // ⚠ **`no-store`, and it is not an optimisation.** A cached 200 would walk a signed-out
        // person straight to a boards page that then 401s everything; a cached 401 would keep a
        // signed-in person on this form for ever. This answer is about *right now*.
        cache: 'no-store',
        headers: { accept: 'application/json' },
        // ⚠ Explicit rather than left to the default, which is already `same-origin`. Saying it
        // is what makes the intent survive a future edit: the cookie must ride on a request to
        // this server and must **never** be attached to one somewhere else. Setting `include`
        // here would be that mistake, and it would not even work — velmd's CORS answer carries
        // no `Access-Control-Allow-Credentials`.
        credentials: 'same-origin',
      },
      timeoutMs,
    );
  }

  function post(url, payload, timeoutMs) {
    return request(
      url,
      {
        method: 'POST',
        cache: 'no-store',
        credentials: 'same-origin',
        // ⚠ `application/json` is also the CSRF story, and it is worth knowing which half does
        // the work. `SameSite=Strict` is the first half: a cross-site request carries no cookie
        // at all. This is the second: an HTML `<form>` on somebody else's page can only send
        // `application/x-www-form-urlencoded`, `multipart/form-data` or `text/plain`, so a
        // server that *insists* on JSON cannot be driven by a forged form even if the cookie
        // rules ever loosened. The server does insist: `sent_as_json` runs before the body is
        // parsed on both of these routes and answers 415, so this header is not a courtesy.
        headers: { 'content-type': 'application/json', accept: 'application/json' },
        body: JSON.stringify(payload),
      },
      timeoutMs,
    );
  }

  // ── drawing ───────────────────────────────────────────────────────────────

  function show(answer) {
    // ⚠ `textContent`, never `innerHTML`, and that holds for our own strings as much as for the
    // server's: the moment one of these is built by concatenation with something off the wire,
    // an `innerHTML` here is script execution on the page holding the password field.
    el.messageTitle.textContent = answer.title;
    el.messageDetail.textContent = answer.detail;
    el.message.classList.toggle('is-bad', answer.tone !== 'note');
    el.message.hidden = false;
  }

  function clearMessage() {
    el.message.hidden = true;
    el.messageTitle.textContent = '';
    el.messageDetail.textContent = '';
  }

  function clearFieldErrors() {
    for (const key of [
      'signinUserError',
      'signinPassError',
      'setupUserError',
      'setupPassError',
      'setupPass2Error',
      'inviteCodeError',
      'inviteUserError',
      'invitePassError',
      'invitePass2Error',
    ]) {
      el[key].textContent = '';
    }
  }

  function hideForms() {
    el.signin.hidden = true;
    el.setup.hidden = true;
    el.invite.hidden = true;
    el.retry.hidden = true;
  }

  function showChecking() {
    el.title.textContent = 'Velm';
    el.sub.textContent = 'Checking…';
    hideForms();
    clearMessage();
    clearFieldErrors();
  }

  function showSignIn() {
    el.title.textContent = 'Sign in';
    el.sub.textContent = 'Your boards are on this server.';
    el.lifetime.textContent = sessionSentence(sessionDays);
    el.setup.hidden = true;
    el.invite.hidden = true;
    el.retry.hidden = true;
    el.signin.hidden = false;
    focusFirstEmpty(el.signinUser, el.signinPass);
  }

  function showSetup() {
    el.title.textContent = 'Set up Velm';
    el.sub.textContent = 'Make the first account.';
    el.signin.hidden = true;
    el.invite.hidden = true;
    el.retry.hidden = true;
    el.setup.hidden = false;
    focusFirstEmpty(el.setupUser, el.setupPass);
  }

  /**
   * The third state: somebody was given a code.
   *
   * ⚠ Reached by a button and never by `begin`, because `whoami` cannot tell an invited person
   * from an ordinary one — both are a 401 on a server that has accounts. `signin.html`'s header
   * carries the whole argument, including why a *"this server takes new accounts"* flag is a
   * fact worth not publishing.
   *
   * The message box is cleared on the way in. A refusal from the sign-in form still on screen
   * over a different form is a sentence about the wrong thing.
   */
  function showInvite() {
    el.title.textContent = 'Create your account';
    el.sub.textContent = 'Use the code you were given.';
    el.signin.hidden = true;
    el.setup.hidden = true;
    el.retry.hidden = true;
    el.invite.hidden = false;
    clearMessage();
    clearFieldErrors();
    focusFirstEmpty(el.inviteCode, el.inviteUser);
  }

  /**
   * Put the caret in the first field that has nothing in it.
   *
   * ⚠ Only where a pointer is not coarse. Focusing a field on load raises the software keyboard
   * on a phone before the page has been read, which covers most of what was written above it —
   * including, on the setup form, the sentence saying this account will own the server.
   */
  function focusFirstEmpty(first, second) {
    if (win.matchMedia && win.matchMedia('(pointer: coarse)').matches) return;
    const target = first.value.trim() === '' ? first : second;
    try {
      target.focus();
    } catch {
      /* a detached document in a harness; nothing depends on the caret */
    }
  }

  /**
   * Leave for the boards.
   *
   * ⚠ `replace`, not `assign`. With `assign` this page stays in history, so Back from the
   * boards lands on a form that immediately redirects forward again — a page you cannot get out
   * of by going back, which is exactly what Back is for.
   */
  function leave() {
    let next = null;
    try {
      next = safeNext(new URL(win.location.href).searchParams.get('next'), win.location.origin);
    } catch {
      /* a harness with a stub location; the boards are the right default anyway */
    }
    win.location.replace(next ?? BOARDS_PATH);
  }

  // ── the wait after a 429 ──────────────────────────────────────────────────

  function stopWaiting() {
    if (waitTimer !== null) {
      win.clearInterval(waitTimer);
      waitTimer = null;
    }
  }

  /**
   * Hold the button for a while, and say how long it is holding it for.
   *
   * ⚠ A disabled button with no explanation of when it comes back is the thing that reads as
   * broken, so the label counts down. It **stops** — a tick whose progress never reaches its
   * end looks finished while still repainting for ever, which is the trap feedback 21 records
   * about the library's drop flourish. One tick a second, for at most the five minutes
   * [`retryAfterMs`] clamps to, and then the label goes back to what it was.
   */
  function waitBefore(button, label, ms) {
    stopWaiting();
    let left = Math.ceil(ms / 1000);
    button.disabled = true;
    const tick = () => {
      if (left <= 0) {
        stopWaiting();
        button.disabled = false;
        button.textContent = label;
        return;
      }
      button.textContent = `${label} in ${left}s`;
      left -= 1;
    };
    tick();
    waitTimer = win.setInterval(tick, 1000);
  }

  // ── what happened, and what the page does about it ────────────────────────

  function failed(context, button, label) {
    const answer = describeFailure(context);
    show(answer);
    el.sub.textContent = answer.want === 'retry' ? 'Not connected.' : 'Not signed in.';

    // ⚠ **`wait` only means "hold the button" when there is a button holding it.** `begin`
    // reaches this function with no button at all, and an IP-wide rate limiter in front of the
    // whole auth surface will 429 the load-time `whoami` — at which point `showChecking` has
    // already hidden both forms and *Try again*, so without this fall-through the page would
    // sit there naming a wait with no gesture on it. A message with nothing to press is the
    // shape this repository has a standing rule against, arriving from the one direction the
    // rule does not usually cover: not a control that does nothing, a screen with no control
    // at all.
    if (answer.want === 'wait') {
      if (button) {
        waitBefore(button, label, context.waitMs ?? DEFAULT_WAIT_MS);
        return;
      }
      hideForms();
      el.retry.hidden = false;
      return;
    }
    if (answer.want === 'restart') {
      // The premise changed under us. Ask again rather than reasoning about it here — there is
      // exactly one function that decides which form this page is, and it is `begin`.
      void begin({ keepMessage: true });
      return;
    }
    if (answer.want === 'retry') {
      hideForms();
      el.retry.hidden = false;
    }
  }

  // ── the two submits ───────────────────────────────────────────────────────

  /**
   * Run one submission, with the button held for its duration.
   *
   * ⚠ The password never reaches this function. It is read, used and dropped inside the handler
   * that calls it — nothing that outlives a submit ever holds it, which is the whole of what
   * this file promises about it.
   */
  async function withBusy(button, working, label, body) {
    if (busy) return;
    busy = true;
    stopWaiting();
    button.disabled = true;
    button.textContent = working;
    el.message.hidden = true;
    try {
      await body();
    } finally {
      busy = false;
      // ⚠ Only if the wait timer has not taken the button over. A 429 landing inside `body` sets
      // its own label and its own disabled state, and putting the idle label back here would
      // undo the countdown the person is reading.
      if (waitTimer === null) {
        button.disabled = false;
        button.textContent = label;
      }
    }
  }

  /**
   * Ask `whoami` once more, and only leave if it says yes.
   *
   * ⚠ **This extra round trip is the point, not an excess of caution.** A 204 from the session
   * route means the server made a session; it does not mean this browser *kept* the cookie. The
   * one that gets kept and the one that does not look identical from here — and the case that
   * bites is ordinary rather than exotic: on a plain `http://192.168.x.x` the cookie is marked
   * `Secure` and the browser drops it silently. Without this check the page would redirect to
   * the boards, be bounced back here, and do it again — a loop with nothing on screen to say
   * why. With it, that becomes one sentence naming the cause.
   */
  async function leaveIfTheSessionStuck() {
    const who = await ask('/api/v1/whoami', WHOAMI_TIMEOUT_MS);
    if (who.error) return failed({ phase: 'verify', network: true, timedOut: who.timedOut });
    if (who.response.status === 200) return leave();
    return failed({ phase: 'verify', status: who.response.status });
  }

  async function submitSignIn(event) {
    event.preventDefault();
    clearFieldErrors();

    // ⚠ Trimmed, and **only** trimmed. A trailing space from an autofill or a phone keyboard is
    // invisible on screen and unfixable by the person looking at it. Case is deliberately left
    // alone: whether `Ada` and `ada` are one account is the server's decision, and a client that
    // lower-cased would break sign-in on a server that said they are two.
    const username = el.signinUser.value.trim();
    const password = el.signinPass.value;

    if (username === '') {
      el.signinUserError.textContent = 'Type your username.';
      el.signinUser.focus();
      return;
    }
    if (password === '') {
      el.signinPassError.textContent = 'Type your password.';
      el.signinPass.focus();
      return;
    }

    await withBusy(el.signinGo, 'Signing in…', 'Sign in', async () => {
      const attempt = await post('/api/v1/session', { username, password }, SUBMIT_TIMEOUT_MS);
      if (attempt.error) {
        return failed({ phase: 'signin', network: true, timedOut: attempt.timedOut });
      }
      const { status } = attempt.response;
      if (status === 200 || status === 204) {
        // Cleared the instant it is no longer needed, and **before** the verifying round trip
        // rather than after it: if that request hangs until its timeout, the field must not be
        // sitting there full for twenty seconds on a screen somebody has walked away from.
        el.signinPass.value = '';
        return leaveIfTheSessionStuck();
      }
      // ⚠ The field is deliberately **not** cleared on a refusal. That is the person's own
      // typing in a field doing its job, and wiping it after a 429 they have just been told to
      // wait out means retyping a password to make an attempt they were told not to make yet.
      return refused('signin', attempt.response, el.signinGo, 'Sign in');
    });
  }

  async function submitSetup(event) {
    event.preventDefault();
    clearFieldErrors();

    const username = el.setupUser.value.trim();
    const password = el.setupPass.value;
    const again = el.setupPass2.value;

    if (username === '') {
      el.setupUserError.textContent = 'Choose a username.';
      el.setupUser.focus();
      return;
    }
    if (password.length < MIN_PASSWORD) {
      el.setupPassError.textContent = `At least ${MIN_PASSWORD} characters, and this one is ${password.length}.`;
      el.setupPass.focus();
      return;
    }
    if (password.length > MAX_PASSWORD) {
      el.setupPassError.textContent = `That is longer than ${MAX_PASSWORD} characters.`;
      el.setupPass.focus();
      return;
    }
    if (again !== password) {
      // ⚠ Named as a mismatch and not as "the second one is wrong": there is no way to know
      // which of the two carries the typo, and the pair is what has to be fixed.
      el.setupPass2Error.textContent = 'These two are not the same.';
      el.setupPass2.focus();
      return;
    }

    await withBusy(el.setupGo, 'Creating…', 'Create this account', async () => {
      const made = await post('/api/v1/accounts', { username, password }, SUBMIT_TIMEOUT_MS);
      if (made.error) {
        return failed({ phase: 'setup', network: true, timedOut: made.timedOut });
      }
      const { status } = made.response;
      if (status !== 200 && status !== 201 && status !== 204) {
        return refused('setup', made.response, el.setupGo, 'Create this account');
      }

      // ⚠ **Sign in as a second step rather than assuming the account POST set a cookie.**
      // Nothing in the wire contract says it does, and an account that exists but cannot be used
      // is the worst possible first impression of a server somebody has just set up. A redundant
      // session POST costs one round trip on the one occasion this form is ever used; guessing
      // costs the whole first run.
      const signedIn = await post('/api/v1/session', { username, password }, SUBMIT_TIMEOUT_MS);
      el.setupPass.value = '';
      el.setupPass2.value = '';
      if (signedIn.error) {
        return failed({ phase: 'signin', network: true, timedOut: signedIn.timedOut });
      }
      if (signedIn.response.status === 200 || signedIn.response.status === 204) {
        return leaveIfTheSessionStuck();
      }
      // The account exists — say so, or the person makes it again and meets a 409.
      show({
        tone: 'note',
        title: 'Your account is made, and signing in did not go through.',
        detail: 'Try signing in with it below.',
      });
      return begin({ keepMessage: true });
    });
  }

  /**
   * Make an account from an invite code, then sign in with it.
   *
   * `submitSetup`'s own shape, and deliberately not a shared function with it. The two forms
   * differ in what they send, in which sentences their refusals produce, and in one of them
   * being the only account on the server while the other can never be an admin — so folding
   * them together would be one function with a boolean in it and two of everything inside.
   *
   * ⚠ The code is sent in the JSON body, never in the path and never in a query. See the file
   * header: a code in a URL survives in history and in every access log it passes through, and
   * it stays a working credential until somebody spends it.
   */
  async function submitInvite(event) {
    event.preventDefault();
    clearFieldErrors();

    // Trimmed, and nothing else. The server folds the case, the dashes and the read-alike
    // characters; see the file header for why this page does not.
    const code = el.inviteCode.value.trim();
    const username = el.inviteUser.value.trim();
    const password = el.invitePass.value;
    const again = el.invitePass2.value;

    if (code === '') {
      el.inviteCodeError.textContent = 'Type the code you were given.';
      el.inviteCode.focus();
      return;
    }
    if (username === '') {
      el.inviteUserError.textContent = 'Choose a username.';
      el.inviteUser.focus();
      return;
    }
    if (password.length < MIN_PASSWORD) {
      el.invitePassError.textContent = `At least ${MIN_PASSWORD} characters, and this one is ${password.length}.`;
      el.invitePass.focus();
      return;
    }
    if (password.length > MAX_PASSWORD) {
      el.invitePassError.textContent = `That is longer than ${MAX_PASSWORD} characters.`;
      el.invitePass.focus();
      return;
    }
    if (again !== password) {
      el.invitePass2Error.textContent = 'These two are not the same.';
      el.invitePass2.focus();
      return;
    }

    await withBusy(el.inviteGo, 'Creating…', 'Create my account', async () => {
      const made = await post('/api/v1/accounts', { username, password, code }, SUBMIT_TIMEOUT_MS);
      if (made.error) {
        return failed({ phase: 'invite', network: true, timedOut: made.timedOut });
      }
      const { status } = made.response;
      if (status !== 200 && status !== 201 && status !== 204) {
        return refused('invite', made.response, el.inviteGo, 'Create my account');
      }

      // ⚠ Cleared the moment the server has taken it, and before the sign-in round trip rather
      // than after: the code is spent from here on, and a spent code sitting in a field on a
      // screen somebody walked away from is a secret left on a desk for no reason at all.
      el.inviteCode.value = '';

      // Sign in as a second step, for `submitSetup`'s reason: nothing in the wire contract says
      // the account POST sets a cookie, and an account that exists but cannot be used is the
      // worst possible first minute on somebody else's server.
      const signedIn = await post('/api/v1/session', { username, password }, SUBMIT_TIMEOUT_MS);
      el.invitePass.value = '';
      el.invitePass2.value = '';
      if (signedIn.error) {
        return failed({ phase: 'signin', network: true, timedOut: signedIn.timedOut });
      }
      if (signedIn.response.status === 200 || signedIn.response.status === 204) {
        return leaveIfTheSessionStuck();
      }
      // ⚠ The account exists and the code is spent, so the one thing this sentence must not do
      // is send them back to the code. It sends them to the sign-in form instead.
      show({
        tone: 'note',
        title: 'Your account is made, and signing in did not go through.',
        detail: 'Sign in with the username and password you just chose. The code is used up now.',
      });
      return begin({ keepMessage: true });
    });
  }

  /** A refusal that carries a status: read what the server said, then classify it. */
  async function refused(phase, response, button, label) {
    const { text } = await readBody(response, WHOAMI_TIMEOUT_MS);
    const waitMs =
      response.status === 429
        ? (retryAfterMs(response.headers?.get?.('retry-after')) ?? DEFAULT_WAIT_MS)
        : undefined;
    failed({ phase, status: response.status, reason: serverReason(text), waitMs }, button, label);
  }

  // ── which page this is ────────────────────────────────────────────────────

  /**
   * Decide, once, which of the two states this is — and it is the **only** function that does.
   *
   * Everything that discovers the premise has changed calls back here rather than reasoning
   * about it in place, so there is one derivation of *what kind of server is this* and not four
   * that can come to disagree.
   */
  async function begin({ keepMessage = false } = {}) {
    stopWaiting();
    if (keepMessage) {
      hideForms();
      el.sub.textContent = 'Checking…';
    } else {
      showChecking();
    }

    const who = await ask('/api/v1/whoami', WHOAMI_TIMEOUT_MS);
    if (who.error) {
      return failed({ phase: 'whoami', network: true, timedOut: who.timedOut });
    }

    const { response } = who;
    if (response.status === 200) return leave();

    if (response.status === 401) {
      const { json } = await readBody(response, WHOAMI_TIMEOUT_MS);
      sessionDays = sessionDaysFrom(json);
      if (!keepMessage) clearMessage();
      return readSetupFlag(json) ? showSetup() : showSignIn();
    }

    return failed({ phase: 'whoami', status: response.status });
  }

  el.signin.addEventListener('submit', submitSignIn);
  el.setup.addEventListener('submit', submitSetup);
  el.invite.addEventListener('submit', submitInvite);
  el.retry.addEventListener('click', () => void begin());

  // ⚠ Both of these swap the form on screen and ask the server nothing. `begin` is the only
  // function that decides which state a *fresh* page is in, and it stays that way: these two
  // move between two states it has already established, so calling it here would throw away a
  // half-typed form to be told the same thing it was told a moment ago.
  el.haveCode.addEventListener('click', () => showInvite());
  el.inviteBack.addEventListener('click', () => {
    // The code and the passwords are dropped rather than left in the fields. Somebody who has
    // decided they already have an account has no use for either, and a secret in a hidden
    // field is still a secret in the page.
    el.inviteCode.value = '';
    el.invitePass.value = '';
    el.invitePass2.value = '';
    clearMessage();
    clearFieldErrors();
    showSignIn();
  });

  // ⚠ Back onto this page out of the browser's cache does not re-run a module, so a form
  // restored from bfcache would still be showing "Sign in" to somebody who signed in on another
  // tab in the meantime. One re-ask, only on a restore.
  win.addEventListener('pageshow', (event) => {
    if (event.persisted) void begin();
  });

  void begin();

  // The harness handle: the same shape `boards.js`'s `mount` returns, so a Node fixture can
  // drive a state without reaching into a closure.
  return {
    begin,
    get sessionDays() {
      return sessionDays;
    },
    get busy() {
      return busy;
    },
  };
}
