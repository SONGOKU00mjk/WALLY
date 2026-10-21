# wally under Hyprland

## Autostart

One line in `~/.config/hypr/hyprland.conf`:

```ini
exec-once = wally --replace -i ~/pictures/wallpaper.png
```

For a solid color instead of an image:

```ini
exec-once = wally -c 0x1a1b26
```

## Ordering notes

- `exec-once` runs when Hyprland starts, before/while outputs are being
  configured. wally waits for the `wlr_layer_shell` and `wl_output` globals
  it needs, so it is safe to start this early — a GPU/display that is not
  ready yet shows as "no matching output" in `WALLY_DEBUG=1` output and
  wally exits; use `execr-once = sh -c 'sleep 1; wally --replace -i ...'`
  on exotic multi-GPU setups where outputs appear late.
- Hyprland reloads (`hyprctl reload`) do not restart `exec-once` processes;
  re-run wally manually (with `--replace`, no gap) or via a `windowrule`
  script if you want reload-driven changes.
- Do not add wally to both `exec-once` and a systemd user unit — two
  instances stack layer surfaces; the second one wins visually but both
  keep running (and `--replace` would kill the first).

## Changing the wallpaper

```sh
wally --replace -i ~/pictures/other.png
```

No `pkill wally`, no `sleep 1`, no black flash: the new process renders
first and takes the old one's place after the first frame is on screen.
