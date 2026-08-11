//! Does churning textures through [`TextureManager`] grow the process?
//!
//! The question this answers is the worst bug this application has had: Velm was seen
//! at **14.24 GB** and then at **15.06 GB**, taking the machine down both times, while
//! every unattended run measured a flat 0.2–0.35 GB. The difference was sustained
//! interaction, and the thing sustained interaction does that an idle board does not is
//! *churn* — upload a texture, evict it, upload it again.
//!
//! `TextureManager` accounts its own bytes and holds them under a budget, so if its
//! accounting were the whole story a churn loop would sit flat at the budget. Resident
//! set size is read from the OS instead, because the number that runs a machine out of
//! memory is what the kernel thinks we hold — including GPU allocations wgpu has been
//! told to drop but has not yet reclaimed.

mod common;

use vellum_render::{ImageSource, TextureBudget, TextureManager};

/// Resident set size in bytes, or `0` where it cannot be read.
fn rss() -> u64 {
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("ps")
            .args(["-o", "rss=", "-p", &std::process::id().to_string()])
            .output()
            .ok()
            .and_then(|out| String::from_utf8(out.stdout).ok())
            .and_then(|text| text.trim().parse::<u64>().ok())
            .map_or(0, |kb| kb * 1024)
    }
    #[cfg(not(target_os = "macos"))]
    {
        0
    }
}

/// One 1024² RGBA image — 4 MiB decoded, plus its mip chain. Smaller than the 2048²
/// a reference-board photo lands at, because the question here is the *shape* of the
/// curve over many rounds and a smaller image buys more rounds per second.
fn image(tint: u8) -> Vec<u8> {
    vec![tint; 1024 * 1024 * 4]
}

/// Uploads far more than the budget, over and over, exactly as a pan across an
/// image-heavy board does. Every upload is followed by the eviction the real caller
/// runs (`crate::assets::Assets::load` does the same two lines).
///
/// The assertion is deliberately loose. This is not a benchmark of steady-state
/// residency — it is asking whether the floor moves at all, and a leak of the shape
/// being hunted moves it by gigabytes, not by the tens of megabytes of slack a real
/// allocator keeps.
#[test]
fn churning_textures_does_not_grow_the_process() {
    let Some((device, queue)) = common::gpu() else {
        common::skipped("texture churn");
        return;
    };

    // A small budget so every round evicts: 64 MiB holds four of the images below.
    let mut textures =
        TextureManager::new(device, TextureBudget { max_bytes: 64 << 20, max_dimension: 2048 });

    // One upload per frame, which is what the real caller does: `Assets` spends a 4 ms
    // decode budget per frame and a 2048² JPEG uses all of it. Advancing the frame is
    // what makes the previous upload evictable at all — `evict_to_budget` deliberately
    // refuses to touch anything marked in the *current* frame, and a loop that uploads
    // several per frame is documented to sit over budget rather than thrash.
    let round = |textures: &mut TextureManager, tint: u8| {
        textures.begin_frame();
        let pixels = image(tint);
        textures
            .upload(device, queue, &ImageSource::new(1024, 1024, &pixels))
            .expect("a 1024 square uploads");
        textures.evict_to_budget();

        // The frame boundary, and both halves of it matter.
        //
        // `Queue::write_texture` does not write anything: it stages the bytes and they
        // are flushed on the next submit. A loop that uploads without submitting piles
        // those staging buffers up at the full size of every image — which is what an
        // earlier version of this test measured and very nearly reported as the
        // application's leak.
        //
        // The poll is the other half: dropping a `wgpu::Texture` only marks it for
        // destruction, and the allocation comes back when the device is polled and the
        // work referencing it has retired.
        queue.submit(std::iter::empty());
        let _ = device.poll(wgpu::PollType::Poll);
    };

    // Warm up, so the baseline is after the allocator and the driver have settled
    // rather than including their first-use growth.
    for tint in 0..32u8 {
        round(&mut textures, tint);
    }

    // The *trend*, not two endpoints. A single before/after cannot tell a leak from
    // allocator slack, and measured that way this test read 61 MB → 683 MB on one run
    // and 449 MB → 792 MB on the next. What distinguishes them is the shape: slack
    // plateaus, a leak keeps climbing.
    const CHECKPOINTS: usize = 8;
    const PER_CHECKPOINT: usize = 32;
    let mut curve = Vec::with_capacity(CHECKPOINTS);
    for checkpoint in 0..CHECKPOINTS {
        for step in 0..PER_CHECKPOINT {
            round(&mut textures, (checkpoint * PER_CHECKPOINT + step) as u8);
        }
        curve.push(rss());
    }

    let accounted = textures.resident_bytes();
    let churned = (CHECKPOINTS * PER_CHECKPOINT) as f64 * 4.0 * 1.34 / 1024.0;
    eprintln!("accounted {} MB (budget 64 MB)", accounted / 1_048_576);
    eprintln!("churned ~{churned:.1} GB through it; RSS at each checkpoint:");
    for (index, bytes) in curve.iter().enumerate() {
        eprintln!(
            "  after {:>4} uploads: {:>5} MB",
            (index + 1) * PER_CHECKPOINT,
            bytes / 1_048_576
        );
    }

    assert!(
        accounted <= 64 << 20,
        "one upload per frame must stay inside the budget, but {accounted} bytes are resident"
    );

    if curve.iter().all(|bytes| *bytes == 0) {
        return; // RSS unavailable on this platform; the accounting check still ran.
    }

    // Compare the second half against the first. Slack is paid once and shows up in
    // the first half; a leak keeps taking, so the later half grows just as much.
    let first = curve[CHECKPOINTS / 2 - 1];
    let last = curve[CHECKPOINTS - 1];
    let late_growth = last.saturating_sub(first);
    assert!(
        late_growth < 64 << 20,
        "resident memory grew a further {:.0} MB over the last {} uploads, long after \
         any warm-up — dropped GPU allocations are not being reclaimed. Curve: {:?} MB",
        late_growth as f64 / 1_048_576.0,
        CHECKPOINTS / 2 * PER_CHECKPOINT,
        curve.iter().map(|b| b / 1_048_576).collect::<Vec<_>>()
    );
}
