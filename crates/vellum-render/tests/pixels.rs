//! Pixel tests: every pipeline drives a real adapter into an offscreen texture and
//! the framebuffer is read back and asserted on.
//!
//! Nothing else can check WGSL. A shader that never runs is a string, and the
//! mistakes that matter here — an edge half a pixel out, a UV rect sampling the wrong
//! quadrant, glyphs upside down, a rotation about the wrong point — all compile, all
//! validate, and all look plausible in a screenshot until something is beside them.

mod common;

use common::{FORMAT, TARGET, assert_near, pixel, render};
use vellum_render::{
    AtlasConfig, DrawList, ImageInstance, ImageSource, MeshTransform, QuadInstance, Renderer, Rgba,
    ShapeStyle, TextureBudget, UvRect, View,
};
use vellum_scene::{Camera, ScreenSize, WorldPoint};
use vellum_shapes::{Shape, Size};

const RED: Rgba = Rgba::new(1.0, 0.0, 0.0, 1.0);
const GREEN: Rgba = Rgba::new(0.0, 1.0, 0.0, 1.0);
const BLUE: Rgba = Rgba::new(0.0, 0.0, 1.0, 1.0);

const OPAQUE_RED: [u8; 4] = [255, 0, 0, 255];
const OPAQUE_GREEN: [u8; 4] = [0, 255, 0, 255];
const OPAQUE_BLUE: [u8; 4] = [0, 0, 255, 255];
const CLEAR: [u8; 4] = [0, 0, 0, 255];

/// The whole viewport in physical pixels, which is the space every test but the
/// camera one draws in.
fn screen() -> View {
    View::screen(ScreenSize::new(f64::from(TARGET), f64::from(TARGET)))
}

fn renderer(device: &wgpu::Device) -> Renderer {
    Renderer::new(device, FORMAT)
}

/// The foundation every other test rests on: the unit quad derived from the vertex
/// index covers exactly the instance's rect, the clip transform puts `(0,0)` at the
/// *top* left rather than the bottom, and per-instance colour reaches the fragment
/// stage.
///
/// The edges are asserted to the pixel. Antialiasing must not soften an axis-aligned,
/// pixel-aligned edge — the coverage ramp is exact there — and if it did, every
/// adjacent rectangle on a board would show a seam.
#[test]
fn quad_edges_land_on_the_pixel_they_were_given() {
    let Some((device, queue)) = common::gpu() else {
        return common::skipped("the offscreen render check");
    };
    let mut renderer = renderer(device);

    let half = (TARGET / 2) as f32;
    let mut list = DrawList::new();
    list.view(screen());
    list.push_quad(QuadInstance::solid([0.0, 0.0], [half, half], RED));
    list.push_quad(QuadInstance::solid([half, half], [half, half], BLUE));

    let pixels = render(device, queue, &mut renderer, &list);
    let (quarter, three_quarters) = (TARGET / 4, TARGET * 3 / 4);

    assert_eq!(pixel(&pixels, quarter, quarter), OPAQUE_RED);
    assert_eq!(pixel(&pixels, three_quarters, three_quarters), OPAQUE_BLUE);
    // The other two quadrants were never covered. Were the y axis flipped, they would
    // hold the quads instead.
    assert_eq!(pixel(&pixels, three_quarters, quarter), CLEAR);
    assert_eq!(pixel(&pixels, quarter, three_quarters), CLEAR);

    // And the boundary is where it was asked for, not a texel either side.
    assert_eq!(pixel(&pixels, 0, 0), OPAQUE_RED);
    assert_eq!(pixel(&pixels, TARGET / 2 - 1, TARGET / 2 - 1), OPAQUE_RED);
    assert_eq!(pixel(&pixels, TARGET / 2, TARGET / 2 - 1), CLEAR);
}

