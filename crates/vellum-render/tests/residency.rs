//! Residency: what stays on the GPU, at what size, and what goes when it will not
//! all fit.
//!
//! `docs/01-architecture.md` §3 makes this the difference between the reference board
//! opening and not — 205 images decoded to RGBA exceed 1.5 GB against a 400 MB idle
//! budget. The policy is unit-tested against a GPU-free model in `src/texture.rs`;
//! these tests drive the real device, so the accounting, the downscale and the
//! bind-group lifetime are exercised as they will be at run time.

mod common;

use vellum_render::{
    AtlasConfig, DetailPolicy, DrawList, GlyphAtlas, ImageInstance, ImageSource, RenderError,
    Renderer, TextureBudget, TextureId, TextureManager, UvRect, View,
};
use vellum_scene::{Camera, ScreenPoint, ScreenSize};
use vellum_text::{GlyphContent, GlyphImage, GlyphKey, LayoutParams, StyledText, TextEngine};

/// An opaque white image of `size` × `size`.
fn image(size: u32) -> Vec<u8> {
    vec![255u8; (size * size * 4) as usize]
}

/// An opaque red image of `size` × `size`. Distinguishable from the harness's black
/// clear colour *and* from white, so a demoted texture that lost its contents in the
/// GPU copy shows up as a wrong pixel rather than as a plausible one.
fn red(size: u32) -> Vec<u8> {
    [255u8, 0, 0, 255].repeat((size * size) as usize)
}

/// Every level of a mip chain, in bytes, for a `size` × `size` RGBA image.
fn chain_bytes(mut size: u32) -> usize {
    let mut total = 0usize;
    loop {
        total += (size * size * 4) as usize;
        if size == 1 {
            return total;
        }
        size = (size / 2).max(1);
    }
}

#[test]
fn uploading_downscales_to_the_budgets_maximum_dimension() {
    let Some((device, queue)) = common::gpu() else {
        return common::skipped("the texture residency check");
    };
    let mut textures = TextureManager::new(
        device,
        TextureBudget { max_bytes: 64 << 20, max_dimension: 128 },
    );

    let pixels = image(512);
    let id = textures
        .upload(device, queue, &ImageSource::new(512, 512, &pixels))
        .expect("upload");

    // 512 halves twice to reach 128.
    assert_eq!(textures.size(id), Some((128, 128)));
    assert_eq!(textures.resident_bytes(), chain_bytes(128));
    assert!(textures.bind_group(id).is_some());
    assert!(textures.contains(id));
}

#[test]
fn an_image_within_the_cap_is_stored_at_full_size() {
    let Some((device, queue)) = common::gpu() else {
        return common::skipped("the texture residency check");
    };
    let mut textures = TextureManager::new(device, TextureBudget::default());
    let pixels = image(64);
    let id = textures
        .upload(device, queue, &ImageSource::new(64, 64, &pixels))
        .expect("upload");
    assert_eq!(textures.size(id), Some((64, 64)));
}

#[test]
fn a_buffer_that_does_not_match_its_dimensions_is_refused() {
    let Some((device, queue)) = common::gpu() else {
        return common::skipped("the texture residency check");
    };
    let mut textures = TextureManager::new(device, TextureBudget::default());
    let result = textures.upload(device, queue, &ImageSource::new(8, 8, &[0; 16]));
    assert!(matches!(result, Err(RenderError::ImageSize { expected: 256, actual: 16, .. })));
    assert!(textures.is_empty());
    assert_eq!(textures.resident_bytes(), 0);
}

