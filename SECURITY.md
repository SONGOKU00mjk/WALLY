# Security policy

## Supported versions

wally is pre-1.0: security fixes land in the latest release (currently the
0.2.x line) and on `main`. Older tags are not maintained.

## Reporting a vulnerability

Please **do not** open a public issue for a vulnerability.

Use GitHub's private reporting flow:
<https://github.com/SONGOKU00mjk/wally/security/advisories/new>

Include:

- the wally version (`wally --version`) and how it was built (release
  binary, `cargo install`, `install.sh`, distribution package);
- compositor and version (Sway, Hyprland, niri, KWin, …);
- a description of the impact and the smallest reproduction you have;
- any relevant image file or a pointer to it (image decoders are the most
  likely place for a memory-safety issue).

You will get an acknowledgement as soon as possible, and credit in the
advisory unless you ask otherwise.

## Scope

wally decodes untrusted image files in-process. Memory-safety bugs in the
decoders it uses are in scope; so are flaws in the control socket
(`$XDG_RUNTIME_DIR/wally-<display>.sock`), the state file parser, and the
installer's download/verification path.

Out of scope: vulnerabilities in the compositor, in the image files
themselves being "malicious looking", and anything that requires an
attacker to already control the user's session.
