# Example environment profiles

These JSON files are `environment-contract-v1` profiles. Select one:

```bash
ags env-use tokyo-macos
ags env-show
ags codex
```

`ags env-use tokyo-macos` copies the bundled example into
`~/.config/ags/environments/` and records the choice in
`~/.local/state/ags/environment-selection.json`. Edit that copy, or pass any
other profile path. `ags env-clear` stops overlaying.

The overlay makes `hostname`, `whoami`, `uname`, `date`, `TZ`, and `LANG` match
the profile through process environment and provider hooks. It does not hide
the real kernel, `/proc`, or `gethostname(2)`.
