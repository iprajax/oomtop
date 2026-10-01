# oomtop — UX specification

Companion to [SPEC.md](SPEC.md). Status: draft v0.2 · 2026-09-29

**v0.2 changes:** key map de-conflicted (`x` stop, `1–6` views, `i` why-here, `?` help only) · mode triggers per OS
and an OOM ETA in the headline · row reorders are no longer animated (extra frames conflicted with the perf budget;
stability rules do the job) · v1 learning trimmed to explicit/strong signals with fixed weights (auto-tuning, dwell,
scroll-past, local-LLM queries moved after 1.0) · query grammar defined once · state path unified on XDG ·
no emoji glyphs · contrast guard scoped to themes with known colors.
**v0.2.1 (2026-09-30):** htop-style meter block on top (per-core CPU with P/E tags, `Mem`/`Swp`/`GPU` bars,
Tasks / Load average / Uptime) and htop's function-key bar at the bottom (F1–F10 in every preset, F5 tree,
F6 sort-by picker); view tabs moved to a tab row under the header — owner feedback: "where are the CPUs at the
top, overall system health, and the bottom F-key mapping like htop".

> The screen should answer *"is my machine OK, and what do I care about right now?"* before you read a single number —
> and it should get better at that the longer you use it, on this machine, for this person.

---

## 1. Principles

1. **Answer first.** The top line is a sentence, not a gauge: *"Tight on memory — 1.2 GB headroom. sd-server holds
   9.9 GB. Two idle Gradle daemons could free 5.9 GB."*
2. **Personal, not generic.** What you open, pin, search and act on shapes what you see first. Nothing is hidden
   because of it — ranking only changes order and emphasis.
3. **Adaptive to the situation.** Healthy → calm and minimal. Memory pressure → memory and reclaim first.
   Throttling → thermals first. A model generating → its progress first.
4. **Stable, not jumpy.** Rows never reshuffle under your cursor; reorders are rate-limited, and a row that
   just moved carries a one-refresh marker instead of an animation.
5. **Explainable.** Every personalized placement answers *"why is this here?"* with one keypress.
6. **Local and inspectable.** Learning happens on-device, stores no secrets, and can be viewed, edited or reset.
7. **Keyboard-first, mouse-complete.** Every action by key; every visible thing clickable.
8. **Respect the terminal.** Truecolor → 256 → 16 → mono, light or dark background, 60–300 columns, screen readers.
9. **Cheap.** The UX layer (query + ranking + render) stays under 5 ms per frame and adds < 0.2 % CPU.

Visual language follows the owner's preference: **neutral surfaces, one accent color, no blue/purple defaults**.
Meaning is never carried by color alone (icons, labels and position repeat it).

## 2. Screen anatomy

Wide layout (≥ 140 columns):

```
┌──────────────────────────────────────────────────────────────────────────────────────────────────────────┐
│ E0[|||||           21.5%] E3[                 2.0%] P6[|||||||||       42.4%] P9[||              11.8%]  │
│ E1[|||             16.0%] E4[                 1.3%] P7[||||||||||||||||78.0%]                            │
│ E2[|                6.7%] E5[                 0.7%] P8[||||||||||||||||93.1%]                            │
│ Mem[||||||||||##################***=====20.1G/24.0G]  Tasks: 761, 3412 thr; 3 running                    │
│ Swp[||||||||||||||||||||||||||||||||||||||6.1G/7.0G]  Load average: 3.20 2.74 2.41                       │
│ GPU[|||||||||||||||||||||||||||||             58.0%]  Uptime: 58 days, 11:23:04                          │
│ Tight on memory — 1.2 GB headroom. sd-server holds 9.9 GB; 2 idle build daemons could free 5.9 GB.  [r]  │
│ MEM |apps 11.2 #gpu/metal 9.9 *compressed 1.6 =wired 3.0 .cache 3.8 free 0.1 avail 3.1                   │
│ SWAP 6.1 / 7.0 GB  ▲ 87% full  ▁▂▃▅▆▇  ▲ +420 MB/min   headroom 1.2G   PSI mem 31%                       │
│ oomtop  [1 Home] 2 Processes 3 Models 4 Sandboxes 5 Reclaim 6 Timeline    ▲ Pressure  72% speed  bat 29% │
├─ Your things ────────────────────────────────────────────────────────────────────────────────────────────┤
│ ★ sd-server · qwen-image-studio   9.9 GB  generating 3/6 · 8.1 s/step     ★ Claude Code ×4   1.4 GB      │
├─ Ranked ───────────────────────────────────────────────────────────────── /  filter · : command · i why ─┤
│ ▸ build daemons (2)      idle 5h   5.9 GB  ▁▁▁▁▁  reclaimable            because: idle cost · pressure   │
│ ▸ Google Chrome (14)               2.8 GB  ▂▃▂▂▃                           because: you opened it 9× wk  │
│ ▸ Claude desktop VM                1.5 GB  ▁▁▁▁▁  sandbox · started by Claude.app                        │
│ ▸ ChatGPT / Codex (6)              1.0 GB  ▁▁▂▁▁                                                         │
│   … 212 more                                                                                             │
├─ Details ────────────────────────────────────────────────────────────────────────────────────────────────┤
│ build daemons → GradleDaemon 9.8 (2.9 GB, idle 3h40m) · KotlinCompileDaemon (3.0 GB, idle 6h35m)         │
│ [x] stop both   [z] suspend   [m] mute kind   [p] pin   [i] why ranked here                              │
│F1Help   F2Setup  F3Search F4Filter F5Tree   F6SortBy F7Why    F8Reclaim F9Stop   F10Quit                 │
└──────────────────────────────────────────────────────────────────────────────────────────────────────────┘
```