/// The policy, end to end: an image the user has stopped looking at goes before one
/// they have not, and one still on screen never goes at all.
#[test]
fn eviction_takes_the_stale_and_distant_and_spares_what_is_on_screen() {
    let Some((device, queue)) = common::gpu() else {
        return common::skipped("the texture residency check");
    };
    // Room for two of the three 64px images (5,460 bytes each with mips).
    let budget = TextureBudget { max_bytes: chain_bytes(64) * 2, max_dimension: 2048 };
    let mut textures = TextureManager::new(device, budget);

    let pixels = image(64);
    let source = ImageSource::new(64, 64, &pixels);
    let on_screen = textures.upload(device, queue, &source).expect("upload");
    let near = textures.upload(device, queue, &source).expect("upload");
    let far = textures.upload(device, queue, &source).expect("upload");
    assert_eq!(textures.len(), 3);
    assert!(textures.resident_bytes() > budget.max_bytes);

    // Frame 1: everything is seen, at different distances.
    textures.begin_frame();
    textures.mark(on_screen, 0.0);
    textures.mark(near, 800.0);
    textures.mark(far, 40_000.0);
    assert!(
        textures.evict_to_budget().is_empty(),
        "nothing seen this frame may be evicted"
    );
    assert!(textures.resident_bytes() > budget.max_bytes, "and so it stays over budget");

    // Frame 2: only the on-screen one is still in view.
    textures.begin_frame();
    textures.mark(on_screen, 0.0);
    let evicted = textures.evict_to_budget();

    assert_eq!(evicted, vec![far], "the furthest of the stale pair goes first");
    assert!(textures.contains(on_screen));
    assert!(textures.contains(near));
    assert!(!textures.contains(far));
    assert!(textures.bind_group(far).is_none());
    assert_eq!(textures.resident_bytes(), chain_bytes(64) * 2);
}

#[test]
fn a_budget_that_is_already_met_evicts_nothing() {
    let Some((device, queue)) = common::gpu() else {
        return common::skipped("the texture residency check");
    };
    let mut textures = TextureManager::new(device, TextureBudget::default());
    let pixels = image(32);
    textures
        .upload(device, queue, &ImageSource::new(32, 32, &pixels))
        .expect("upload");
    textures.begin_frame();
    assert!(textures.evict_to_budget().is_empty());
    assert_eq!(textures.len(), 1);
}

#[test]
fn removing_a_texture_returns_its_bytes_to_the_budget() {
    let Some((device, queue)) = common::gpu() else {
        return common::skipped("the texture residency check");
    };
    let mut textures = TextureManager::new(device, TextureBudget::default());
    let pixels = image(32);
    let id = textures
        .upload(device, queue, &ImageSource::new(32, 32, &pixels))
        .expect("upload");

    assert!(textures.remove(id));
    assert!(!textures.remove(id), "removing twice is not an error, only a no-op");
    assert_eq!(textures.resident_bytes(), 0);
    assert!(textures.is_empty());
}

/// Distinct, real glyph keys. Only `vellum-text` can mint one, and the atlas is keyed
/// by them, so the tests borrow keys from a real shaping pass and supply their own
/// bitmaps — which keeps the assertions about packing rather than about whichever
/// face the platform happens to provide.
fn keys(count: usize) -> Option<Vec<GlyphKey>> {
    let mut engine = TextEngine::new().ok()?;
    let alphabet: String = ('a'..='z').chain('0'..='9').collect();
    let layout = engine.layout(
        &StyledText::plain(&alphabet),
        &LayoutParams { font_size: 32.0, ..LayoutParams::default() },
    );
    let mut seen = Vec::new();
    for glyph in layout.glyphs() {
        let key = glyph.physical((0.0, 0.0), 1.0).key;
        if !seen.contains(&key) {
            seen.push(key);
        }
    }
    (seen.len() >= count).then(|| seen.into_iter().take(count).collect())
}

fn coverage_glyph(size: u32) -> GlyphImage {
    GlyphImage {
        content: GlyphContent::Coverage,
        width: size,
        height: size,
        left: 0,
        top: size as i32,
        data: vec![255; (size * size) as usize],
    }
}

#[test]
fn the_atlas_finds_every_glyph_it_was_prepared_with() {
    let Some((device, queue)) = common::gpu() else {
        return common::skipped("the glyph atlas check");
    };
    let Some(keys) = keys(16) else {
        return eprintln!("no system fonts; skipping the glyph atlas check");
    };
    let mut atlas = GlyphAtlas::new(device, AtlasConfig::default());

    let entries: Vec<_> = keys.iter().map(|&k| (k, coverage_glyph(12))).collect();
    atlas.prepare(device, queue, &entries).expect("16 glyphs must fit");

    assert_eq!(atlas.len(), 16);
    for key in &keys {
        let slot = atlas.slot(*key).expect("resident");
        assert_eq!((slot.width, slot.height), (12, 12));
        assert!(atlas.bind_group(slot.page).is_some());
        // The UV rect covers exactly the slot on a 1024px page.
        assert!((slot.uv_max[0] - slot.uv_min[0] - 12.0 / 1024.0).abs() < 1e-6);
    }
    assert_eq!(atlas.pages(vellum_render::AtlasKind::Coverage), 1);
}