/// A corner radius has to remove the corner. The centre of the arc is `r` in from
/// both edges, so the extreme corner texel is outside the shape by `r(√2 − 1)`.
#[test]
fn a_corner_radius_cuts_the_corner_away() {
    let Some((device, queue)) = common::gpu() else {
        return common::skipped("the offscreen render check");
    };
    let mut renderer = renderer(device);

    let mut list = DrawList::new();
    list.view(screen());
    list.push_quad(
        QuadInstance::solid([0.0, 0.0], [TARGET as f32, TARGET as f32], GREEN)
            .with_corner_radius(20.0),
    );

    let pixels = render(device, queue, &mut renderer, &list);
    assert_eq!(pixel(&pixels, TARGET / 2, TARGET / 2), OPAQUE_GREEN, "the middle is filled");
    assert_eq!(pixel(&pixels, 0, 0), CLEAR, "the top-left corner is rounded away");
    assert_eq!(pixel(&pixels, TARGET - 1, 0), CLEAR);
    assert_eq!(pixel(&pixels, 0, TARGET - 1), CLEAR);
    assert_eq!(pixel(&pixels, TARGET - 1, TARGET - 1), CLEAR);
    // A point on the straight part of an edge is still filled.
    assert_eq!(pixel(&pixels, TARGET / 2, 0), OPAQUE_GREEN);
}

/// The border is drawn *inside* the shape's edge — CSS `border-box`, and what Miro
/// does — so a bordered item never overflows the bounds the scene layer culls by.
#[test]
fn a_border_is_drawn_inside_the_edge() {
    let Some((device, queue)) = common::gpu() else {
        return common::skipped("the offscreen render check");
    };
    let mut renderer = renderer(device);

    let mut list = DrawList::new();
    list.view(screen());
    list.push_quad(
        QuadInstance::solid([16.0, 16.0], [32.0, 32.0], BLUE).with_border(RED, 6.0),
    );

    let pixels = render(device, queue, &mut renderer, &list);
    assert_eq!(pixel(&pixels, 32, 17), OPAQUE_RED, "1px inside the top edge is border");
    assert_eq!(pixel(&pixels, 32, 21), OPAQUE_RED, "5px inside is still border");
    assert_eq!(pixel(&pixels, 32, 24), OPAQUE_BLUE, "8px inside is fill");
    assert_eq!(pixel(&pixels, 32, 15), CLEAR, "nothing is drawn outside the rect");
    assert_eq!(pixel(&pixels, 17, 32), OPAQUE_RED, "the left edge too");
}

/// Rotation is about the rect's centre. A 90° turn of a wide, short rect must produce
/// a tall, narrow one in the same place — rotating about the origin instead would
/// throw it off screen, and rotating about the top-left would offset it by half its
/// diagonal.
#[test]
fn a_rotated_quad_turns_about_its_centre() {
    let Some((device, queue)) = common::gpu() else {
        return common::skipped("the offscreen render check");
    };
    let mut renderer = renderer(device);

    // 40 x 8, centred on the target.
    let upright = QuadInstance::solid([12.0, 28.0], [40.0, 8.0], GREEN);

    let mut list = DrawList::new();
    list.view(screen());
    list.push_quad(upright);
    let before = render(device, queue, &mut renderer, &list);
    assert_eq!(pixel(&before, 32, 32), OPAQUE_GREEN);
    assert_eq!(pixel(&before, 16, 32), OPAQUE_GREEN, "wide before the turn");
    assert_eq!(pixel(&before, 32, 16), CLEAR, "and short");

    let mut list = DrawList::new();
    list.view(screen());
    list.push_quad(upright.with_rotation(std::f32::consts::FRAC_PI_2));
    let after = render(device, queue, &mut renderer, &list);
    assert_eq!(pixel(&after, 32, 32), OPAQUE_GREEN, "still centred");
    assert_eq!(pixel(&after, 32, 16), OPAQUE_GREEN, "tall after the turn");
    assert_eq!(pixel(&after, 16, 32), CLEAR, "and narrow");
}

