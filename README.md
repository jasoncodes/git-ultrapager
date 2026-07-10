# git-ultrapager

English | [日本語](README-ja.md)

`git-ultrapager` is a streaming terminal pager for Git output. It keeps familiar
`less` navigation while adding diff-aware line numbers, syntax highlighting,
sticky file context, and fast file navigation.

## Features

- Reads Git output from standard input and starts before the producer reaches EOF.
- Uses familiar `less` navigation, including `e`/`y` aliases for `j`/`k`.
- Shows old and new source line numbers and wraps long lines to terminal width.
- Highlights source syntax based on the changed path or a detected shebang.
- Highlights intraline changes while ignoring whitespace differences by default.
- Marks commit and file boundaries with context headers, stacking the current commit above the current file while scrolling.
- Opens a filterable, commit-grouped changed-file list with `Tab`.
- Searches Git logical lines up to 16 MiB with counted, smart-case regular
  expressions.
- Falls back to a safe plain pager for unsupported or malformed input.
- Supports macOS and Linux terminals with UTF-8 output.

## Installation

Install v0.1.0 from source:

```sh
cargo install --path .
```

Rust 1.96 or newer is required. CI also tests the current stable toolchain.
Prebuilt releases, crates.io publishing, Homebrew, and Windows support are
outside the 0.1.0 scope.

## Git configuration

Configure it as Git's default pager:

```sh
git config --global core.pager git-ultrapager
```

The program does not modify global Git configuration itself. Remove the setting
with:

```sh
git config --global --unset core.pager
```

## Command-line interface

`git-ultrapager` accepts input only from standard input. Positional file arguments
and `less` command-line options are not supported.

```text
git-ultrapager [OPTIONS]

-h, --help
-V, --version
--theme dark|light
--no-color
--paging auto|always|never
--tab-width 1..16
--whitespace ignore|show
```

The defaults are `--theme dark`, `--paging auto`, `--tab-width 4`, and
`--whitespace ignore`. `NO_COLOR` has the same color-disabling effect as
`--no-color`.

An empty pipe exits successfully without output. Invoking the program with a TTY
as stdin prints a usage hint and exits with status 2 instead of waiting for input.
Unknown options, positional arguments, and invalid values are also usage errors
with status 2.

With terminal stdout, paging modes behave as follows:

- `auto`: render complete output shorter than one terminal screen directly. Open
  the interactive pager when the rendered output reaches one screen, or when the
  producer is still running 100 ms after the first byte arrives.
- `always`: open the interactive pager for every non-empty input, including short
  completed output.
- `never`: stream decorated, wrapped output directly without entering the
  alternate-screen UI.

Interactive mode reads commands from the controlling TTY while stdin remains the
Git output stream. It therefore requires a controlling TTY; `auto` falls back to
direct rendering if one cannot be opened, while `always` exits with status 1.
When stdout is not a terminal, or when `TERM` is unset, empty, or `dumb`, all paging choices use sanitized pass-through:
no generated line numbers, wrapping, or color, and incoming ANSI/OSC/DCS control
sequences are removed.

## Key bindings

In the pager body, numeric prefixes repeat row, page, structure, and search
commands, such as `5j`, `3]`, `2}`, `4/word`, or `3n`.

| Action | Keys |
| --- | --- |
| Down one visual row | `j`, `e`, `Down`, `Enter`, `Ctrl-E`, `Ctrl-N` |
| Up one visual row | `k`, `y`, `Up`, `Ctrl-Y`, `Ctrl-P` |
| Down one page | `Space`, `f`, `PageDown`, `Ctrl-F`, `Ctrl-V` |
| Up one page | `b`, `PageUp`, `Ctrl-B` |
| Down half a page | `d`, `Ctrl-D` |
| Up half a page | `u`, `Ctrl-U` |
| Beginning of input | `g`, `Home` |
| Loaded end / follow incoming data | `G`, `End` |
| Input logical line N | `Ng`, `NG` |
| Previous / next hunk | `[`, `]` |
| Previous / next file | `{`, `}` |
| Previous / next commit | `(`, `)` |
| Forward / backward search | `/`, `?` |
| Repeat / reverse the current search | `n`, `N` |
| Open the file list | `Tab` |
| Open help | `h`, `H` |
| Toggle whitespace comparison | `W` |
| Repaint the terminal | `Ctrl-L` |
| Suspend and resume with shell job control | `Ctrl-Z` |
| Close an overlay or cancel input | `Esc` |
| Quit | `q` |
| Interrupt and quit with status 130 | `Ctrl-C` |

Search uses Rust regular-expression syntax. A pattern with no uppercase Unicode
scalar is case-insensitive; a pattern containing one is case-sensitive.
It matches the full logical line before wrapping. A count before `/` or `?`
selects the Nth match in that direction; counts also apply to `n` and `N`, and
multiple matches on one line count separately. Search does not wrap at BOF or
EOF. A forward search can wait for streaming input, and `Esc` cancels that wait.