/// A space has no ink and must not take a slot — a board of stickies is a third
/// whitespace, and reserving zero-area rectangles for it would churn the packer for
/// nothing.
#[test]
fn a_blank_glyph_takes_no_slot() {
    let Some((device, queue)) = common::gpu() else {
        return common::skipped("the glyph atlas check");
    };
    let Some(keys) = keys(2) else {
        return eprintln!("no system fonts; skipping the glyph atlas check");
    };
    let mut atlas = GlyphAtlas::new(device, AtlasConfig::default());

    let blank = GlyphImage {
        content: GlyphContent::Coverage,
        width: 0,
        height: 0,
        left: 0,
        top: 0,
        data: Vec::new(),
    };
    atlas.prepare(device, queue, &[(keys[0], blank)]).expect("a blank glyph is not an error");
    assert!(atlas.is_empty());
    assert!(atlas.slot(keys[0]).is_none());
    // But it is remembered as blank, so a caller can tell a space from a hole in the
    // atlas — a glyph never prepared at all is not blank, it is missing.
    assert!(atlas.is_blank(keys[0]));
    assert!(!atlas.is_blank(keys[1]));
}

/// The two formats cannot share a page, because one is tinted by the run's colour and
/// the other must not be.
#[test]
fn colour_and_coverage_glyphs_land_on_separate_pages() {
    use vellum_render::AtlasKind;

    let Some((device, queue)) = common::gpu() else {
        return common::skipped("the glyph atlas check");
    };
    let Some(keys) = keys(2) else {
        return eprintln!("no system fonts; skipping the glyph atlas check");
    };
    let mut atlas = GlyphAtlas::new(device, AtlasConfig::default());

    let emoji = GlyphImage {
        content: GlyphContent::Color,
        width: 8,
        height: 8,
        left: 0,
        top: 8,
        data: vec![200; 8 * 8 * 4],
    };
    atlas
        .prepare(device, queue, &[(keys[0], coverage_glyph(8)), (keys[1], emoji)])
        .expect("both must fit");

    let letter = atlas.slot(keys[0]).expect("resident").page;
    let emoji = atlas.slot(keys[1]).expect("resident").page;
    assert_eq!(letter.kind, AtlasKind::Coverage);
    assert_eq!(emoji.kind, AtlasKind::Color);
    assert_ne!(letter, emoji);
    assert_eq!(atlas.pages(AtlasKind::Coverage), 1);
    assert_eq!(atlas.pages(AtlasKind::Color), 1);
}

/// The eviction path. A single 64px page holds sixteen 16px glyphs; asking for
/// sixteen more in the next frame must repack rather than fail, dropping exactly the
/// glyphs the new frame does not use.
#[test]
fn a_full_atlas_repacks_and_drops_the_glyphs_the_frame_does_not_use() {
    let Some((device, queue)) = common::gpu() else {
        return common::skipped("the glyph atlas check");
    };
    let Some(keys) = keys(32) else {
        return eprintln!("no system fonts; skipping the glyph atlas check");
    };
    let mut atlas = GlyphAtlas::new(device, AtlasConfig { page_size: 64, max_pages: 1 });

    let batch = |range: std::ops::Range<usize>| -> Vec<(GlyphKey, GlyphImage)> {
        keys[range].iter().map(|&k| (k, coverage_glyph(16))).collect()
    };

    atlas.begin_frame();
    atlas.prepare(device, queue, &batch(0..16)).expect("sixteen 16px glyphs fill one 64px page");
    assert_eq!(atlas.len(), 16);

    atlas.begin_frame();
    atlas.prepare(device, queue, &batch(16..32)).expect("the second frame must repack");

    assert_eq!(atlas.len(), 16, "the first frame's glyphs were dropped, not kept");
    for key in &keys[16..32] {
        assert!(atlas.slot(*key).is_some(), "this frame's glyphs are all resident");
    }
    for key in &keys[0..16] {
        assert!(atlas.slot(*key).is_none(), "last frame's glyphs are gone");
    }
    assert_eq!(atlas.pages(vellum_render::AtlasKind::Coverage), 1, "no page was added");
}

