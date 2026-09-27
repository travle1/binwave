# BinWave

BinWave is a Rust + egui desktop app for inspecting and decoding binary files. Users open a file, see it as a signal plot, hex view and table, and write a **decoder** (YAML, Python or CSV) that describes the file's structures. Windows is the main target; macOS and Linux are also supported.

## Proprietary data: never commit

Some files I test with are proprietary. They, and anything that reveals their format, must never enter git history.

- **Never commit:**
  - Proprietary data files (anything matching the private patterns in `.gitignore`, or kept in `private/`).
  - Decoders that describe proprietary formats.
  - Tests, fixtures, snapshots, benchmarks or screenshots that use or are derived from those files.
  - Design references. `docs/design/binwave-egui-v2-design/` is **local reference only** (gitignored); never commit it or add proprietary details to it.
- **Don't leak details** of proprietary formats (field names, structure names, magic bytes, offsets, sample values, file names) into code, comments, test names, docs, commit messages or logs.
- **Tests and demos use synthetic data only**, such as the PWAV waveform fixture and its generator in `samples/generators/`. If a test needs a new format feature, create a new synthetic fixture that exercises it.
- **Keep proprietary files in a gitignored location** (for example `private/`), and keep `.gitignore` entries for them. Never remove or weaken those entries.
- **Before every commit**, run `git status` and `git diff --cached --stat`. If anything proprietary is staged, unstage it and tell me. Never use `git add -A` or `git add .`; add files by name.
- **If you find proprietary content already tracked or in history**, stop and tell me. Don't rewrite history on your own.
- **If you're unsure whether a file is proprietary, treat it as proprietary and ask.**

## Before you change code

- Read the design reference in `docs/design/binwave-egui-v2-design/` (local only, never commit it): `DESIGN_SPEC.md`, `design-tokens.json`, `mockups/` and `TASKS.md`. Mockups use the app's current palette; plot values and bracketed placeholders are illustrative.
- Check versions in `Cargo.toml`. Don't upgrade or add crates without asking first, and name the exact version you'd use.
- For large UI changes (a whole panel or screen), propose a short plan and wait for approval before editing.

## Working rules

- One numbered task = one commit, with a short descriptive message. Stop after each task so I can review.
- Before committing, run `cargo build` and `cargo clippy`, and `cargo test` if tests exist; fix new warnings. Then verify: a file opens, decoding runs, and plot, hex view, table, editor and docking still work.
- Change only what the task asks. Keep decoding logic and file handling untouched unless the task is about them.
- If something in the spec is unclear or not possible in egui, stop and ask rather than guessing.

## Styling rules

- **All colors, radii, spacing and font sizes live in the theme module**, under the token names from `design-tokens.json`. Widgets read tokens; no hex values or magic numbers in widget code. A light theme and theme switching will be added later, so this matters.
- **Palette: keep the current neutral grays and periwinkle accent.** Reference values measured from the app:

  | Token | Value |
  | --- | --- |
  | surface.sidebar | `#181818` |
  | surface.window (gaps, wells) | `#1C1C1C` |
  | surface.card | `#232323` |
  | surface.raised (inspector, zebra rows) | `#292929` |
  | surface.control (buttons) | `#303030` |
  | surface.active (active tab) | `#353535` |
  | selection / Run / selected segment | `#424765` |
  | text.primary | `#EEEDEA` |
  | text.secondary | `#C4C3C1` |
  | text.muted | `#959491` |
  | accent | `#8C9BFF` |
  | success | `#5FD0BC` |
  | warning | `#F0B45E` |
  | syntax.key / syntax.comment | `#8CC2EE` / `#82908B` |

  If code and this table disagree, the code's existing colors win. Tell me.
- Fonts: Geist (interface) and Geist Mono (bytes, offsets, values, code).
- Shape: rounded everywhere. Window 14 px, cards 12 px, controls and tabs 8 px, table rows 6–7 px. 8 px gaps between dock cards.
- No inline help paragraphs; put instructions in tooltips or an ⓘ icon.

## Terminology

- **Decoder**: the YAML/Python/CSV file. Never "definition".
- **Structure** / **type**: things defined inside a decoder ("Decoder types (7)").
- **Samples**: only the records collection. Other rows are **items**.
- Panels: Signal, Hex, Inspector (inside Hex), Table, Decoder, Structure, Console. Screens: start screen, Decoder library.

## Formatting data

- Thousands separators for decimal counts and sizes shown to people (31,249,935). Hex offsets as 8-digit `0x00000030`, matching the Hex view.
- Sizes human-readable (B / KiB / MiB / GiB) with exact bytes on hover.
- Ranges are inclusive with size: `0x60–0x6F · 16 B`. Never "end exclusive".
- Enum fields show value and meaning from the decoder's `cases`: `3 · f64`.
- Correct singular/plural ("1 byte"); never "(s)".
- Numbers right-aligned in mono in tables.

## Window and layout decisions

- Custom rounded window frame, identical on all platforms: our own header, caption buttons, drag area and edge resizing. If the OS can't do transparent windows, fall back to square corners with the same frame. Maximized windows are square.
- Collapsible sidebar (Ctrl B), with state remembered between launches.
- Docked panels via egui_dock. Users must always be able to drag, split and redock. Tab close buttons show on hover, or via right-click → Close and Ctrl W.
- Decoder file name appears in one place only: the Decoder panel tab, next to Run (F5).
- Recent files: one entry per full path, showing the parent folder when names collide.
