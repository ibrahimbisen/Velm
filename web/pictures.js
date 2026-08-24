// Putting a picture on the board from a browser tab.
//
// Two ways in, one path afterwards:
//
//   a paste event's file  ─┐
//                          ├─→ hash → upload → place
//   the Image tool's picker┘
//
// `crates/vellum-web/src/pictures.rs` is the other half and its header carries the argument
// for the split. The short version: the page owns the *address* — `index.html` builds
// `blobBase` and `blobSuffix` once, and `blobSuffix` is where a token lives — and the wasm
// side owns the *document*. Handing the address into wasm so it could build the same string
// a second time is how two derivations drift apart.
//
// # ⚠ The order is hash, upload, place — and place is last
//
// An item naming a hash the server has not got is the exact defect `crates/velmd/src/blobs.rs`
// was written to end: the browser draws nothing where the picture is, for ever, because no
// later render can fetch bytes that were never sent. It is also the bug this session was
// asked to fix on the Mac side. So a failed upload leaves the board as it was, and the
// person is told why.
//
// # ⚠ Why a `paste` event and not `navigator.clipboard.read()`
//
// `clip.js`'s Rust half states the decision this appears to break: *"this module never
// touches `navigator.clipboard`, in either direction"*, because reading it needs a permission
// the user did not ask for and the answer is a promise a synchronous export cannot await.
//
// A **paste event** is neither. `event.clipboardData` is already resolved when the handler
// runs, it needs no permission in any browser, and it only exists because the person just
// pressed ⌘V — which is consent, expressed the way the platform expresses it. Nothing here
// reads the clipboard at a moment the user did not choose, so the decision stands and this
// is the door it left open.
//
// # ⚠ Where this does *not* mount
//
// A board opened from a static `./board.bin` has `blobBase === './blobs/'` — a directory of
// files, with no route behind it. There is nowhere to upload to, so the feature declines to
// mount at all rather than offering a picker that fails at the last step. `index.html` gates
// on the same thing it gates the toolbar on.

/// The largest picture the server will take, from `crates/velmd/src/blobs.rs`.
///
/// ⚠ Checked here as well as there, and the reason is what the failure looks like otherwise:
/// the route refuses from the `Content-Length` before it reads a byte, so a 90 MB drop would
/// spend the whole upload and then report a number. Refusing locally costs nothing and can
/// say the size.
const MAX_PICTURE_BYTES = 64 * 1024 * 1024;

/// How long a message stays up.
const MESSAGE_MS = 4_000;

const STYLE_ID = 'velm-pictures-style';

const CSS = `
.velm-pictures-note {
  position: fixed;
  left: 50%;
  transform: translateX(-50%);
  bottom: calc(max(12px, env(safe-area-inset-bottom)) + 12px);
  max-width: min(420px, calc(100vw - 24px));
  box-sizing: border-box;
  padding: 10px 14px;
  border-radius: 10px;
  border: 1px solid #E2E7EA;
  background: #fff;
  color: #22282C;
  font: 13px/1.45 -apple-system, BlinkMacSystemFont, "Segoe UI", Inter, sans-serif;
  box-shadow: 0 1px 2px rgba(26, 29, 31, .06), 0 2px 8px rgba(26, 29, 31, .08);
  /* The board runs underneath it — tools.js's rule, and this one takes nothing back
     because there is nothing here to press. */
  pointer-events: none;
  -webkit-user-select: none;
  user-select: none;
}
.velm-pictures-note[hidden] { display: none; }
/* ⚠ Off-screen and not display:none. A file input that is not rendered cannot be opened by
   click() in Safari, which is the one browser this client exists for. */
.velm-pictures-file {
  position: fixed;
  width: 1px;
  height: 1px;
  opacity: 0;
  pointer-events: none;
  left: -9999px;
  top: 0;
}
`;

/// The surfaces currently on the page, so a second `mountPictures` can take the first down.
///
/// Idempotent for `chrome.js`'s reason and with its stakes: this installs a window `paste`
/// listener, so a second mount without this leaves the first one uploading as well and one
/// ⌘V puts the picture on the board twice.
let mounted = null;

function ensureStyle(doc) {
  if (doc.getElementById(STYLE_ID)) return;
  const style = doc.createElement('style');
  style.id = STYLE_ID;
  style.textContent = CSS;
  doc.head.append(style);
}

/// The name each verb is exported under, in the order they are tried.
///
/// `tools.js`'s table and its reason: the two halves of this feature were written together
/// and could still disagree about a prefix, and guessing produces a build where the control
/// is drawn and the function behind it exists.
const EXPORTS = {
  picture_hash: ['velm_picture_hash', 'picture_hash'],
  place_picture: ['velm_place_picture', 'place_picture'],
  note_paste_aim: ['velm_note_paste_aim', 'note_paste_aim'],
};

/** The name this module actually exports for a verb, or `null`. */
function bound(mod, name) {
  for (const candidate of EXPORTS[name] || [name]) {
    if (typeof mod[candidate] === 'function') return candidate;
  }
  return null;
}