/// Glyphs still in use this frame survive a repack, because the repack keeps whatever
/// the current frame has already touched.
#[test]
fn a_repack_keeps_the_glyphs_the_current_frame_still_needs() {
    let Some((device, queue)) = common::gpu() else {
        return common::skipped("the glyph atlas check");
    };
    let Some(keys) = keys(20) else {
        return eprintln!("no system fonts; skipping the glyph atlas check");
    };
    let mut atlas = GlyphAtlas::new(device, AtlasConfig { page_size: 64, max_pages: 1 });

    atlas.begin_frame();
    let first: Vec<_> = keys[0..8].iter().map(|&k| (k, coverage_glyph(16))).collect();
    atlas.prepare(device, queue, &first).expect("eight fit easily");

    atlas.begin_frame();
    // Touch four of the first eight, then ask for twelve more — which needs a repack.
    let mixed: Vec<_> = keys[0..4]
        .iter()
        .chain(&keys[8..20])
        .map(|&k| (k, coverage_glyph(16)))
        .collect();
    atlas.prepare(device, queue, &mixed).expect("sixteen 16px glyphs fit after a repack");

    for key in &keys[0..4] {
        assert!(atlas.slot(*key).is_some(), "a glyph used this frame survived");
    }
    for key in &keys[4..8] {
        assert!(atlas.slot(*key).is_none(), "a glyph not used this frame did not");
    }
    assert_eq!(atlas.len(), 16);
}

/// More glyphs than the atlas can ever hold is a configuration error, and has to be
/// reported rather than silently drawing text with holes in it.
#[test]
fn an_atlas_too_small_for_the_frame_reports_it() {
    let Some((device, queue)) = common::gpu() else {
        return common::skipped("the glyph atlas check");
    };
    let Some(keys) = keys(32) else {
        return eprintln!("no system fonts; skipping the glyph atlas check");
    };
    let mut atlas = GlyphAtlas::new(device, AtlasConfig { page_size: 64, max_pages: 1 });

    let entries: Vec<_> = keys.iter().map(|&k| (k, coverage_glyph(24))).collect();
    let result = atlas.prepare(device, queue, &entries);
    assert!(
        matches!(result, Err(RenderError::AtlasFull { page_size: 64, pages: 1, .. })),
        "{result:?}"
    );
}

// ---------------------------------------------------------------------------
// Resolution: matching a texture's size to the size it is drawn at.
//
// The measured defect these exist for: the reference board at fit-zoom held
// 785,935,916 bytes against a 268,435,456 byte budget with *nothing evictable*,
// because all 122 images were on screen at once. Eviction had the wrong question.
// ---------------------------------------------------------------------------

/// A screen-pixel view over the harness's target, which is the space these tests
/// measure in: one unit is one device pixel, so an instance's size *is* its drawn
/// size and the arithmetic under test is not hidden behind a camera.
fn screen_view() -> View {
    View::screen(ScreenSize::new(f64::from(common::TARGET), f64::from(common::TARGET)))
}

fn one_image(id: TextureId, size: f32) -> DrawList {
    let mut list = DrawList::new();
    list.view(screen_view());
    list.push_image(id, ImageInstance::new([0.0, 0.0], [size, size], UvRect::FULL));
    list
}

/// The fix, end to end and through the real pipeline: an image drawn at 64 px stops
/// paying for 512 px of texels, keeps its handle, and still draws the same colour.
///
/// The colour is the part worth the GPU. Demotion copies level *k* of the old texture
/// into level 0 of a new one and throws the old away; a wrong mip level, a missing
/// `COPY_SRC`, or a bind group left pointing at the freed texture would all still
/// produce a texture of the right *size*.
#[test]
fn an_image_drawn_small_is_rebuilt_small_and_still_draws_the_same_picture() {
    let Some((device, queue)) = common::gpu() else {
        return common::skipped("the image detail check");
    };
    let mut renderer = Renderer::new(device, common::FORMAT);

    let pixels = red(512);
    let id = renderer
        .textures_mut()
        .upload(device, queue, &ImageSource::new(512, 512, &pixels))
        .expect("upload");
    assert_eq!(renderer.textures().size(id), Some((512, 512)));
    let full = renderer.textures().resident_bytes();

    let list = one_image(id, f64::from(common::TARGET) as f32);
    renderer.begin_frame();
    let frame = common::render(device, queue, &mut renderer, &list);

    // 512 texels across 64 pixels is three levels of surplus; one is kept as slack.
    assert_eq!(renderer.textures().size(id), Some((128, 128)));
    assert!(renderer.textures().contains(id), "the handle survived the rebuild");
    assert!(
        renderer.textures().resident_bytes() * 8 < full,
        "{} bytes against {full}",
        renderer.textures().resident_bytes()
    );
    common::assert_near(common::pixel(&frame, 32, 32), [255, 0, 0, 255], 2, "the demoted image");

    // And it settles: a second frame at the same size moves nothing.
    renderer.begin_frame();
    let frame = common::render(device, queue, &mut renderer, &list);
    assert_eq!(renderer.textures().size(id), Some((128, 128)));
    common::assert_near(common::pixel(&frame, 32, 32), [255, 0, 0, 255], 2, "the settled image");
}

