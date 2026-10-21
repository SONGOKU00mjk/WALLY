//! Criterion benches for the paths the README makes claims about.
//!
//! Run with: `cargo bench`. Hardware, methodology and regeneration
//! commands: docs/benchmarks.md.
//!
//! wally is a binary-only crate, so the bench target pulls the modules in
//! directly (`#[path]` includes — the same files, compiled once more for
//! the bench; no lib.rs split). cfg(test) unit tests inside them are not
//! compiled here.
//!
//! These benches are pure CPU: they decode and compose real fixture bytes
//! into in-memory buffers and never touch a compositor. What they do NOT
//! capture is the other half of wally's performance identity — the parts
//! that are about *not* working (identical-frame skip, presentation
//! gating, relax/madvise handback) — because absence of work is not
//! benchmarkable; those are pinned by tests and WALLY_DEBUG counters.

// The modules under test, in dependency order. The includes compile the
// files outside their usual crate context, so parts of them are dead here
// (test-only helpers, main-only entry points) — allowed once, here, so
// the modules themselves stay lint-clean for the real build.
#[allow(dead_code, unused_imports, unused_mut, unused_variables)]
#[path = "../src/cli.rs"]
mod cli;
#[allow(dead_code, unused_imports, unused_mut, unused_variables)]
#[path = "../src/daemon.rs"]
mod daemon;
#[allow(dead_code, unused_imports, unused_mut, unused_variables)]
#[path = "../src/render.rs"]
mod render;
#[allow(dead_code, unused_imports, unused_mut, unused_variables)]
#[path = "../src/shm.rs"]
mod shm;
#[allow(dead_code, unused_imports, unused_mut, unused_variables)]
#[path = "../src/source.rs"]
mod source;

use std::hint::black_box;

use criterion::{criterion_group, criterion_main, Criterion};
use image::RgbaImage;

use cli::Mode;
use render::Rect;
use source::Source;

/// A deterministic synthetic wallpaper: gradients exercise every resample
/// tap, unlike a flat color.
fn gradient(w: u32, h: u32) -> RgbaImage {
    let mut img = RgbaImage::new(w, h);
    for (x, y, px) in img.enumerate_pixels_mut() {
        px.0 = [
            (x * 255 / w.max(1)) as u8,
            (y * 255 / h.max(1)) as u8,
            ((x + y) % 256) as u8,
            255,
        ];
    }
    img
}

fn compose_buf(pw: u32, ph: u32) -> Vec<u8> {
    vec![0u8; pw as usize * ph as usize * 4]
}

/// Full recompose of an 800x600 source onto a 1920x1080 output: the
/// worst case the painter can face in a tick.
fn bench_full_recompose(c: &mut Criterion) {
    let img = gradient(800, 600);
    c.bench_function("compose/full 800x600 -> 1920x1080 fill", |b| {
        b.iter(|| {
            let mut dst = compose_buf(1920, 1080);
            render::compose_into(black_box(&img), Mode::Fill, 1920, 1080, [0, 0, 0], &mut dst);
            dst
        })
    });
}

/// Sub-rect recompose: a typical animated GIF frame only rewrites a band.
/// The rect path composes exactly that band out of an already-composed
/// XRGB canvas; same source, same output.
fn bench_subrect_recompose(c: &mut Criterion) {
    let img = gradient(800, 600);
    let canvas = {
        let mut dst = compose_buf(1920, 1080);
        render::compose_into(&img, Mode::Fill, 1920, 1080, [0, 0, 0], &mut dst);
        dst
    };
    let band = Rect {
        x: 560,
        y: 480,
        w: 800,
        h: 120,
    };
    c.bench_function("compose/sub-rect 800x120 band of 1920x1080", |b| {
        b.iter(|| {
            let mut dst = canvas.clone();
            render::compose_xrgb_into_rect(
                black_box(&canvas),
                (800, 600),
                Mode::Fill,
                1920,
                1080,
                [0, 0, 0],
                &mut dst,
                band,
            );
            dst
        })
    });
}

/// wally's compose path (crop + separable Triangle + XRGB pack) against
/// the `image` crate's generic Triangle resize on identical inputs —
/// stretch mode on an aspect-true target is a pure resample, so this is
/// the honest apples-to-apples pair.
fn bench_resample_vs_image(c: &mut Criterion) {
    let img = gradient(1024, 768);
    let mut group = c.benchmark_group("resample 1024x768 -> 1920x1080");
    group.bench_function("wally (stretch compose, Triangle)", |b| {
        b.iter(|| {
            let mut dst = compose_buf(1920, 1080);
            render::compose_into(
                black_box(&img),
                Mode::Stretch,
                1920,
                1080,
                [0, 0, 0],
                &mut dst,
            );
            dst
        })
    });
    group.bench_function("image::imageops::resize (Triangle)", |b| {
        b.iter(|| {
            image::imageops::resize(
                black_box(&img),
                1920,
                1080,
                image::imageops::FilterType::Triangle,
            )
        })
    });
    group.finish();
}

/// Animated WebP frame decode, the actual per-tick cost of a wallpaper
/// animation: one step = one frame composited into the canvas.
fn bench_webp_frame_decode(c: &mut Criterion) {
    let path = std::path::Path::new("tests/fixtures/anim-lossy.webp");
    c.bench_function("webp/decode+composite frame (anim-lossy)", |b| {
        b.iter_batched(
            || Source::open(path, true).expect("fixture opens"),
            |mut src| {
                for _ in 0..10 {
                    let _ = black_box(src.step());
                }
            },
            criterion::BatchSize::LargeInput,
        )
    });
}

criterion_group!(
    benches,
    bench_full_recompose,
    bench_subrect_recompose,
    bench_resample_vs_image,
    bench_webp_frame_decode
);
criterion_main!(benches);
