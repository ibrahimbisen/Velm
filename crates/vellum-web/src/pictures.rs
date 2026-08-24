//! Putting a picture **on** the board from a browser tab.
//!
//! `images.rs` is the other direction — the pictures a board already names, fetched and
//! decoded for the GPU. This is the one that was missing: until it existed a tab could look
//! at every picture on a board and add none, and `tools.rs`'s `Tool::Image` said so out loud
//! (*"placing an image needs a file picker this build has not got"*) while `clip.rs` said the
//! system clipboard was out of reach entirely.
//!
//! # The four steps, and which half does each
//!
//! ```text
//!   1. the bytes      page      a paste event's file, or a file picker
//!   2. the name       wasm      velm_picture_hash  -> BLAKE3-256, hex
//!   3. the upload     page      POST /api/v1/blobs/{hash}
//!   4. the item       wasm      velm_place_picture -> ItemKind::Image
//! ```
//!
//! ⚠ **The item is written after the upload, never before, and the order is the whole point
//! of splitting this in two.** An item naming a hash the server has not got is exactly the
//! defect `crates/velmd/src/blobs.rs` was written to end and `crate::images` reports as
//! `Undecodable`: the browser draws nothing where the picture is, for ever, and no later
//! render can recover because the bytes were never sent. Adding the item last means a failed
//! upload leaves the board as it was.
//!
//! # Why the page uploads and this does not
//!
//! The page already holds the two halves of a blob URL — `crate::start` is given `blob_base`
//! and `blob_suffix`, and `blob_suffix` carries the token when there is one. Handing them
//! back into wasm to build the same string a second time is how the two derivations drift
//! apart on exactly the boards whose names are not identifiers, which `crate::boot` warns
//! about for the snapshot URL. So the page keeps the address and this keeps the document.
//!
//! # Why the hash is computed here and not on the server
//!
//! `blobs.rs` requires the client to name the hash and states why: `velmd` can never remove a
//! blob, so a body corrupted in transit and stored under whatever it happened to hash to is
//! permanent disk that nothing will ever reference. Naming it in advance turns that from a
//! silent orphan into a 400. A tab that wants to add a picture therefore has to be able to
//! compute a BLAKE3-256, which is why `blake3` is a dependency of this crate.

use wasm_bindgen::prelude::wasm_bindgen;

use vellum_doc::{ItemKind, Placement};
// ⚠ The sizing rule and the name check live in `vellum-project` and not here, and
// that is not tidiness: this crate is `#![cfg(target_arch = "wasm32")]`, so a test
// written beside them would never be compiled. `vellum_app::actions` places a
// pasted picture through the same two functions, which is what makes the Mac and a
// tab agree about how big a screenshot arrives.
use vellum_project::picture::{fitted, is_blob_hash};

/// The BLAKE3-256 of some bytes, as the 64 lowercase hex characters `velmd` names a blob by.
///
/// This is `vellum_store::Hash::of` with the store taken off — that type cannot come to wasm,
/// because `vellum-store` is SQLite — and the two must stay byte-identical or a picture added
/// in a tab is a second copy of one already on the Mac rather than the same blob.
///
/// Empty bytes answer the hash of nothing rather than an error. The refusal belongs at the
/// door, in [`velm_place_picture`] and in the page, where there is a sentence to show.
#[wasm_bindgen]
#[must_use]
pub fn velm_picture_hash(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

/// Adds a picture the page has already uploaded, centred where a paste would land.
///
/// `hash` is what [`velm_picture_hash`] answered and what the page has just stored under.
/// `width` and `height` are the picture's **own** pixels, from `createImageBitmap`; `0` for
/// either means the page could not decode it, and `vellum_project::picture` supplies a
/// fallback rather than refusing — a picture with no size is one nothing can select.
///
/// Answers `true` when an item was added. `false` covers every refusal, and each one is
/// logged with the reason:
///
/// - the viewer is not reachable, or a frame is holding it — *"not now"*, the same answer
///   every other export in this crate gives for a `try_borrow` collision;
/// - the board is read-only (a static `board.bin` has nowhere to upload to anyway);
/// - the hash is not 64 hex characters;
/// - the size is not a pair of finite numbers.
///
/// # ⚠ The hash is validated here even though the page just computed it
///
/// It becomes an `asset_id` in the document, and an `asset_id` is what `crate::images` puts
/// into a URL. `vellum_app::push::hashes_of` states the rule for the other end of the same
/// pipe: board content reaches a URL, so the parse is the whole defence. `wasm_bindgen` also
/// hands `undefined` across as an empty string, so *"called with no arguments"* arrives here
/// as a legitimate-looking call.
#[wasm_bindgen]
pub fn velm_place_picture(hash: &str, width: f64, height: f64) -> bool {
    if !is_blob_hash(hash) {
        log::warn!("velm pictures: refusing a name that is not a blob hash");
        return false;
    }
    let Some(held) = crate::edit::viewer() else { return false };
    let Ok(mut viewer) = held.try_borrow_mut() else { return false };
    // The item being typed into is not the item about to be added, but a caret holds an undo
    // group open over the whole session — and `add_one` opens one of its own. Ended first, or
    // the two nest and every grouped operation after it fails for the life of the tab. This
    // is `velm_delete_selection`'s rule, applied to the other kind of mutation.
    crate::caret::settle(&mut viewer);
    let at = crate::clip::aim_world(&viewer.camera);
    let crate::Viewer { edit, board, projection, push, .. } = &mut *viewer;
    if !edit.enabled() {
        log::warn!("velm pictures: this board is read-only");
        return false;
    }

    // `fitted` runs the reported size through `measured` itself, so an unmeasurable
    // picture and a `NaN` from a call with no arguments both land on the fallback here
    // rather than reaching the multiplication.
    let (w, h) = fitted(width, height);
    let placement = Placement::new(at.x, at.y, w, h);
    let kind = ItemKind::Image { asset_id: hash.to_owned(), crop: None };
    match crate::tools::add_one(board, projection, kind, placement) {
        Ok(doc) => {
            // ⚠ **After the projection has been rebuilt, never before.** `scene_id` reads the
            // projection and the projection does not know the new item until `add_one` has
            // resettled it — `tools.rs`'s own warning, and the same failure: an id looked up
            // too early is `None`, so the picture arrives unselected and cannot be moved
            // without hunting for it.
            if let Some(scene) = projection.scene_id(doc) {
                edit.select_only(scene);
            }
            edit.sync(projection);
            crate::tools::finish(true, board, push);
            true
        }
        Err(error) => {
            log::error!("velm pictures: {error}");
            false
        }
    }
}
