# Themes

**Your terminal is the theme.** The default, `terminal`, uses only the 16 ANSI colors plus your terminal's default
foreground and background, and never paints a background — so oomtop looks native in Ghostty, iTerm2, Kitty,
WezTerm, Alacritty, Terminal.app, tmux or over SSH, and follows your terminal when it switches light/dark.

Widgets never ask for "yellow"; they ask for a **semantic token** (`mem.gpu`, `state.warn`) and the theme maps
tokens to styles. That is why one theme works everywhere and custom themes stay small. Meaning is never carried by
color alone: state also has a glyph (● ▲ ■) in every theme.

## Commands

```console
$ oomtop theme list                    # built-ins and your themes (current one marked)
$ oomtop theme preview ember           # every token rendered in this terminal, plus a mock header
$ oomtop theme preview ember --variant light --color 256
$ oomtop theme check my-theme          # unknown tokens/colors, blue/purple defaults, contrast (exit 1 on issues)
$ oomtop theme import gruvbox.yaml     # base16/base24, iTerm2 .itermcolors, Ghostty, Kitty, Alacritty
$ oomtop theme export mono -o ~/.config/oomtop/themes/my-mono.toml   # start from a built-in
$ oomtop config set appearance.theme my-mono
```

Try one without saving: `oomtop --theme ember`.

## Built-in themes

| Theme | Colors | Notes |
|---|---|---|
| `terminal` (default) | 16 ANSI + default fg/bg | accent = bold default fg; ok/warn/crit = green/yellow/red; no background, no blue/magenta |
| `mono` | truecolor neutral greys, white accent | light and dark variants |
| `ember`, `mint`, `sand`, `coral` | neutral surfaces + one accent | light and dark variants |
| `high-contrast` | ≥ 7:1 contrast | no dim text |
| `colorblind` | deuteranopia/protanopia-safe ramp | glyphs carry state too |
| `none` | no color | attributes only; used automatically when `NO_COLOR` is set |

Truecolor themes pick their **light or dark variant** from `appearance.appearance` (`auto | dark | light`). With
`auto`, oomtop asks the terminal for its background color (OSC 11) once at start; if the terminal doesn't answer
(some multiplexers), it uses the dark variant. `oomtop doctor` shows what was detected. In tmux,
`set -g allow-passthrough on` lets the query reach the outer terminal.

Color depth degrades truecolor → 256 → 16 → none automatically (`COLORTERM`, `TERM`), or set
`appearance.color` / `--color`. `NO_COLOR` always wins.

## Tokens

```
[ui]     surface, text, muted, faint, border, border.focus, accent, accent.text, selection, selection.text
[state]  ok, warn, crit, info, stale
[mem]    app, gpu, compressed, wired, cache, free, swap
[kind]   agent, model, sandbox, daemon, app, system, other
[chart]  spark, spark.peak, bar.fill, bar.track
[text]   number, unit, label, key, link, headline
```

Each token is a style: `{ fg = "3", bold = true }`, `{ fg = "#ffb547" }`, `{ fg = "bright-black", dim = true }`,
`{ reverse = true }`. Keys: `fg`, `bg`, `bold`, `dim`, `italic`, `underline`, `reverse`.

Colors:

| Form | Example | Meaning |
|---|---|---|
| ANSI name | `"yellow"`, `"bright-black"` | your terminal's palette entry |
| ANSI index | `"3"`, `"208"` | 0–15 palette, 16–255 xterm cube (dropped at 16-color depth) |
| `"default"` | | the terminal's own foreground/background |
| hex | `"#ffb547"` | truecolor; downsampled to the nearest 256/16 color when needed |
| reference | `"@ui.accent"` | another token's fg (or bg, when used as `bg`) |

## Writing a theme

`~/.config/oomtop/themes/night-ember.toml`:

```toml
name = "night-ember"
inherits = "mono"            # only override what differs; missing tokens come from the parent, then `terminal`
appearance = "dark"          # pin a variant (omit to follow appearance.appearance)

[ui]
accent     = { fg = "#ffb547", bold = true }
selection  = { bg = "#2a2a2e" }
border     = { fg = "#2b2b30" }

[mem]
gpu        = { fg = "#ff8a6b" }
swap       = { fg = "@state.warn", underline = true }
```

Dotted token names inside a section are quoted: `"border.focus" = { fg = "@ui.accent" }`.

A user theme with a built-in's name overrides it (and may `inherits = "mono"` to extend the original). Themes are
live-reloaded on save.

### Rules `theme check` enforces

- Every token and color is valid; inheritance has no cycles.
- **No blue/purple defaults** for accents and states (a design rule of the project); pick them deliberately in a
  user theme if you want them — `check` reports them so you know.
- **Contrast** for themes with hex colors: text tokens ≥ 4.5:1 against the surface, state colors ≥ 3:1 (WCAG).
  Failing tokens are nudged in lightness at load and reported; the `terminal` theme's colors are whatever your
  terminal defines, so there is nothing to check.
- Backgrounds are only painted when `appearance.background = "theme"`; the default `transparent` keeps your
  terminal's background (and its transparency/blur).

## Importing terminal schemes

`oomtop theme import FILE [--name NAME] [--force] [--print]` maps a terminal palette to tokens (neutral surfaces from
the background ramp, one accent from the scheme's orange/yellow, states from its green/yellow/red), runs the
contrast guard, and writes `themes/<name>.toml`. Supported: base16/base24 YAML (legacy and tinted-theming),
iTerm2 `.itermcolors`, Ghostty theme files, Kitty `.conf`, Alacritty TOML. Review with `oomtop theme preview`.