Search previews valid patterns while typing and shows the current match
distinctly. `Enter` commits the pattern, `Esc` restores the pre-search viewport,
and Up/Down browse shared `/` and `?` history. Left/Right, `Ctrl-B`/`Ctrl-F`,
Home/End, `Ctrl-A`/`Ctrl-E`, Backspace/Delete, `Ctrl-W`, and `Ctrl-U` edit search
and filter input by grapheme. Other printable keys, including `q`, are text while
an input field is active. Committing an empty input reruns the previous pattern in
the requested direction; `n` follows its committed direction and `N` reverses it.

After ANSI removal and unsafe-byte escaping, logical lines larger than 16 MiB
remain visible but are skipped by regex search. The status bar reports the number
as `skipped:N`. Weak highlights for other visible matches are bounded and may be
omitted on dense or oversized display lines; navigation to the current match is
unaffected.

`Ng` and `NG` go to the Nth input logical line; wrapped continuation rows do not
count. If that line has not arrived yet, the pager waits for it and `Esc` cancels
the request. Bare `G` instead moves to the loaded end and follows incoming data.

The changed-file list preserves Git output order rather than sorting by match
score. Press `/` to apply a case-insensitive subsequence filter to decoded paths.
The list retains a selection-centered window of at most 5,000 stable file IDs
while still reporting the total match count. The unfiltered list includes empty
commit groups as `(no file changes)`; a path filter naturally excludes them.

Outside filter input, `j`/`k`, `e`/`y`, arrows, page keys, and `g`/`G` or
Home/End move the file selection; `Enter` jumps and closes the list. During filter
input, only Up/Down and PageUp/PageDown move the selection. The first `Esc` closes
the input and restores the unfiltered list; `Tab`, `Esc`, or `q` closes the list
when no input is active. Help uses the same row/page and `g`/`G` scrolling; `Esc`,
`q`, `h`, or `H` closes help without quitting the pager.

## Display behavior

Interactive mode reserves one row for a sticky commit context, adds a second
sticky row while inside a file, and reserves one row for the status bar. The
same headers appear inline before each commit and file section, then move into
the sticky stack when they reach the top. Diff source lines have separate
old/new line-number columns. Long lines hard-wrap at grapheme boundaries;
continuation rows leave both number
columns blank and align with the source text. A single grapheme larger than 64 KiB
is split at Unicode scalar boundaries so one crafted cluster cannot require an
unbounded row buffer.

Incoming ANSI, OSC, and DCS sequences are stripped before display. Invalid UTF-8
and unsafe control bytes are shown as `\xNN` in interactive mode instead of being
sent to the terminal. Tabs expand to configurable tab stops.

The built-in dark and light themes degrade from true color to 256 or 16 colors as
needed. Color is never the only indication of addition, deletion, selection, or
search state.

In the default `ignore` whitespace mode, intraline pairing and emphasis ignore
leading and trailing ASCII spaces and tabs, and normalize each interior run of
ASCII spaces or tabs to one separator. `foo bar` and `foobar` remain different;
other Unicode whitespace is compared as ordinary text. Diff lines are never
hidden by this mode—use Git's `-w` when the lines themselves should be excluded.

`W` toggles the comparison for the current session, and `--whitespace show`
starts with exact whitespace comparison. In `show` mode, changed whitespace spans
render spaces as `·` and tabs as `→` followed by `·` cells; unchanged whitespace
and context lines retain their normal display.

## Supported Git output

Standard single-parent `git diff`, `git show`, and `git log -p` unified diffs get
the complete diff-aware display. Rename and copy entries retain both paths, and
the changed-file list groups repeated file occurrences by commit. Standard
`commit <object-id>` headers create groups even when a commit has no changed file.

Combined diffs, word diffs, submodule output, binary patches, malformed sections,
and custom external-diff formats use a conservative fallback. Search, wrapping,
sticky context, and file navigation remain available when their structure can be
recognized, but source line numbers, syntax highlighting, or intraline highlighting
may be disabled for that section.

`git log --graph -p`, `--color-words`, Git color-moved annotations, mouse input,
folding, side-by-side layout, generic non-Git patch parsing, and configuration
files are not supported in 0.1.0.

## Development

Run the quality gate before submitting code changes:

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
```

The detailed design contract is in [docs/design-ja.md](docs/design-ja.md) (Japanese).
When README content changes, update this file and `README-ja.md` together.

The full test command includes the actual-Git PTY end-to-end test and a reduced
25,000-line spill/viewport acceptance test. For a manual large-input end-to-end
run, send the default 5,000,000-line, 200-byte-payload workload through the
release pager itself:

```sh
cargo build --release
scripts/generate-large-fixture.sh 5000000 200 | target/release/git-ultrapager --paging always
```

For repeatable parser, spill, and cached/cold viewport-collection measurements,
the profile harness defaults to 5,000,000 lines, a 200-byte payload, and 101
samples:

```sh
scripts/profile-large-fixture.sh
```

A smaller local run uses the same three positional parameters:

```sh
scripts/profile-large-fixture.sh 25000 96 21
```

The profile script creates the fixture, report, and an isolated release target
directory under `/tmp`, then prints their paths. Its 16 ms cached and 50 ms cold
viewport-collection targets are informational labels; timing does not determine
the exit status. The timed region is `ViewportLayout::collect` only, not the
end-to-end path from a key event through terminal redraw.

## License

`git-ultrapager` is available under the [MIT License](LICENSE).