/// The analytic ellipse: filled at the centre and at both extremes of its axes, empty
/// at the corners of its box. This is the sign of `sd_ellipse`, which the crate docs
/// promise is exact even though the magnitude is approximate.
#[test]
fn an_analytic_ellipse_fills_its_box_but_not_its_corners() {
    let Some((device, queue)) = common::gpu() else {
        return common::skipped("the offscreen render check");
    };
    let mut renderer = renderer(device);

    let params = Shape::Ellipse
        .sdf_params(Size::new(TARGET as f32, TARGET as f32))
        .expect("an ellipse is analytic");
    let mut list = DrawList::new();
    list.view(screen());
    list.push_shape(&params, [32.0, 32.0], &ShapeStyle::filled(RED));

    let pixels = render(device, queue, &mut renderer, &list);
    assert_eq!(pixel(&pixels, 32, 32), OPAQUE_RED, "the centre");
    assert_eq!(pixel(&pixels, 32, 1), OPAQUE_RED, "the top of the minor axis");
    assert_eq!(pixel(&pixels, 1, 32), OPAQUE_RED, "the left of the major axis");
    for (x, y) in [(2, 2), (61, 2), (2, 61), (61, 61)] {
        assert_eq!(pixel(&pixels, x, y), CLEAR, "the box corner at ({x},{y})");
    }
}

/// The polygon field, on a real catalogue shape. Every assertion is checked against
/// `SdfParams::distance` on the CPU first, so the test states one fact — the shader
/// agrees with the Rust reference `vellum-shapes` already tests its outline against.
#[test]
fn an_analytic_polygon_agrees_with_its_signed_distance() {
    let Some((device, queue)) = common::gpu() else {
        return common::skipped("the offscreen render check");
    };
    let mut renderer = renderer(device);

    let params = Shape::Diamond
        .sdf_params(Size::new(TARGET as f32, TARGET as f32))
        .expect("a diamond is a polygon");
    let mut list = DrawList::new();
    list.view(screen());
    list.push_shape(&params, [32.0, 32.0], &ShapeStyle::filled(BLUE));

    let pixels = render(device, queue, &mut renderer, &list);
    let mut inside = 0;
    let mut outside = 0;
    for y in 0..TARGET {
        for x in 0..TARGET {
            // Item-local: the pixel's centre, relative to the shape's centre.
            let local = vellum_shapes::p(x as f32 + 0.5 - 32.0, y as f32 + 0.5 - 32.0);
            let distance = params.distance(local);
            // Skip the antialiased band, which is a blend by design.
            if distance < -1.5 {
                assert_eq!(pixel(&pixels, x, y), OPAQUE_BLUE, "inside at ({x},{y})");
                inside += 1;
            } else if distance > 1.5 {
                assert_eq!(pixel(&pixels, x, y), CLEAR, "outside at ({x},{y})");
                outside += 1;
            }
        }
    }
    assert!(inside > 500 && outside > 500, "{inside} inside, {outside} outside");
}

