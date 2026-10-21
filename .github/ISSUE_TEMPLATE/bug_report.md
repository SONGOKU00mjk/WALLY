---
name: Bug report
about: wally set the wrong thing, crashed, or misbehaves
labels: bug
---

Thank you for reporting a bug. Please fill in everything below — without
the debug output we can usually only guess.

**What happened?**

A clear description of what you expected and what happened instead.

**How to reproduce**

Steps or the exact command line, e.g.:

```sh
wally -i wallpaper.png -m fill
```

**The image that fails**

Which format is it (png/jpeg/gif/webp/avif/jxl/…)? If the file is not
sensitive, attach it — half of image bugs are decoder edge cases that only
reproduce with the actual bytes.

**Debug output**

Run wally with debug enabled and paste the output:

```sh
WALLY_DEBUG=1 wally -i your-image.png
```

(`WALLY_DEBUG=2` adds per-frame tracing for animation bugs — much more
output, use it when the bug is about animation playback.)

**Compositor and environment**

- Compositor and version (`sway --version`, `hyprctl version`, …):
- Output of `wally --list-outputs` (if wally starts at all):
- Distribution and architecture (`install.sh --check` prints a summary):
- How you installed wally (install.sh / cargo / distribution package):

**Anything else?**

Screenshots, `/proc/$(pidof wally)/status` snippets for memory reports,
or anything else that seems relevant.