/// The other half. A texture that has been demoted and is then drawn large has to get
/// its detail back, and the only source of those texels is the caller's blob store —
/// so it is dropped, and the caller notices the handle is gone and re-uploads.
///
/// The frame that *decides* this must still draw the coarse texture, or zooming in
/// would flash a hole in every image at once. Hence the drop landing at the next
/// [`Renderer::begin_frame`] rather than immediately.
#[test]
fn an_image_drawn_larger_than_it_is_stored_is_dropped_for_re_upload() {
    let Some((device, queue)) = common::gpu() else {
        return common::skipped("the image detail check");
    };
    let mut renderer = Renderer::new(device, common::FORMAT);

    let pixels = red(512);
    let id = renderer
        .textures_mut()
        .upload(device, queue, &ImageSource::new(512, 512, &pixels))
        .expect("upload");

    renderer.begin_frame();
    common::render(device, queue, &mut renderer, &one_image(id, 64.0));
    assert_eq!(renderer.textures().size(id), Some((128, 128)));

    // Now the camera moves in and the same image wants 512 px again.
    renderer.begin_frame();
    common::render(device, queue, &mut renderer, &one_image(id, 512.0));
    assert!(
        renderer.textures().contains(id),
        "the frame that noticed still had something to draw"
    );

    let dropped = renderer.begin_frame();
    assert_eq!(dropped, vec![id], "and the next frame is where it goes");
    assert!(!renderer.textures().contains(id));
    assert_eq!(renderer.textures().resident_bytes(), 0);

    // Re-uploaded from the source, it comes back at its ceiling and stays there.
    let id = renderer
        .textures_mut()
        .upload(device, queue, &ImageSource::new(512, 512, &pixels))
        .expect("re-upload");
    common::render(device, queue, &mut renderer, &one_image(id, 512.0));
    assert_eq!(renderer.textures().size(id), Some((512, 512)));
    assert_eq!(renderer.textures().detail_ceiling(id), Some((512, 512)));
}

/// Refinement costs a decode, and the caller decodes on a per-frame budget. Asking
/// for twenty at once would spend the budget on the first few and hand back
/// placeholders for the rest, so the queue is drained a few per frame.
#[test]
fn refinement_is_paced_rather_than_dropping_everything_at_once() {
    let Some((device, queue)) = common::gpu() else {
        return common::skipped("the image detail check");
    };
    let mut textures = TextureManager::new(device, TextureBudget::default());
    textures.set_detail_policy(DetailPolicy { refinements_per_frame: 4, ..Default::default() });

    let pixels = image(256);
    let ids: Vec<_> = (0..6)
        .map(|_| {
            textures
                .upload(device, queue, &ImageSource::new(256, 256, &pixels))
                .expect("upload")
        })
        .collect();

    textures.begin_frame();
    for id in &ids {
        textures.note_drawn(*id, (16.0, 16.0));
    }
    assert_eq!(textures.resolve_detail(device, queue), 6, "all six shrink together");
    for id in &ids {
        assert_eq!(textures.size(*id), Some((32, 32)));
    }

    textures.begin_frame();
    for id in &ids {
        textures.note_drawn(*id, (256.0, 256.0));
    }
    textures.resolve_detail(device, queue);

    assert_eq!(textures.begin_frame().len(), 4);
    assert_eq!(textures.begin_frame().len(), 2);
    assert!(textures.begin_frame().is_empty());
    assert!(textures.is_empty());
}