/// The crop must sample the region it names and nothing else. This is the claim the
/// feature catalogue makes about images — "crop is a UV-rect, so it stays free" — and
/// getting the axes the wrong way round produces a plausible-looking image of the
/// wrong part of the picture.
#[test]
fn a_cropped_texture_samples_the_region_it_names() {
    let Some((device, queue)) = common::gpu() else {
        return common::skipped("the offscreen render check");
    };
    let mut renderer = Renderer::with_limits(
        device,
        FORMAT,
        TextureBudget::default(),
        AtlasConfig::default(),
    );

    // 64 x 64, one flat colour per quadrant.
    const SIZE: u32 = 64;
    let quadrant = |x: u32, y: u32| -> [u8; 4] {
        match (x < SIZE / 2, y < SIZE / 2) {
            (true, true) => [255, 0, 0, 255],
            (false, true) => [0, 255, 0, 255],
            (true, false) => [0, 0, 255, 255],
            (false, false) => [255, 255, 0, 255],
        }
    };
    let mut rgba = Vec::with_capacity((SIZE * SIZE * 4) as usize);
    for y in 0..SIZE {
        for x in 0..SIZE {
            rgba.extend_from_slice(&quadrant(x, y));
        }
    }
    let id = renderer
        .textures_mut()
        .upload(device, queue, &ImageSource::new(SIZE, SIZE, &rgba))
        .expect("the test image must upload");

    // Show only the bottom-left quadrant, blown up to the whole target.
    let crop = UvRect::from_pixels(0, SIZE / 2, SIZE / 2, SIZE / 2, (SIZE, SIZE));
    let mut list = DrawList::new();
    list.view(screen());
    list.push_image(
        id,
        ImageInstance::new([0.0, 0.0], [TARGET as f32, TARGET as f32], crop),
    );

    let pixels = render(device, queue, &mut renderer, &list);
    for (x, y) in [(8, 8), (32, 32), (56, 56), (8, 56), (56, 8)] {
        assert_near(pixel(&pixels, x, y), OPAQUE_BLUE, 2, &format!("cropped at ({x},{y})"));
    }

    // The uncropped image shows all four quadrants, the right way up: red top-left,
    // yellow bottom-right.
    let mut list = DrawList::new();
    list.view(screen());
    list.push_image(
        id,
        ImageInstance::new([0.0, 0.0], [TARGET as f32, TARGET as f32], UvRect::FULL),
    );
    let pixels = render(device, queue, &mut renderer, &list);
    assert_near(pixel(&pixels, 16, 16), OPAQUE_RED, 2, "top-left");
    assert_near(pixel(&pixels, 48, 16), OPAQUE_GREEN, 2, "top-right");
    assert_near(pixel(&pixels, 16, 48), OPAQUE_BLUE, 2, "bottom-left");
    assert_near(pixel(&pixels, 48, 48), [255, 255, 0, 255], 2, "bottom-right");
}

/// Atlas rows are stored top-down and `v` increases downwards, so a glyph must arrive
/// the way up it was rasterised. The bitmap here has ink in its **top** row only; if
/// the pairing of quad corner to UV corner were reversed the ink would appear at the
/// bottom, which is the classic upside-down-text bug.
///
/// A synthetic bitmap rather than a real letter: the assertion is about the atlas and
/// the shader, and a real glyph would make it depend on whichever face the platform
/// happens to provide.
#[test]
fn glyphs_are_not_vertically_flipped() {
    use vellum_text::{GlyphContent, GlyphImage, LayoutParams, StyledText, TextEngine};

    let Some((device, queue)) = common::gpu() else {
        return common::skipped("the offscreen render check");
    };
    let mut renderer = renderer(device);

    let Ok(mut engine) = TextEngine::new() else {
        eprintln!("no system fonts; skipping the glyph placement check");
        return;
    };
    // A real key, because only `vellum-text` can mint one — the image under it is
    // ours.
    let layout = engine.layout(&StyledText::plain("H"), &LayoutParams::default());
    let key = layout
        .glyphs()
        .next()
        .expect("H shapes to one glyph")
        .physical((0.0, 0.0), 1.0)
        .key;

    const W: u32 = 8;
    const H: u32 = 8;
    let mut data = vec![0u8; (W * H) as usize];
    data[..W as usize].fill(255);
    let image = GlyphImage {
        content: GlyphContent::Coverage,
        width: W,
        height: H,
        left: 0,
        // The bitmap sits entirely above the baseline.
        top: H as i32,
        data,
    };

    renderer
        .atlas_mut()
        .prepare(device, queue, &[(key, image)])
        .expect("one 8x8 glyph must fit");
    let slot = *renderer.atlas().slot(key).expect("the glyph is resident");

    let mut list = DrawList::new();
    list.view(screen());
    // Pen on the baseline at y = 40, so the bitmap occupies rows 32..40.
    list.push_glyph(&slot, (24, 40), GREEN);

    let pixels = render(device, queue, &mut renderer, &list);
    assert_eq!(pixel(&pixels, 28, 32), OPAQUE_GREEN, "the inked row is at the top");
    assert_eq!(pixel(&pixels, 28, 39), CLEAR, "the blank rows are at the bottom");
    assert_eq!(pixel(&pixels, 28, 31), CLEAR, "nothing above the bitmap");
    assert_eq!(pixel(&pixels, 23, 32), CLEAR, "nothing left of the pen");
}

