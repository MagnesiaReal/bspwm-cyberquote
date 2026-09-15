# bspwm-cyberquote-native

Native (Cairo/Pango) fork of [bspwm-cyberquote](https://github.com/MagnesiaReal/bspwm_cyberquote) —
a cyberpunk motivational quote overlay for BSPWM. One undecorated, desktop-hint
window per monitor paints the quote ticker (scanlines, vignette, RGB-split
glitch, typewriter reveal, terminal caret) directly with Cairo — no WebView.

## Install (Arch Linux)

```sh
makepkg -si           # run from the repository root (packaging/arch/PKGBUILD)
```

This installs:

| Path | Purpose |
|---|---|
| `/usr/bin/bspwm-cyberquote-native` | the binary |
| `/etc/bspwm-cyberquote/config.toml` | system-wide default config |
| `/usr/share/bspwm-cyberquote-native/quotes.json` | default quote pool |
| `/usr/lib/... ` | _(nothing — no systemd unit, launch via bspwmrc)_ |

### Config

On first run the app copies the packaged default to
`~/.config/bspwm-cyberquote/config.toml`; edit that copy to personalize.
Resolution order:

1. `~/.config/bspwm-cyberquote/config.toml`
2. `/etc/bspwm-cyberquote/config.toml`
3. `./config.toml` (repo checkout / dev)
4. built-in defaults

Point `[quotes] source = "/path/to/quotes.json"` at your own file to use a
personal quote list.

## Run

Launch it at session start from `~/.config/bspwm/bspwmrc`:

```sh
pgrep -f bspwm-cyberquote-native >/dev/null || bspwm-cyberquote-native &
```

(`pgrep -f` matches the full command line — `/proc/<pid>/comm` is truncated to
15 chars, so a plain `pgrep -x` won't find it.)

If you'd rather use systemd, enable crash-restart + logs instead:

```sh
systemctl --user edit --full --force --runtime bspwm-cyberquote.service <<'EOF'
[Unit]
Description=bspwm-cyberquote-native desktop quote ticker
[Service]
Type=simple
Environment=DISPLAY=:0
ExecStart=/usr/bin/bspwm-cyberquote-native
Restart=always
RestartSec=2
[Install]
WantedBy=default.target
EOF
systemctl --user enable --now bspwm-cyberquote.service
```

(You still need `DISPLAY` right — start it from bspwmrc with
`systemctl --user start bspwm-cyberquote.service` if your display is not `:0`.)

## Development

```sh
cargo build --release    # link + release build
cargo test               # unit + integration + doctests
bash -n scripts/*.sh     # syntax-check shell scripts
```

System dependencies: `gtk3`, `glib2`, `pango`, `cairo`, plus `rust` and
`pkgconf` to build.

## License

MIT — see [LICENSE](LICENSE).