/**
 * The first image in a paste or a drop, or `null`.
 *
 * ⚠ **`files` first and `items` second, because Safari fills only one of them.** A screenshot
 * pasted on macOS arrives as a `File` in `clipboardData.files`; some builds put it only in
 * `items` with `kind === 'file'`. Reading both is two lines and is the difference between
 * "pasting a picture works" and "pasting a picture works on my machine".
 */
function pictureIn(data) {
  if (!data) return null;
  const files = data.files;
  if (files) {
    for (const file of files) {
      if (file && typeof file.type === 'string' && file.type.startsWith('image/')) return file;
    }
  }
  const items = data.items;
  if (items) {
    for (const item of items) {
      if (item && item.kind === 'file' && typeof item.type === 'string'
        && item.type.startsWith('image/')) {
        const file = item.getAsFile();
        if (file) return file;
      }
    }
  }
  return null;
}

/**
 * A picture's own pixel size, or `null` when it will not decode.
 *
 * The bitmap is closed explicitly rather than left to the collector — `images.rs` states the
 * same rule for the same reason: a decoded bitmap holds pixels outside the heap, and a
 * screenshot off a 5K display is 59 MB of them.
 */
async function sizeOf(win, blob) {
  if (typeof win.createImageBitmap !== 'function') return null;
  try {
    const bitmap = await win.createImageBitmap(blob);
    const size = { width: bitmap.width, height: bitmap.height };
    bitmap.close();
    return size.width > 0 && size.height > 0 ? size : null;
  } catch {
    return null;
  }
}

/**
 * Mounts the picture path: a window `paste` listener and a file picker for the Image tool.
 *
 * `blobBase` and `blobSuffix` are the two halves `index.html` already built for the viewer —
 * a blob's URL is `blobBase + hash + blobSuffix`. They are used **verbatim**; see the header.
 *
 * Returns `{ pick, take, note, unmount }`, or `null` when the module cannot place a picture
 * or there is nowhere to upload to. A `null` here is what tells `index.html` to leave the
 * Image tool saying what it has always said rather than offering a picker that cannot finish.
 */