/// Only what a frame actually drew is reconsidered. A texture off screen is
/// eviction's problem — demoting it on the strength of a draw it did not receive
/// would guarantee a re-upload the moment it is panned back into view.
#[test]
fn a_texture_the_frame_did_not_draw_is_left_alone() {
    let Some((device, queue)) = common::gpu() else {
        return common::skipped("the image detail check");
    };
    let mut textures = TextureManager::new(device, TextureBudget::default());
    let pixels = image(256);
    let drawn = textures
        .upload(device, queue, &ImageSource::new(256, 256, &pixels))
        .expect("upload");
    let off_screen = textures
        .upload(device, queue, &ImageSource::new(256, 256, &pixels))
        .expect("upload");

    textures.begin_frame();
    textures.note_drawn(drawn, (16.0, 16.0));
    textures.resolve_detail(device, queue);

    assert_eq!(textures.size(drawn), Some((32, 32)));
    assert_eq!(textures.size(off_screen), Some((256, 256)), "nobody asked about it");
}

/// The number the whole change exists for.
///
/// One image the size the board's largest are stored at, drawn at the size fit-zoom
/// gives it — about sixty device pixels — and then multiplied by the board's 122
/// images. Before, that product was 785,935,916 measured bytes against a
/// 268,435,456 byte budget with nothing evictable. It has to come in under budget
/// with room to spare, because the glyph atlas and the glass backdrops are in the
/// same 400 MB as well.
#[test]
fn the_reference_boards_images_fit_the_budget_at_fit_zoom() {
    let Some((device, queue)) = common::gpu() else {
        return common::skipped("the image detail check");
    };
    let budget = TextureBudget::default();
    let mut textures = TextureManager::new(device, budget);

    let pixels = image(2048);
    let id = textures
        .upload(device, queue, &ImageSource::new(2048, 2048, &pixels))
        .expect("upload");
    let before = textures.resident_bytes();

    textures.begin_frame();
    textures.note_drawn(id, (60.0, 60.0));
    textures.resolve_detail(device, queue);
    let after = textures.resident_bytes();

    assert_eq!(textures.size(id), Some((128, 128)));
    assert!(
        after * 122 < budget.max_bytes,
        "122 images at {after} bytes is {} against a {} byte budget",
        after * 122,
        budget.max_bytes
    );
    assert!(
        before * 122 > budget.max_bytes,
        "the test is not exercising the defect: {before} bytes each was already fine"
    );
}

/// The demand is measured in *texels of the whole image*, so a crop showing a corner
/// of a photo across the same screen rectangle needs a proportionally larger texture.
/// Getting this wrong stores every cropped image on the board too small, and the
/// error is invisible until someone looks at one.
#[test]
fn a_cropped_image_keeps_the_resolution_its_crop_needs() {
    let Some((device, queue)) = common::gpu() else {
        return common::skipped("the image detail check");
    };
    let mut renderer = Renderer::new(device, common::FORMAT);
    let pixels = red(1024);
    let id = renderer
        .textures_mut()
        .upload(device, queue, &ImageSource::new(1024, 1024, &pixels))
        .expect("upload");

    let mut list = DrawList::new();
    list.view(screen_view());
    // An eighth of the image, drawn across 64 px: that is 512 texels of the whole.
    list.push_image(
        id,
        ImageInstance::new([0.0, 0.0], [64.0, 64.0], UvRect::new([0.0, 0.0], [0.125, 0.125])),
    );

    renderer.begin_frame();
    common::render(device, queue, &mut renderer, &list);
    assert_eq!(
        renderer.textures().size(id),
        Some((1024, 1024)),
        "a 512-texel demand has only one level of surplus, which is the slack"
    );

    // The same rectangle without the crop asks for 64 texels, and now it may shrink.
    renderer.begin_frame();
    common::render(device, queue, &mut renderer, &one_image(id, 64.0));
    assert_eq!(renderer.textures().size(id), Some((128, 128)));
}