/// Mesh vertices stay in their own space and are placed by a transform the vertex
/// shader looks up. That indirection is what lets a pan rewrite a few dozen floats
/// instead of a megabyte of ink, so it has to actually work.
#[test]
fn a_mesh_triangle_is_placed_by_its_transform() {
    let Some((device, queue)) = common::gpu() else {
        return common::skipped("the offscreen render check");
    };
    let mut renderer = renderer(device);

    let mut list = DrawList::new();
    list.view(screen());
    // A right triangle in a 0..1 unit box, mapped onto the target's top-left quadrant.
    let transform = list
        .meshes_mut()
        .push_transform(MeshTransform::unit_box([0.0, 0.0], [32.0, 32.0], 0.0));
    list.meshes_mut().push_indexed(
        &[[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]],
        &[0, 1, 2],
        RED,
        transform,
    );
    list.push_all_meshes();

    let pixels = render(device, queue, &mut renderer, &list);
    assert_eq!(pixel(&pixels, 4, 4), OPAQUE_RED, "well inside the triangle");
    assert_eq!(pixel(&pixels, 24, 24), CLEAR, "past the hypotenuse");
    assert_eq!(pixel(&pixels, 40, 8), CLEAR, "outside the unit box entirely");

    // Move the transform, leave the geometry alone: the triangle moves. This is the
    // pan path — the transform table is re-uploaded and the vertex buffer is not —
    // so it is exercised without a full `prepare`.
    list.meshes_mut()
        .set_transform(transform, MeshTransform::unit_box([32.0, 32.0], [32.0, 32.0], 0.0));
    renderer.update_mesh_transforms(device, queue, list.meshes().transforms());
    let pixels = common::redraw(device, queue, &renderer);
    assert_eq!(pixel(&pixels, 4, 4), CLEAR, "no longer in the old quadrant");
    assert_eq!(pixel(&pixels, 36, 36), OPAQUE_RED, "in the new one");
}

/// A diagonal mesh edge resolves to **partial coverage**, not to a staircase.
///
/// # Why this is asserted on pixels rather than on the sample count
///
/// The mesh pipeline is the one content path in the app with no analytic antialiasing: a
/// quad, a shape, an image and a glyph all compute sub-pixel coverage in their own fragment
/// shader, while `fs_mesh` returns a flat colour because tessellated geometry has no distance
/// field to sample. So freehand ink, connectors and lyon-stroked shape outlines were
/// rasterised with binary coverage — the user compared Velm's pen against Miro's and said it
/// *"looks so much more pixelated, so much more uglier"*.
///
/// Asserting `BOARD_SAMPLES > 1` would pin the mechanism and prove nothing about the result:
/// a multisampled pipeline drawing into an attachment nobody resolves, or resolved with
/// `StoreOp` set wrong, satisfies it and still draws a staircase. A count of intermediate
/// colours along the hypotenuse is the property the user is actually looking at, and it is
/// **zero** on the unmultisampled build.
#[test]
fn a_diagonal_mesh_edge_is_antialiased_rather_than_stepped() {
    let Some((device, queue)) = common::gpu() else {
        return common::skipped("the offscreen render check");
    };
    let mut renderer = renderer(device);

    let mut list = DrawList::new();
    list.view(screen());
    let transform = list
        .meshes_mut()
        .push_transform(MeshTransform::unit_box([0.0, 0.0], [48.0, 48.0], 0.0));
    list.meshes_mut().push_indexed(
        &[[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]],
        &[0, 1, 2],
        RED,
        transform,
    );
    list.push_all_meshes();
    let pixels = render(device, queue, &mut renderer, &list);

    // Walk the hypotenuse and count pixels that are neither fully covered nor fully clear.
    // On a binary rasteriser every pixel is one or the other and this is 0.
    let mut partial = 0;
    for step in 2..46 {
        let (x, y) = (step, 47 - step);
        let p = pixel(&pixels, x, y);
        if p != OPAQUE_RED && p != CLEAR {
            partial += 1;
        }
    }
    assert!(
        partial >= 10,
        "a 45° mesh edge should resolve to partial coverage; only {partial} of 44 pixels \
         along the hypotenuse were blended, which is a staircase"
    );
}