export function mountPictures(mod, { canvas, blobBase, blobSuffix = '', document: docOption } = {}) {
  if (!mod || !canvas) return null;
  const doc = docOption || canvas.ownerDocument;
  if (!doc) return null;
  const win = doc.defaultView || (typeof window !== 'undefined' ? window : null);
  if (!win) return null;

  const hashFn = bound(mod, 'picture_hash');
  const placeFn = bound(mod, 'place_picture');
  if (!hashFn || !placeFn) return null;
  // A route, not a directory. See the header: a static board has nowhere to put bytes.
  if (typeof blobBase !== 'string' || !blobBase.startsWith('/api/')) return null;

  if (mounted) mounted.unmount();
  ensureStyle(doc);

  const note = doc.createElement('div');
  note.className = 'velm-pictures-note';
  note.setAttribute('role', 'status');
  note.hidden = true;

  // ⚠ `accept` narrows the picker and does not enforce anything — a person can still choose
  // "All files" in the system dialog. `take` checks the type itself, which is the check that
  // counts.
  const file = doc.createElement('input');
  file.className = 'velm-pictures-file';
  file.type = 'file';
  file.accept = 'image/*';

  const state = { live: true, timer: 0, busy: false };

  const say = (text) => {
    if (!state.live) return;
    // `textContent`, never `innerHTML`: this shows a filename and a server's own sentence,
    // and both are strings somebody else wrote. `find.js` states the rule.
    note.textContent = text;
    note.hidden = !text;
    win.clearTimeout(state.timer);
    if (text) state.timer = win.setTimeout(() => { note.hidden = true; }, MESSAGE_MS);
  };

  /**
   * Hash, upload, place. The one path both doors lead to.
   *
   * ⚠ **One at a time.** `state.busy` is not politeness: two 20 MB uploads racing on a phone
   * is where this feature would be blamed for the tab dying, and the second ⌘V of a
   * double-press is the common way to start one.
   */
  const take = async (picture) => {
    if (!state.live || !picture) return false;
    if (typeof picture.type !== 'string' || !picture.type.startsWith('image/')) {
      say('That is not a picture.');
      return false;
    }
    if (state.busy) {
      say('Still sending the last picture.');
      return false;
    }
    if (picture.size > MAX_PICTURE_BYTES) {
      const mb = Math.round(picture.size / (1024 * 1024));
      say(`That picture is ${mb} MB. The limit is 64 MB.`);
      return false;
    }

    state.busy = true;
    try {
      say('Sending the picture…');
      const bytes = new Uint8Array(await picture.arrayBuffer());
      if (!bytes.length) {
        say('That picture is empty.');
        return false;
      }
      // The name the server will check the bytes against. See `pictures.rs`: the client names
      // it so a body corrupted in transit is a 400 rather than permanent orphaned disk.
      const hash = mod[hashFn](bytes);
      if (typeof hash !== 'string' || hash.length !== 64) {
        say('Could not name that picture.');
        return false;
      }

      // Measured before the upload so a picture that will not decode is refused before it
      // costs bandwidth — and so the item, when it is written, is the shape the picture is.
      const size = await sizeOf(win, picture);

      const response = await win.fetch(`${blobBase}${hash}${blobSuffix}`, {
        method: 'POST',
        // ⚠ Required by the route, and a cross-site request forgery defence rather than a
        // formality — `blobs.rs` argues it at length. A plain HTML form cannot send this
        // type, so a forged upload cannot be built out of markup.
        headers: { 'content-type': 'application/octet-stream' },
        // The session cookie is `SameSite=Strict` and this is same-origin, so it travels; a
        // token board carries its token in `blobSuffix` instead. Named rather than left to
        // the default, because the default is what a later same-origin change would silently
        // alter.
        credentials: 'same-origin',
        body: bytes,
      });
      // 409 is the server saying it already has these bytes, which is content addressing
      // working rather than a refusal — `pictures.rs` reads it the same way.
      if (!response.ok && response.status !== 409) {
        say(uploadFailure(response.status));
        return false;
      }

      const placed = mod[placeFn](hash, size ? size.width : 0, size ? size.height : 0);
      if (placed === false) {
        say('The picture was sent but could not be placed on this board.');
        return false;
      }
      say('Picture added.');
      return true;
    } catch (error) {
      // ⚠ Guarded whole. `panic = "abort"` poisons a wasm module, so once one export throws
      // every export throws — and this is the moment the page is least able to say so.
      // `find.js` records the same trap on its search.
      say(`Could not add that picture: ${error && error.message ? error.message : error}`);
      return false;
    } finally {
      state.busy = false;
    }
  };

  /**
   * ⌘V with a picture on the clipboard.
   *
   * ⚠ **It stands down for a field.** `tools.js` puts an off-screen `textarea` under the
   * on-canvas caret and gives it its own `paste` listener; that event bubbles to the window,
   * and answering it here as well would drop a picture on the board every time somebody
   * pasted a word into a sticky. The same test covers the find field and any input a later
   * surface adds.
   */
  const onPaste = (event) => {
    if (!state.live || event.defaultPrevented) return;
    const target = event.target;
    const tag = target && target.tagName ? target.tagName.toLowerCase() : '';
    if (tag === 'input' || tag === 'textarea' || (target && target.isContentEditable)) return;
    const picture = pictureIn(event.clipboardData);
    if (!picture) return;
    // Claimed only once there is a picture in it, so a paste carrying text still reaches
    // `velm_paste` and this tab's own clipboard.
    event.preventDefault();
    void take(picture);
  };

  /**
   * Dropping a picture on the board.
   *
   * ⚠ **`dragover` must be prevented or `drop` never fires**, and the browser navigates to
   * the file instead — which on a board that is live-synced means leaving the page. That is
   * why there are two listeners here for one gesture.
   */
  const onDragOver = (event) => {
    if (!state.live) return;
    if (!event.dataTransfer) return;
    event.preventDefault();
    event.dataTransfer.dropEffect = 'copy';
  };
  const onDrop = (event) => {
    if (!state.live) return;
    const picture = pictureIn(event.dataTransfer);
    if (!picture) return;
    event.preventDefault();
    // Where it lands. The same door `tools.js` uses for a right-click paste, so a dropped
    // picture arrives under the pointer rather than in the middle of the screen.
    const aim = bound(mod, 'note_paste_aim');
    if (aim) mod[aim](event.clientX, event.clientY);
    void take(picture);
  };

  const onPicked = () => {
    const chosen = file.files && file.files[0];
    // Cleared so choosing the same file twice in a row fires `change` the second time.
    file.value = '';
    if (chosen) void take(chosen);
  };

  win.addEventListener('paste', onPaste);
  win.addEventListener('dragover', onDragOver);
  win.addEventListener('drop', onDrop);
  file.addEventListener('change', onPicked);

  canvas.insertAdjacentElement('afterend', note);
  doc.body.append(file);

  const session = {
    /** Open the system file dialog. What the Image tool runs. */
    pick() {
      if (!state.live) return;
      file.click();
    },
    /** Take one `File` or `Blob` straight through. For a fixture. */
    take,
    /** The last message shown, for a fixture. */
    note: () => (note.hidden ? '' : note.textContent),
    unmount() {
      if (!state.live) return;
      state.live = false;
      win.clearTimeout(state.timer);
      win.removeEventListener('paste', onPaste);
      win.removeEventListener('dragover', onDragOver);
      win.removeEventListener('drop', onDrop);
      file.removeEventListener('change', onPicked);
      note.remove();
      file.remove();
      if (mounted === session) mounted = null;
    },
  };
  mounted = session;
  return session;
}

/** What a refused upload is called, in a sentence rather than a number. */
function uploadFailure(status) {
  if (status === 401 || status === 403) return 'Sign in again to add a picture.';
  if (status === 413) return 'That picture is too large for this server.';
  if (status === 400) return 'The server did not accept that picture.';
  return `The server refused that picture (HTTP ${status}).`;
}
