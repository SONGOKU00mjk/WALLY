# Session integration recipes

Tested starting points for wiring wally into a Wayland session. Each
compositor directory contains a ready-made config or unit plus the
compositor-specific notes; read the one you need.

- `sway/` — systemd **user** unit (`wally.service`) for sway-systemd
  sessions, including the stale `.wants` symlink warning;
- `hyprland/` — the `exec-once` line and ordering notes;
- `niri/` — the `spawn-at-startup` entry, plus the honest note that
  per-workspace backdrops currently require a wrapper.

## The canonical one-shot loop problem

Almost every wallpaper tool that runs once per image gets scripted like
this eventually:

```sh
while true; do
    pkill swaybg
    swaybg -i "$(next-wallpaper)"
    sleep 30m
done
```

Every iteration of that loop shows the same black flash, and it is not a
bug in any one tool — it is the shape of the solution:

1. `pkill` destroys the layer surface; the compositor immediately renders
   the background layer's clear color (black);
2. the replacement process must then start, connect, negotiate
   `wlr-layer-shell`, allocate a buffer, decode the image, and commit —
   somewhere between hundreds of milliseconds and a second;
3. that whole gap is on your screen.

## How to avoid it with wally

WALLY is a long-lived process: it sets the image and **stays**, driving
animations with frame callbacks and going fully idle for static images, so
a wallpaper change does not need to kill anything at all when you keep one
instance per configuration. When you do want a different image in the
running session, the gap is closed with `--replace`:

```sh
wally --replace -i ~/pictures/next.png
```

The new process connects, renders the new image into its own layer
surfaces, and only then asks the previous wally to exit — the old pixels
stay on screen until the new ones are committed, so there is nothing to
see. The compositor stacks same-layer surfaces newest-first, which is what
makes the handover invisible.

A side effect worth knowing: if you start wally twice *without*
`--replace`, both instances run and the newer one simply draws on top of
the older one. That is the takeover mechanism working as designed — but it
also means a stray second instance is silent; `wally --list-outputs` and
`pgrep -a wally` tell you what is actually running.