/// The precision claim, measured rather than asserted in prose. The same item drawn
/// at the far corner of the reference board, with the camera beside it, must produce
/// **byte-identical** pixels to one at the origin — because what reaches `f32` is the
/// camera-relative offset in both cases.
///
/// `vellum_scene::camera` proves the arithmetic; this proves the renderer actually
/// consumes it that way rather than casting a world coordinate somewhere along the
/// line.
#[test]
fn camera_relative_geometry_is_stable_at_the_boards_far_corner() {
    let Some((device, queue)) = common::gpu() else {
        return common::skipped("the offscreen render check");
    };
    let mut renderer = renderer(device);

    let viewport = ScreenSize::new(f64::from(TARGET), f64::from(TARGET));
    let draw = |renderer: &mut Renderer, centre: WorldPoint| {
        let mut camera = Camera::new(viewport);
        camera.set_center(centre);

        let mut list = DrawList::new();
        list.view(View::board(&camera));
        let corner = WorldPoint::new(centre.x - 16.0, centre.y - 16.0);
        list.push_quad(
            QuadInstance::solid(camera.to_camera_relative(corner), [32.0, 32.0], GREEN)
                .with_corner_radius(6.0)
                .with_border(RED, 3.0),
        );
        render(device, queue, renderer, &list)
    };

    let at_origin = draw(&mut renderer, WorldPoint::ORIGIN);
    // The far corner of the real board, and a point far beyond any board.
    let at_board_corner = draw(&mut renderer, WorldPoint::new(41_282.89, 17_515.36));
    let far_away = draw(&mut renderer, WorldPoint::new(10_000_000.3, -8_000_000.7));

    assert_eq!(at_origin, at_board_corner, "the board's far corner rendered differently");
    assert_eq!(at_origin, far_away, "a distant camera rendered differently");
    // And it actually drew something, so the comparison is not of three blank images.
    assert_eq!(pixel(&at_origin, 32, 32), OPAQUE_GREEN);
    assert_eq!(pixel(&at_origin, 32, 17), OPAQUE_RED);
}

/// Every pipeline in one pass, in one order, and the whole thing is a handful of
/// draws. This is the number `docs/01-architecture.md` makes the argument on, checked
/// against the plan the renderer actually recorded rather than against the list.
#[test]
fn a_frame_using_every_pipeline_is_a_handful_of_draw_calls() {
    let Some((device, queue)) = common::gpu() else {
        return common::skipped("the offscreen render check");
    };
    let mut renderer = renderer(device);

    let rgba = vec![255u8; 16 * 16 * 4];
    let id = renderer
        .textures_mut()
        .upload(device, queue, &ImageSource::new(16, 16, &rgba))
        .expect("upload");

    let diamond = Shape::Diamond.sdf_params(Size::new(12.0, 12.0)).unwrap();
    let mut list = DrawList::new();
    list.view(screen());
    for i in 0..8 {
        list.push_quad(QuadInstance::solid([i as f32 * 8.0, 0.0], [6.0, 6.0], RED));
    }
    for i in 0..8 {
        list.push_shape(&diamond, [i as f32 * 8.0 + 4.0, 20.0], &ShapeStyle::filled(GREEN));
    }
    for i in 0..8 {
        list.push_image(
            id,
            ImageInstance::new([i as f32 * 8.0, 32.0], [6.0, 6.0], UvRect::FULL),
        );
    }
    let transform = list.meshes_mut().push_transform(MeshTransform::at([0.0, 48.0]));
    for i in 0..8 {
        let x = i as f32 * 8.0;
        list.meshes_mut().push_indexed(
            &[[x, 0.0], [x + 6.0, 0.0], [x, 6.0]],
            &[0, 1, 2],
            BLUE,
            transform,
        );
    }
    list.push_all_meshes();

    let pixels = render(device, queue, &mut renderer, &list);
    assert_eq!(renderer.draw_calls(), 4, "quads, shapes, images, meshes");
    assert_eq!(list.stats().draw_calls, 4);

    // And each pipeline actually put something on screen.
    assert_eq!(pixel(&pixels, 2, 2), OPAQUE_RED, "quads");
    assert_eq!(pixel(&pixels, 4, 20), OPAQUE_GREEN, "shapes");
    assert_eq!(pixel(&pixels, 2, 34), [255, 255, 255, 255], "images");
    assert_eq!(pixel(&pixels, 1, 49), OPAQUE_BLUE, "meshes");
}

