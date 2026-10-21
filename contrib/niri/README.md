# wally under niri

## Autostart

One entry in `~/.config/niri/config.kdl`:

```kdl
spawn-at-startup "wally" "--replace" "-i" "/home/you/pictures/wallpaper.png"
```

niri spawns plain argv — every argument gets its own quoted string.

## Changing the wallpaper

```sh
wally --replace -i ~/pictures/other.png
```

The new instance renders before the old one is asked to exit, so there is
no black-screen gap.

## Per-workspace backdrops

wally draws one backdrop per **output** (that is what layer-shell offers:
a background layer anchored to a whole monitor). niri workspaces live on
shared outputs, so a different wallpaper per workspace is not a layer-shell
feature — it currently requires a wrapper that re-runs wally (or similar
tools) as you switch workspaces, which fights the one-canvas design. If you
want this, a compositor-side workspace background feature or a screenshot-
compositing wrapper are the honest paths; do not try to fake it with
multiple layer surfaces, they all cover the whole output.