/// Through the board view, where `units_per_pixel` is `1/zoom` — the path the app
/// actually takes. A board-sized image at 4 % is sixty pixels of screen whatever its
/// world size says, and that is the number residency has to follow.
#[test]
fn the_board_view_zoom_is_what_decides_the_stored_size() {
    let Some((device, queue)) = common::gpu() else {
        return common::skipped("the image detail check");
    };
    let mut renderer = Renderer::new(device, common::FORMAT);
    let pixels = image(1024);
    let id = renderer
        .textures_mut()
        .upload(device, queue, &ImageSource::new(1024, 1024, &pixels))
        .expect("upload");
    let full = renderer.textures().resident_bytes();

    let mut camera = Camera::new(ScreenSize::new(1600.0, 900.0));
    camera.set_zoom_about(0.04, ScreenPoint::new(800.0, 450.0));
    let mut list = DrawList::new();
    list.view(View::board(&camera));
    // 1500 world px at 4 % is 60 device px.
    list.push_image(id, ImageInstance::new([0.0, 0.0], [1500.0, 1500.0], UvRect::FULL));

    renderer.begin_frame();
    common::render(device, queue, &mut renderer, &list);

    assert_eq!(renderer.textures().size(id), Some((128, 128)));
    assert!(renderer.textures().resident_bytes() * 30 < full);
    assert!(renderer.textures().peak_resident_bytes() >= full, "the peak is remembered");
}

/// The quality claim, checked rather than asserted in prose.
///
/// Demoting is supposed to be free of visible cost, and the reason is exact: level 1
/// of a chain rebuilt from level 2 of the original *is* level 3 of the original — the
/// same box filter applied the same number of times. The sampler was already reading
/// that level, so throwing the ones above it away changes which mip index it asks for
/// and nothing else. A gradient is used because a flat colour would survive any
/// filtering error, including sampling the wrong level entirely.
#[test]
fn a_demoted_texture_samples_the_same_as_the_full_one() {
    let Some((device, queue)) = common::gpu() else {
        return common::skipped("the image detail check");
    };

    const SIZE: u32 = 512;
    let mut pixels = Vec::with_capacity((SIZE * SIZE * 4) as usize);
    for y in 0..SIZE {
        for x in 0..SIZE {
            pixels.extend_from_slice(&[(x / 2) as u8, (y / 2) as u8, ((x + y) / 4) as u8, 255]);
        }
    }

    let off = DetailPolicy { demote_threshold_levels: u32::MAX, ..DetailPolicy::default() };
    let mut renderer = Renderer::new(device, common::FORMAT);
    renderer.textures_mut().set_detail_policy(off);
    let id = renderer
        .textures_mut()
        .upload(device, queue, &ImageSource::new(SIZE, SIZE, &pixels))
        .expect("upload");

    let list = one_image(id, f64::from(common::TARGET) as f32);
    renderer.begin_frame();
    let full = common::render(device, queue, &mut renderer, &list);
    assert_eq!(renderer.textures().size(id), Some((SIZE, SIZE)), "nothing demoted yet");

    renderer.textures_mut().set_detail_policy(DetailPolicy::default());
    renderer.begin_frame();
    let demoted = common::render(device, queue, &mut renderer, &list);
    assert_eq!(renderer.textures().size(id), Some((128, 128)));

    for y in 0..common::TARGET {
        for x in 0..common::TARGET {
            common::assert_near(
                common::pixel(&demoted, x, y),
                common::pixel(&full, x, y),
                1,
                &format!("texel {x},{y} after demotion"),
            );
        }
    }
}

/// Two prepares in one frame — a thumbnail and the window — must decide together.
/// If the small pass could act on its own, every offscreen render would demote the
/// board and the window would spend the next frames refining it all back.
#[test]
fn a_second_pass_in_one_frame_cannot_demote_what_the_first_needs() {
    let Some((device, queue)) = common::gpu() else {
        return common::skipped("the image detail check");
    };
    let mut renderer = Renderer::new(device, common::FORMAT);
    let pixels = image(512);
    let id = renderer
        .textures_mut()
        .upload(device, queue, &ImageSource::new(512, 512, &pixels))
        .expect("upload");

    renderer.begin_frame();
    // The window wants all 512 texels; the thumbnail, in the same frame, wants 16.
    renderer.prepare(device, queue, &one_image(id, 512.0));
    renderer.prepare(device, queue, &one_image(id, 16.0));
    assert_eq!(renderer.textures().size(id), Some((512, 512)));

    // Next frame the window asks again, and the thumbnail's observation does not win.
    renderer.begin_frame();
    renderer.prepare(device, queue, &one_image(id, 512.0));
    assert_eq!(renderer.textures().size(id), Some((512, 512)));
}