/// Opacity has to compose the fill and the border together rather than blending them
/// against each other — the premultiplied composite in `fill_and_border` is what makes
/// a half-transparent bordered sticky look like one object at half strength.
#[test]
fn opacity_scales_the_whole_shape_rather_than_its_parts() {
    let Some((device, queue)) = common::gpu() else {
        return common::skipped("the offscreen render check");
    };
    let mut renderer = renderer(device);

    let mut list = DrawList::new();
    list.view(screen());
    list.push_quad(
        QuadInstance::solid([0.0, 0.0], [TARGET as f32, TARGET as f32], RED)
            .with_border(BLUE, 8.0)
            .with_opacity(0.5),
    );

    let pixels = render(device, queue, &mut renderer, &list);
    // Half red over black.
    assert_near(pixel(&pixels, 32, 32), [128, 0, 0, 255], 2, "the fill at half opacity");
    // Half blue over black — not a blend of red and blue.
    assert_near(pixel(&pixels, 32, 4), [0, 0, 128, 255], 2, "the border at half opacity");
}

/// The whole text path with real glyphs: shape, rasterise, pack, place, draw.
///
/// Deliberately loose about *which* pixels are inked — the face is whatever the
/// platform provides — and strict about the two things that are ours: every glyph
/// found a slot, and the ink landed inside the block's own box rather than somewhere
/// a sign error would have put it.
#[test]
fn a_real_text_block_lands_inside_its_own_box() {
    use vellum_text::{LayoutParams, StyledText, TextEngine};

    let Some((device, queue)) = common::gpu() else {
        return common::skipped("the offscreen render check");
    };
    let Ok(mut engine) = TextEngine::new() else {
        eprintln!("no system fonts; skipping the text rendering check");
        return;
    };
    let mut renderer = renderer(device);

    let params = LayoutParams { font_size: 18.0, ..LayoutParams::default() };
    let text = StyledText::plain("Hi");
    let origin = [8.0f32, 8.0];
    let layout = engine.layout(&text, &params);
    let entries = engine.atlas_entries(&layout, (origin[0], origin[1]), 1.0);
    assert!(!entries.is_empty(), "two letters must rasterise");

    renderer
        .atlas_mut()
        .prepare(device, queue, &entries)
        .expect("two glyphs at 18px must fit");

    let mut list = DrawList::new();
    list.view(screen());
    let missing = list.push_layout(renderer.atlas(), &layout, origin, 1.0, GREEN);
    assert_eq!(missing, 0, "every glyph must have found a slot");
    assert!(list.stats().glyphs >= 2, "{:?}", list.stats());

    let pixels = render(device, queue, &mut renderer, &list);
    let inked = |x: u32, y: u32| pixel(&pixels, x, y) != CLEAR;

    let bottom = (origin[1] + layout.extent.height).ceil() as u32;
    assert!(bottom < TARGET, "the block must fit in the target: {bottom}");
    assert!(
        (origin[1] as u32..bottom).any(|y| (0..TARGET).any(|x| inked(x, y))),
        "no ink inside the block's box"
    );
    for y in 0..origin[1] as u32 {
        assert!((0..TARGET).all(|x| !inked(x, y)), "ink above the block at row {y}");
    }
    for y in bottom..TARGET {
        assert!((0..TARGET).all(|x| !inked(x, y)), "ink below the block at row {y}");
    }
    for x in 0..origin[0] as u32 {
        assert!((0..TARGET).all(|y| !inked(x, y)), "ink left of the block at column {x}");
    }
}
