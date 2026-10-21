# Benchmarks

wally's performance claims are measurable. This document says exactly how
the numbers are produced so anyone can reproduce or challenge them.

## What is benchmarked, and what is not

`cargo bench` (criterion, `benches/bench.rs`) covers the CPU work per
frame:

| bench | what it measures |
|---|---|
| `compose/full 800x600 -> 1920x1080 fill` | worst-case full recompose of a static-sized source (crop + separable Triangle + XRGB pack) |
| `compose/sub-rect 800x120 band of 1920x1080` | the same output when an animation only dirtied a band — the rect pipeline |
| `resample 1024x768 -> 1920x1080` (both variants) | wally's compose path vs `image::imageops::resize` with the same Triangle filter, aspect-true target so both do a pure resample |
| `webp/decode+composite frame (anim-lossy)` | one animation step of a real lossy animated WebP (decode into the display canvas) |

What the benchmarks deliberately do **not** cover: the "not working" half
of the performance identity — identical-frame skip, presentation gating
(zero CPU when the screen is off), sub-rect *damage*, the `madvise`
handback, multi-output sharing. Absence of work is not benchmarkable; it
is pinned by unit tests (e.g. `static_sources_never_schedule_a_wake`,
`presentation_gate_fails_open_and_honors_grace`) and observable with
`WALLY_DEBUG=1` (rss/cpu lines) and `WALLY_DEBUG=2` (compose-rect trace)
on a real compositor.

## How to run

```sh
cargo bench                                       # everything, default settings
cargo bench -- --warm-up-time 1 --measurement-time 3   # quicker pass
cargo bench -- "sub-rect"                         # one bench
```

Criterion keeps history under `target/criterion/`; each run is compared
against the previous one on the same machine.

## Reference numbers

From the development sandbox (Debian 13, containerized, shared CPU —
treat as relative, not absolute; run on your hardware for real numbers):

| bench | time |
|---|---|
| full recompose, 800x600 → 1920x1080 fill | ~9.7 ms |
| sub-rect band 800x120 of the same output | ~1.2 ms (8.1x less than full) |
| wally stretch compose (Triangle), 1024x768 → 1920x1080 | ~11.5 ms |
| `image::imageops::resize` Triangle, same input/output | ~179.5 ms (~15.6x slower) |
| animated WebP frame (lossy), decode + composite | ~0.36 ms |

The resample gap is the Q8 fixed-point separable pipeline versus the
generic float resize; output equality between the two is pinned within
±1 LSB by the parity tests, so the speedup is free, not a shortcut.

## Fixtures

The benches use `tests/fixtures/anim-lossy.webp` (sha256 below) and a
deterministic procedural gradient, so no external assets are needed.

```sh
sha256sum tests/fixtures/anim-lossy.webp
```

## CI regression gate

There is intentionally no "fail on >15 % regression" CI job: shared CI
runners routinely vary more than 15 % between runs of the same code, so
the gate would either cry wolf or need enough repeats to cost more than
it protects. The benchmark history in `target/criterion/` plus the
reference table above serve the same purpose with less noise. If a
dedicated runner ever exists, `critcmp` against a stored baseline is the
honest way to add the gate.