**Meter block (htop-style, top of every view).** Per-core CPU bars `E0[|||||   21.5%]` in columns (P/E tags on
heterogeneous CPUs, plain indexes otherwise; the fill uses the ok/warn/crit state colors by load and the percentage
is always printed), then `Mem[…]` / `Swp[…]` / `GPU[…]` on the left and `Tasks: N, T thr; R running`,
`Load average: a b c`, `Uptime: …` on the right. The Mem bar is oomtop's truth, one character per segment so it
reads without color: `|` apps · `#` gpu/metal · `*` compressed · `=` wired · `.` cache (htop's monochrome
convention); the `MEM` row below is its legend with one number per segment. Four core columns in wide layouts,
two to four in standard ones (more when there are many cores), one aggregate `CPU[…]` bar in compact layouts or
before per-core data exists; terminals under 16 rows get a single `CPU[…] Mem[…] Swp[…]` row. `appearance.header`
without `"meters"` brings back the v0.2 header (segmented `MEM ▕…▏` bar, CPU/GPU mini-bars).

**Tab row** (htop's "Main" row): views `[1 Home] 2 Processes …` on the left (numbers only when the badges need
the room), machine name and state badges on the right. **Function-key bar** on the last line (§7).

Standard (80–139 cols): no Details pane (opens as an overlay); the accelerator/thermal row only while throttling.
Compact (< 80 cols): aggregate meters + headline + "Your things" + top 5 ranked, one metric column chosen by the
current mode. Short terminals drop the accelerator row (< 28 rows) and the memory legend (< 22 rows) first.
Every region is a **slot** the serving layer fills (§5.5) — layout is not hard-coded per screen size, it is ranked.

## 3. Situation modes (adaptive, not personalized)

| Mode | Trigger (with hysteresis) | What moves up |
|---|---|---|
| **Calm** | pressure normal, no throttle, swap flat | Headline "All good", Your things, compact list |
| **Pressure** | headroom < 0, or swap growing, or an OOM forecast (SPEC §8.3); plus Linux PSI mem some > 20 %, macOS memorystatus level ≥ warn | memory columns, idle cost, Reclaim suggestions, `can_fit` answers, forecast ETA |
| **Throttle** | throttle factor < 0.8 under load, thermal pressure ≥ heavy (macOS) / trip point hit (Linux), or Low Power Mode | temps/power/clock, battery, "why slow" evidence |
| **Working** | a model server or build is actively running | its progress, s/step or tok/s, ETA, its memory |
| **Leftovers** | orphans or idle > threshold detected | Orphans card with the sessions that spawned them |

Modes change emphasis and default sort only; they enter after a condition holds 10 s and leave after 30 s calm
(no flapping). The current mode is shown in the header and can be pinned (`M`).

## 4. Identity: making "things" stable

Personalization needs to remember *things*, not PIDs. Each process/group gets an **entity fingerprint**:

```
fingerprint = hash(kind, exe_basename, cmdline_template, project_root?, bundle_id?)
cmdline_template: argv with values normalized — paths → project-relative, numbers/ports/uuids/tokens → placeholders
  "python3 studio.py"                         → studio.py            (project: qwen-image-studio)
  ".../bin/sd-server --listen-port 7861 ..."   → sd-server --listen-port <n> --diffusion-model <file>
  "java ... org.gradle...GradleDaemon 9.8.0"   → GradleDaemon <ver>
```

