# wally under sway (systemd user unit)

`wally.service` runs wally as part of a systemd-managed sway session. It
needs the `sway-session.target` that [sway-systemd](https://github.com/alebastr/sway-systemd)
(or uwsm) provides; without it, use the plain `exec` recipe from the main
README instead.

## Install

1. Copy the unit somewhere systemd looks at — `~/.config/systemd/user/` is
   the usual choice — and edit `ExecStart=` to point at your wallpaper and
   the real wally binary path (`command -v wally`).

2. Reload and start:

```sh
systemctl --user daemon-reload
systemctl --user enable --now wally.service
```

3. Check it:

```sh
systemctl --user status wally.service
journalctl --user -u wally.service -f
```

## Changing the wallpaper afterwards

`wally --replace -i ~/pictures/new.png` — the running instance is taken
over by the new process, so there is no black-screen gap. If you did NOT
start wally with `--replace`, killing and restarting leaves the layer
surface destroyed for a moment and the screen flashes black.

## The stale symlink trap

`systemctl --user enable` writes `~/.config/systemd/user/sway-session.target.wants/wally.service`.
If you later rename or copy the unit (say you keep a copy in
`contrib/sway/` and enable that one too), you can end up with **two**
`.wants` entries pointing at two different unit files with the same
purpose — systemd then builds a confusing dependency chain where the
wallpaper starts before the session is ready or starts twice. Symptoms:
`wally.service` reported active but no wallpaper, or "Job
wally.service/start deleted to break ordering cycle" in the journal.

Fix: keep exactly one unit file, one name:

```sh
ls -la ~/.config/systemd/user/sway-session.target.wants/
systemctl --user disable wally.service   # remove every stale entry
rm ~/.config/systemd/user/sway-session.target.wants/wally.service  # if still there
systemctl --user daemon-reload
systemctl --user enable --now wally.service
```
