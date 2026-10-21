---
name: Feature request
about: something wally should do that it does not
labels: enhancement
---

Thank you for the idea. WALLY is deliberately minimal, so the bar is: does
this fit "one small binary that sets wallpapers correctly"? Features that
belong in a wallpaper manager (scheduling, per-workspace history, online
galleries) are out of scope on purpose.

**What would you like wally to do?**

Describe the outcome, not the implementation.

**The problem it solves**

What are you trying to do, and what do you do today instead (workarounds,
other tools)?

**What other tools do**

Does swaybg, awww/swww, mpvpaper, or your compositor's built-in tooling
already do this? How does their approach feel?

**Constraints a solution must respect**

WALLY stays `wlr-layer-shell` + pure Rust with no GUI toolkit and no
heavyweight dependencies, and the one-shot path stays the default. If your
idea needs a toolkit or a system library, say so anyway — sometimes the
honest answer is a documented "no".

**Alternatives considered**

Other flags, other tools, scripts over the existing CLI.