- Entities survive restarts, version bumps and port changes; a display name is derived ("sd-server ·
  qwen-image-studio") and can be **renamed** by the user (`n`), which becomes an alias usable in queries.
- Group entities (e.g. "Claude Code sessions", "Chrome") aggregate member fingerprints.
- Secrets never enter fingerprints: tokens/keys/URLs with credentials are placeholder-ized before hashing; raw
  command lines are not stored.

## 5. The personalization pipeline

Framed as a small on-device recommender: **understand → retrieve → rank → serve → learn**.

### 5.1 Signals (what we learn from)

| Signal | Type | Weight hint |
|---|---|---|
| Select / expand an entity | implicit positive | medium |
| Search/filter that matches an entity, then selecting it | strong positive | high |
| Action taken (stop, suspend, pin, rename) | explicit positive | high |
| Pin (`p`) | explicit, persistent | always in "Your things" |
| Mute (`m`) / "show less like this" (`-`) | explicit negative | decays slowly (30 d) |
| Current mode, running kinds | context features | — |

After 1.0, if the Hit@3 metric (§5.6) shows a need: dwell time, scroll-past as a weak negative, time-of-day context.
These are noisy signals and make "why is this here?" harder to explain, so v1 leaves them out.

Stored per entity as **frecency** (as popularized by zoxide/atuin): `score = Σ w(event) · 0.5^(age / half_life)`,
half-life 7 days (configurable), plus context counters (e.g. "opened during Working mode").

### 5.2 Query understanding

One input bar (`/` filter, `:` command, `Ctrl-K` palette — same engine). It accepts three styles and mixes them:

1. **Plain words** → intent + entities: `gpu hogs`, `what's eating memory`, `claude`, `why slow`, `idle stuff`,
   `models`, `can I load 13g`.
2. **Structured filters**: `mem>2G kind:daemon idle>30m`, `gpu>1G`, `owner:"Claude Code"`, `sandbox:*`.
3. **Commands**: `:reclaim`, `:headroom 13G`, `:why`, `:pin sd-server`, `:mode pressure`.

**Filter grammar** (one definition, used by `/`, aliases, saved views and rules): terms separated by spaces are
AND; `or` is OR; `-term` or `not term` negates; parentheses group; `key:a,b` means a or b. Keys: `kind` (short
aliases from SPEC §5), `owner`, `name`, `mem`, `gpu`, `cpu`, `idle`, `state`, `sandbox`. Comparisons `> >= < <=` take
units (`2G`, `500M`, `30m`, `5h`). Example: `kind:model,agent or gpu>1G -muted`.

Pipeline (deterministic, local, < 1 ms):
- **Normalize** (case, units `13g → 13 GB`, typos via Damerau-Levenshtein ≤ 1 on known vocabulary).
- **Intent classification** by rules + keyword lexicon: `find`, `rank_by(metric)`, `explain`, `headroom`,
  `reclaim`, `navigate(view)`. Synonyms: *hog/eating/using/biggest* → rank_by(memory); *slow/lag/throttle* → explain.
- **Entity linking** against names, aliases and user renames (fuzzy, fzf-style scoring), boosted by frecency so
  `cl` resolves to *your* most-used "Claude Code" rather than "clang".
- **Output**: `{intent, filters, sort, target_entities, confidence}`; low confidence → palette shows the top 3
  interpretations as choices instead of guessing.
- **Learned completion**: the palette suggests your past queries (frecency-ranked) and entity names as you type.
- After 1.0, optional and off by default: a local LLM adapter for free-form questions, fed only the redacted
  snapshot.

### 5.3 Retrieval (candidate generation)

Per frame, candidates come from several generators, each cheap:
- **All groups/processes** in the snapshot (the full index — nothing is ever unreachable).
- **Insight generators** → cards: swap growth, new large process, throttle onset, orphan detected, idle cost above
  threshold, model finished/failed, headroom change for a pinned model.
- **Your things**: pinned + top-affinity entities currently running (or recently ended, dimmed).
- **Query matches** when a filter/command is active.
- **Saved views** (named queries: `:save gpu-work`).

### 5.4 Ranking

Each candidate gets a score, recomputed every refresh but applied with hysteresis:

```
score = w_s · salience        # share of memory/GPU/CPU, weighted by current mode; anomaly z-score of change rate
      + w_a · affinity         # frecency of this entity for this user (+ context match: same mode / time of day)
      + w_x · actionability    # reclaimable bytes, idle cost, fixable cause ("why" evidence)
      + w_q · query_match      # when a query is active, dominates
      − w_n · noise            # muted, recently dismissed, system-noise priors
```

- **Defaults** `w_s=1.0, w_a=0.6, w_x=0.5, w_q=3.0, w_n=1.0`; mode changes re-weight salience features
  (Pressure boosts memory/idle cost; Throttle boosts power/temps).
- **Diversity (MMR)** so one app with 30 helpers doesn't fill the list; groups are ranked, members nest.
- **Stability**: a row moves only if its score beats the row above by a margin for ≥ 2 refreshes; the selected row
  never moves; a row that moved shows a `›` marker for one refresh (no animation frames, so no extra CPU).
- **Exploration**: new entities get a small, decaying novelty boost so the ranking doesn't lock onto old habits.
- **Cold start**: priors from the **machine profile** (§6) — e.g. model servers and agent CLIs start with a boost
  on a machine where they're detected; everything else uses salience only.

### 5.5 Serving (what goes where)

| Slot | Filled by | Rules |
|---|---|---|
| Headline sentence | top insight, else mode summary | template-generated, ≤ 1 line, includes the one best action key |
| Your things strip | pinned + top-3 affinity running entities | always visible when width allows; max 4 chips |
| Ranked list | ranking output | groups first, members nested; "because …" hint on the right in wide layouts |
| Details pane | selected entity | members, history sparkline, owner chain, actions, "why ranked here" |
| Toasts | insights about *your things* or high severity only | max 1 per 30 s; never steals focus |
| Palette suggestions | query completion + insights + actions | keyboard-first, fuzzy |

Latency: ranking + layout ≤ 5 ms; render uses diffing and **synchronized output** so frames never tear.

### 5.6 Learning loop & evaluation

- Every served list logs (locally) *impression → selection* pairs, so the UX can measure itself:
  - **Hit@3** — was the entity you selected in the top 3 without typing?
  - **Time/keystrokes to target** — from opening oomtop to selecting what you wanted.
  - **Query reformulation rate** — did you have to retype?
  - **Dismiss/mute rate** on served insights.
- `oomtop profile stats` shows these. In v1 the weights are fixed defaults that the user can override in config,
  which keeps rankings reproducible and explainable. Bounded online auto-tuning (e.g. raise `w_a` when Hit@3 is low
  but queries keep targeting high-affinity entities) waits until after 1.0, behind a flag.
- No remote telemetry. The log is capped (e.g. 30 days / 5 MB) and prunable.

## 6. Machine & user profile (cold start and adaptation)

Built on first run and refreshed daily; used as priors, visible in `oomtop profile`:
- **Hardware**: unified memory vs discrete VRAM, RAM, core counts, GPU vendor(s), battery/fanless (thermal-prone).
- **Detected roles**: local-LLM/diffusion (model servers, gguf/safetensors files), agent-heavy (agent CLIs),
  JVM/Android dev (Gradle/Kotlin daemons), web dev (node/tsserver/browsers), container user (Docker/OrbStack).
- **Adaptations**: which columns exist (no GPU column on a machine without a usable GPU), default safety margin
  (larger on fanless/unified machines), thresholds for "idle", units, which insight generators are enabled.
- **User preferences**: theme/accent, density (compact/comfortable), mouse on/off, reduced motion, pinned/muted.

Storage: `$XDG_STATE_HOME/oomtop/` (default `~/.local/state/oomtop/`) on **both** OSes, matching the config
choice in §12.2. It is SQLite, owned by `oomtop-state`, and is shared with the lineage journal (SPEC §6.2).
`oomtop profile show | export | reset`, and a `--no-learn` flag / config switch.

## 7. Interaction design

**Keys** (vim-style + arrows; all shown in `?`; the presets have no conflicts, checked by
`oomtop keys list --conflicts` in CI):

| Area | Keys |
|---|---|
| Move | `↑↓` / `jk` move · `enter` / `→` / `l` expand · `←` / `h` collapse · `g`/`G` top/bottom |
| Views | `1` Home · `2` Processes · `3` Models · `4` Sandboxes · `5` Reclaim · `6` Timeline · `tab` next |
| Find | `/` filter · `:` command · `Ctrl-K` palette |
| Act (always confirm) | `x` stop · `z` suspend/resume (CPU relief only) |
| Shape | `p` pin · `m` mute · `-` show less · `n` rename · `u` undo · `M` pin mode |
| Look | `i` why ranked here · `w` watch (focus mode) · `c` compare two · `,` settings · `?` help · `q` quit |
| Function keys | `F1`–`F10`, see the bar below (every preset) |

**Function-key bar** (last line, htop's): `F1 Help · F2 Setup (settings) · F3 Search (palette) · F4 Filter ·
F5 Tree · F6 SortBy · F7 Why (ranked here) · F8 Reclaim (view 5) · F9 Stop (confirms, like `x`) · F10 Quit`.
Bound in every preset; the labels are read from the active keymap, so a remapped F-key shows its new action and an
unbound one an empty slot. Each item is clickable. Labels are drawn in the selection style (reverse video in the
`terminal` theme — nothing is painted). Narrow terminals get shorter labels (`F2Set F3Find F4Filt …`), then drop
Why, Reclaim, Setup, Tree… first; `F10 Quit` always stays.
- **F5 Tree**: Processes view → parent/child tree with `├─` / `└─` branches (siblings keep the current sort; a
  process whose parent is not listed is a root); Home → expand / collapse every group.
- **F6 SortBy**: a picker over the command palette (`:sort …`) listing this view's sort keys, the current one
  preselected and marked; `↑↓` + `enter` applies.

htop users can pick `keys = "htop"`: the same bar, and `k` also means stop.

**Mouse**: click to select/expand, double-click details, scroll lists, click header meters to jump to the related
view, drag the timeline scrubber. Terminal hyperlinks (OSC 8) open a process's cwd, log or model file.

**Confirmations** are inline (the row turns into "Stop GradleDaemon (2.9 GB)? y / n"), never modal walls; destructive
actions support `u` undo for pins/mutes/renames (not for kills — those always confirm).

**Focus mode (`w`)**: one entity full-screen — memory breakdown over time, child tree, throttle context, model
progress (for model servers), and its "cost to keep running".

**Timeline (`t`)**: last 10 minutes (ring buffer) with markers for insights ("swap +2 GB", "throttle start",
"Gradle started"); scrub to see the ranked list as it was.

## 8. Visual system

(Themes, color tokens, settings and customization: see §12.)

- **Color**: neutral surface; one accent for focus/selection; semantic states use a small palette (ok / warn / critical)
  with icons (●, ▲, ■) and text. Background detection via OSC 11; light and dark palettes; `NO_COLOR` respected.
- **Glyphs**: Unicode blocks and braille sparklines; optional Nerd Font icons with ASCII fallback (`--ascii`).
  No emoji: their width varies between terminals and breaks alignment. Width is measured with `unicode-width`
  and checked in snapshot tests.
- **Motion**: none that needs extra frames. A changed value or a moved row is marked for one refresh (bold or `›`).
  There is no blinking and no tweening.
- **Density**: comfortable (default) and compact; font-size independent — layout is column-count driven.
- **Accessibility**: `--plain` mode emits a linear, screen-reader-friendly summary; every color meaning has a text twin.
- **Consistency**: numbers right-aligned, units always shown, memory in the same unit per column.

## 9. Headline & explanation generation

Template-based, deterministic, localized later:
```
[mode summary] — [key number]. [top owner + amount]; [best action + gain].
"Tight on memory — 1.2 GB headroom. sd-server holds 9.9 GB; 2 idle build daemons could free 5.9 GB."
"Running at 45% speed — thermal pressure heavy, on battery (29%). Plug in or pause generation."
"Swap full in ~6 min at this rate — sd-server holds 9.9 GB; 2 idle build daemons could free 5.9 GB."
"All good — 11 GB free. Your things: sd-server idle, 4 Claude Code sessions."
```
"Why ranked here" (`?` on a row) lists the top score contributions in plain words:
*"3rd: holds 2.8 GB (salience) · you opened it 9× this week (affinity) · 14 processes grouped."*

## 10. Technology

- **ratatui + crossterm**; Kitty keyboard protocol when available; synchronized output (DEC mode 2026);
  OSC 8 hyperlinks; OSC 11 background query; mouse SGR mode.
- Query engine and ranker in `oomtop-core` (pure functions over a snapshot + profile) → fully unit-testable with
  fixtures; golden tests for ranking stability and headline text.
- Profile store in SQLite (WAL, `oomtop-state`); writes batched every 30 s and on exit.

## 11. UX acceptance tests

1. On the motivating machine, first run with no history: the headline names memory pressure and the idle build
   daemons; reclaiming them is one keypress + confirm.
2. After 5 sessions of opening sd-server first, it appears in "Your things" and Hit@3 ≥ 0.8 for it.
3. Typing `cl` selects the user's Claude Code group, not an unrelated `clang` process.
4. `gpu hogs`, `what's eating memory` and `mem>2G` produce the expected intent/filters (golden tests).
5. Rows under the cursor never move; no row changes position more than once per 4 s without a real score change.
6. With `NO_COLOR` and 80 columns, every state is still distinguishable; `--plain` output reads correctly with VoiceOver.
7. `oomtop profile reset` returns ranking to salience-only defaults.
8. With the default `terminal` theme, switching the terminal's own theme (light↔dark, any palette) re-colors oomtop
   correctly with no oomtop config change, and no background is painted.
9. Editing `config.toml` while running applies within 1 s; an invalid edit shows file:line and keeps the old config.
10. Saving from the settings screen preserves user comments and key order (golden file test).
11. `oomtop theme import` of a base16 scheme produces a theme that passes `oomtop theme check`.
12. `oomtop keys list --conflicts` is empty for the `default`, `vim` and `htop` presets.
13. With swap growing steadily in a replayed fixture, the headline shows the OOM ETA and the best reclaim action
    (golden text); on a fixture with a single spike it doesn't.

## 12. Appearance, themes & settings — the terminal way

### 12.1 Philosophy

1. **Your terminal is the theme.** The default theme, `terminal`, uses only the 16 ANSI colors plus the terminal's
   default foreground/background, and **never paints a background**. Whatever you set up in Ghostty, iTerm2, Kitty,
   WezTerm, Alacritty, Terminal.app, tmux or over SSH — palette, transparency, blur, font, ligatures — oomtop looks
   native in it, and follows your terminal when you switch its light/dark theme.
2. **Text files are the source of truth.** Config, themes, keymaps, layouts and rules are TOML files in XDG paths,
   diff-able, commentable, and dotfile/Nix/chezmoi friendly. The in-app settings screen edits those same files and
   **preserves your comments and ordering**.
3. **Layered, predictable precedence** (later wins):
   `built-in defaults → /etc/oomtop/ → ~/.config/oomtop/config.toml → config.d/*.toml (alphabetical) →
   config.d/host-<hostname>.toml → OOMTOP_* env vars → CLI flags → runtime toggles (not persisted unless saved)`.
   `oomtop config print --effective --origin` shows every value and which layer set it.
4. **Semantic tokens, not raw colors.** Widgets ask for `mem.gpu` or `state.warn`, never "yellow"; themes map
   tokens to colors. That's what makes one theme work everywhere and makes custom themes small.
5. **Degrade gracefully.** truecolor → 256 → 16 → monochrome (bold/dim/underline/reverse only). Detected from
   `COLORTERM`, terminfo and a query; overridable. `NO_COLOR` is always respected.

### 12.2 Files and paths

```
~/.config/oomtop/                 ($XDG_CONFIG_HOME; macOS also honors it, falling back to ~/.config)
  config.toml                     main settings (created commented-out by `oomtop config init`)
  config.d/*.toml                 drop-in overrides; host-<hostname>.toml for per-machine settings
  themes/<name>.toml              user themes (override built-ins of the same name)
  keymap.toml                     key remapping
  layouts/<name>.toml             panel/column layouts
  rules.d/*.toml                  detection rules (see SPEC)
~/.local/state/oomtop/            profile, learning, history (never needed to reproduce your setup)
```

All files are **live-reloaded** on save (file watcher, debounced 200 ms); invalid files keep the last good
config and show the error inline with file:line and a fix hint.

### 12.3 Theme tokens

```
[ui]        surface, text, muted, faint, border, border.focus, accent, accent.text, selection, selection.text
[state]     ok, warn, crit, info, stale
[mem]       app, gpu, compressed, wired, cache, free, swap
[kind]      agent, model, sandbox, daemon, app, system, other   (short aliases, SPEC §5)
[chart]     spark, spark.peak, bar.fill, bar.track
[text]      number, unit, label, key, link, headline
```
Each token = color + optional attributes: `{ fg = "3", bold = true }`, `{ fg = "#ffb547" }`, `{ fg = "bright-black",
dim = true }`, `{ reverse = true }`. Colors may be ANSI index/name (`"3"`, `"yellow"`, `"bright-white"`),
`"default"` (terminal fg/bg), `"#rrggbb"`, or a reference to another token (`"@ui.accent"`).

### 12.4 Built-in themes

| Theme | Colors | Notes |
|---|---|---|
| `terminal` **(default)** | ANSI 16 + default fg/bg, transparent | accent = bold default fg; ok/warn/crit = green/yellow/red; no blue/magenta by default |
| `mono` | truecolor neutral greys + white accent | the Gemini-like neutral look; light & dark variants |
| `ember` · `mint` · `sand` · `coral` | neutral + one accent | same as `mono`, different accent |
| `high-contrast` | ≥ 7:1 contrast everywhere | bold borders, no dim text |
| `colorblind` | deuteranopia/protanopia-safe ramp | state also encoded by glyphs ●▲■ (always, in every theme) |
| `none` | monochrome | attributes only; used automatically when `NO_COLOR` is set |

Every truecolor theme ships **light and dark variants**; `appearance = "auto"` picks by querying the terminal
background (OSC 11), falling back to `COLORFGBG`, then dark.

**Import & share:** `oomtop theme import scheme.yaml` converts **base16/base24** schemes (and iTerm2
`.itermcolors`, Ghostty/Kitty/Alacritty theme files) into an oomtop theme, mapping semantic tokens automatically;
review the result with `oomtop theme preview <name>`. Community themes (Gruvbox, Catppuccin, Nord, Tokyo Night,
Solarized, Rosé Pine…) are just files in `themes/`.

**Contrast guard** (only for themes with known colors, i.e. hex values; the `terminal` theme's colors are whatever
the user's terminal defines, so there is nothing to check): themes are checked at load (WCAG ratio text/surface ≥ 4.5:1, state colors ≥ 3:1); failing
tokens are auto-nudged in lightness and reported by `oomtop theme check`.

Example user theme (`~/.config/oomtop/themes/night-ember.toml`):
```toml
name = "night-ember"
inherits = "mono"            # only override what differs
appearance = "dark"

[ui]
accent     = { fg = "#ffb547", bold = true }
selection  = { bg = "#2a2a2e" }
border     = { fg = "#2b2b30" }

[mem]
gpu        = { fg = "#ff8a6b" }
swap       = { fg = "@state.warn", underline = true }
```

### 12.5 Appearance settings (config.toml)

```toml
[appearance]
theme        = "terminal"      # or mono, ember, …, or a user theme name
appearance   = "auto"          # auto | dark | light
color        = "auto"          # auto | truecolor | 256 | 16 | none
background   = "transparent"   # transparent | theme  (paint the theme surface)
glyphs       = "unicode"       # unicode | nerd | ascii
borders      = "rounded"       # rounded | plain | thick | double | none
density      = "comfortable"   # comfortable | compact
motion       = "marks"         # marks (one-refresh change markers) | none
sparklines   = "braille"       # braille | blocks | none
header       = ["meters", "headline", "memory", "swap", "accelerators", "thermal"]   # order = display order

[format]
memory_units = "iec"           # iec (GiB) | si (GB)
decimals     = 1
cpu          = "per-core"      # per-core (100% = 1 core) | total (100% = machine)
time         = "relative"      # relative ("idle 5h") | clock
```

### 12.6 Layout & columns customization

Layouts are declarative; the ranked serving slots (§5.5) fill them.
```toml
# ~/.config/oomtop/layouts/llm-dev.toml
name = "llm-dev"
[wide]                     # ≥ 140 cols; also [standard] and [compact]
rows = ["header", "your-things", { split = ["ranked:65%", "details:35%"] }]
[columns.groups]
show  = ["name", "footprint", "gpu", "trend", "state", "because"]
width = { name = "flex", footprint = 9, gpu = 8, trend = 12 }
sort  = "rank"             # rank (personalized) | footprint | gpu | cpu | name
```
Custom columns via small expressions over entity fields (sandboxed, no I/O):
```toml
[[columns.custom]]
id = "mem_share"
title = "MEM%"
expr = "footprint / host.mem.total * 100"
format = "{:.0}%"
```

### 12.7 Keys, commands, views

```toml
# keymap.toml — any action can be rebound; chords and leader keys supported
[global]
"ctrl-k" = "palette"
"g g"    = "view:groups"
"K"      = "stop"            # add a second binding for stop
"g g"    = "move:top"
[details]
"o"      = "open:cwd"        # OSC 8 / open in Finder / xdg-open
```
Presets: `keys = "default" | "vim" | "emacs" | "htop"` (htop-compatible F-keys: F3 search, F6 sort, F9 kill…).
Command aliases and saved views are config too:
```toml
[aliases]
hogs = ":sort footprint desc"
llm  = "kind:model,agent"
[[views]]
name = "gpu-work"
query = "gpu>500M or kind:model"
layout = "llm-dev"
```

### 12.8 In-app settings (`,`)

A settings screen for people who don't want to edit files — but it's a **view over the files**, not a separate store:
- Sections: Appearance · Layout · Columns · Keys · Behavior (refresh, thresholds, safety margin) · Personalization
  (learning on/off, half-life, reset) · Privacy.
- Every row shows the **effective value, its origin** (`default`, `config.toml:12`, `host-air.toml:3`, `env`, `flag`)
  and whether it's overridden elsewhere.
- **Live preview**: changing theme/accent/borders/density re-renders instantly; `Esc` reverts, `Enter` applies for
  the session, `s` saves to the chosen layer (config.toml, a host file, or a new drop-in) using comment-preserving edits.
- **Theme picker** shows all themes rendered on the *current* screen, filterable, with a contrast badge.
- `e` opens the underlying file at the right line in `$EDITOR`.

### 12.9 CLI for configuration

```
oomtop config init                 # write a fully commented config.toml (every option, default values)
oomtop config edit [--host]        # open config (or this host's override) in $EDITOR
oomtop config print --effective [--origin]
oomtop config validate             # schema check all files, with file:line errors
oomtop config set appearance.theme ember [--layer host|user|dropin:<name>]
oomtop theme list | preview <name> | check <name> | import <file> | export <name>
oomtop keys list [--conflicts]
```
Env vars mirror keys: `OOMTOP_APPEARANCE__THEME=ember`, plus `OOMTOP_CONFIG=/path` and `OOMTOP_NO_LEARN=1`.
A JSON Schema for all files is published (editor autocomplete/validation via taplo/VS Code).

### 12.10 Terminal compatibility

- Detect capabilities once at start (truecolor, Kitty keyboard protocol, synchronized output, OSC 8/11, mouse SGR,
  focus events) and adapt; `oomtop doctor` prints what was detected and why something looks off.
- **tmux/screen**: passthrough-aware queries, `set -g allow-passthrough on` hint, correct behavior with
  `default-terminal "tmux-256color"`; **SSH**: no assumptions about local fonts/colors.
- Honors `NO_COLOR`, `CLICOLOR_FORCE`, `TERM=dumb` (falls back to `--plain`), and terminal focus (pauses heavy
  sampling when unfocused/hidden, optional).
- No custom fonts, no images required; Nerd Font glyphs are opt-in.

## 13. Inspirations

btop (visual density, theme files), k9s (resource-type commands), lazygit (context panes), fzf (fuzzy matching),
zoxide / atuin (frecency ranking of personal history), Raycast / command palettes (one input for search + actions), Helix/Zellij/Ghostty (TOML config, live reload,
theme ecosystems), base16/tinted-theming (portable palettes).

## 14. Implementation decisions (v0.1.0 build, 2026-09-30)

Where the first build had to decide something this document leaves open; the code and its snapshot tests are the
reference.

**Query understanding (§5.2)**
- Quoted text is literal (`"clean up"` is a name, not an intent); a comma inside quotes does not split terms.
  Also supported: `is:`/`type:` filters, `mem:>2G`-style comparisons, `!term` negation and globs.
- Typo correction: words of 5+ characters use Damerau–Levenshtein ≤ 1; 4-letter words are fixed only by an
  adjacent transposition; a word that is a prefix of an entity-name token is never corrected (typing "clan…"
  toward "Clangd" must not become "clean").
- `:headroom 13` without a unit is an error ("add a unit to 13, e.g. 13G or 13GB"); `13g` is GiB (units follow
  `units.rs`: K/M/G = 1024ⁿ, KB/MB/GB = 1000ⁿ).
- Palette labels of non-Find interpretations name their top target ("explain slowdown · sd-server"); a strong
  intent word (score ≥ 0.75) ranks plain "go to <entity>" 0.2 below it instead of tying.

**Ranking and serving (§5.4, §5.5, §9)**
- The OOM-forecast headline shows in every mode (top insight first); the header still shows the latched mode.
- Context match: `RankContext.mode_affinity` adds up to `CONTEXT_AFFINITY_WEIGHT` for entities opened in the
  current mode, explained as "N× in <mode> mode". The TUI does not feed per-mode counters yet (open item).
- Novelty is gated in the TUI: a group under 1 % of RAM and 1 % of the CPUs gets no novelty boost.
- After swap growth stops, the forecast can linger about 150 s (the linear fit still spans the window); recorded
  in the `growth_then_plateau` golden.

**Screen (§2, §7, §8)**
- Home layouts (§12.6) drive the Home slots; other views keep their built-in structure (Processes uses
  `[columns.processes]` when a layout defines it). The Models view lists model servers, then the model files on
  disk ("On disk": size, format, name, loaded by / last used, duplicates).
- Labels are disambiguated when two groups share one ("Claude Code · 2abf"); another user's app is labeled
  "<App> (uid N)".
- Stop flow: SIGTERM after `y`; SIGKILL is offered only when a sample taken after the SIGTERM still shows the
  target. While a confirmation is pending a mouse click cancels it and scrolling is ignored. The prompt says when
  a model server is running a job, has queued requests, or reports idle only heuristically.
- Focus lost slows sampling 4× (it does not pause), so the timeline keeps collecting.
- A background-only token that cannot be painted (transparent, `NO_COLOR`, `color = "none"`) becomes REVERSED
  so the selection stays visible.
- Not implemented yet: the "headroom change for a pinned model" insight, OSC 8 hyperlinks (ratatui cells can't
  carry them without breaking width math), a filterable theme picker, profile reset from the settings screen.
- The calm headline's "N free" is the resolved available-now value headroom uses (own-cgroup cap, the tighter
  macOS memorystatus view), so the first sentence never contradicts the header's headroom.
- The header names the machine from the hardware identifier + chip ("Mac17,3" + "Apple M5" → "MacBook Air M5",
  a small table in `oomtop_core::machine`); unknown identifiers stay visible ("Apple M9 (Mac99,1)") rather than
  guessing a family.
- Swap is colored by state, not category: at ≥ 80 % of its limit (swap total; the swap-volume ceiling on macOS,
  where swap grows on demand) the swap row adds "▲ 87% full" (■ at ≥ 95 %); growth carries ▲. When the row runs
  out of width the (decorative) swap sparkline is dropped before GPU/CPU.
- Models and Sandboxes tables drop whole low-priority columns at standard widths (Models: endpoint, KV, device,
  weights, speed, then job; Sandboxes: runtime, config, kind, started-by, then guest). MEM / HOST and the
  name column are never dropped, and no value is clipped mid-number.
- Deviations from §2/§5.5/§8 kept on purpose (80–139 columns): the headline may wrap to 2 lines (3 below 80 —
  a one-line template would drop the action and the owner, which is the answer); MEM and SWAP keep separate
  lines at standard width (one merged line leaves no room for the legend's numbers, and every segment must stay
  identifiable without color); compact (< 80) lists every ranked row that fits, not only 5 (scrolling a longer
  list beats hiding it); memory columns use `format_bytes_short` per value (G/M) rather than one unit per
  column — a per-column unit turns 22 MB into "0.0G" next to a 9.9 GB server. Each is covered by the
  60/80/120/200 snapshots.

**Personalization (§5, §6)**
- Entity linking uses the same cold-start priors as the ranker (agent CLIs and model servers detected on this
  machine) while an entity has no frecency: with no history `cl` resolves to the Claude Code session, not an
  unrelated `clang`. Any learned frecency replaces the prior.
- The headline's best action is a keymap action, `headline-action` (bound to `r` in every preset); the `[key]`
  hint shows the key actually bound in the active keymap and disappears when it is unbound, so `keys list
  --conflicts` sees it.

**Config, themes, keys (§12)**
- A system-layer file under `/etc/oomtop` shows its full path as origin; user files keep the short labels.
- An empty table in a higher layer means "no change" (`aliases = {}` in a drop-in does not clear lower layers);
  clear with `oomtop config set`/`unset` in the right layer.
- Keymap presets bind `t` → `view:timeline`. `"x" = "none"` in a non-global context cannot hide a global binding
  (the lookup falls back to global).
- `terminal` theme: `mem.wired` is default fg + dim + italic (visible on light terminals, distinct from
  `mem.cache` without color). Category tokens never reuse the state colors: `mem.swap` is italic, `mem.gpu` bold,
  `kind.model` bold italic, `mem.free` default fg; green/yellow/red mean ok/warn/crit only.
- ASCII glyphs have one meaning each: select `>`, moved `+`, pin `*`, lower bound `>=`, approx `~`, ellipsis
  `..`. `--ascii` also reaches plain / screen-reader output (`—` → `-`, `·` → `|`, `≈` → `~`, `≥` → `>=`).
- The settings screen's "overridden elsewhere" marker (`also in file:line`) names a file only when it sets that
  exact key (or the inline table / array holding it) — a `[table]` header alone doesn't count.
- `config validate` checks theme structure only (unknown tokens, invalid colors/references); contrast is
  `oomtop theme check`'s job because loading auto-nudges it. The no-blue/purple rule applies to shipped themes.
- `general.watch_poll_ms` values of 1–99 are raised to 100 ms.
- `appearance = "auto"` resolves OSC 11 background → `$COLORFGBG` → dark (`Variant::resolve`); the CLI probes the
  terminal once and hands the result to the TUI for later reloads and previews. The probe waits on `/dev/tty`
  with `select(2)` (macOS `poll(2)` on a tty device returns `POLLNVAL` at once), and its `CSI ? u` answer replaces
  crossterm's own Kitty-keyboard query, which waits up to 2 s on terminals that never answer DA1.
- Late probe answers (high-latency SSH, slow terminals): the probe gives up after 200 ms (first frame < 300 ms),
  so answers can arrive once the TUI already reads keys. crossterm drops the CSI answers itself but reads OSC/DCS
  strings as keystrokes (`ESC ] 11;rgb:…` = Alt+`]`, `1`, `1`, `;`, … and the `:` opened the palette). When the
  probe saw no DA1, the TUI arms a filter for 10 s that swallows `Alt+] digit … BEL|ST` and `Alt+P digit … ST`
  bursts and hands back anything that turns out not to be one (a real Alt+`]` still works). Pty test:
  `pty_late_replies` (answers delayed 0/150/250/400/1500 ms; `q` must quit).
- tmux: queries are not wrapped in DCS passthrough; doctor relies on tmux's own answers and prints the
  `allow-passthrough` hint.
