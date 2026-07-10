# git-ultrapager

English | [日本語](README-ja.md)

`git-ultrapager` is a streaming terminal pager for Git output. It keeps familiar
`less` navigation while adding diff-aware line numbers, syntax highlighting,
sticky file context, and fast file navigation.

> [!NOTE]
> Version 0.1.0 is under development. The documented interface is the target
> contract for the first release.

## Features

- Reads Git output from standard input and starts before the producer reaches EOF.
- Uses familiar `less` navigation, including `e`/`y` aliases for `j`/`k`.
- Shows old and new source line numbers and wraps long lines to terminal width.
- Highlights source syntax based on the changed path or a detected shebang.
- Highlights intraline changes while ignoring whitespace differences by default.
- Keeps the current file in a sticky header while scrolling.
- Opens a filterable, commit-grouped changed-file list with `Tab`.
- Searches Git logical lines up to 16 MiB with smart-case regular expressions.
- Falls back to a safe plain pager for unsupported or malformed input.
- Supports macOS and Linux terminals with UTF-8 output.

## Installation

The first release is installed from source:

```sh
cargo install --path .
```

Rust stable is required. Prebuilt releases, crates.io publishing, Homebrew, and
Windows support are outside the 0.1.0 scope.

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

Paging modes behave as follows:

- `auto`: print short output directly and open the interactive pager for longer
  or still-streaming output.
- `always`: open the interactive pager even for short, non-empty output.
- `never`: render directly to a terminal without opening the interactive UI.

When stdout is not a terminal, output is passed through without generated line
numbers, wrapping, or color. Incoming terminal control sequences are removed.

## Key bindings

Numeric prefixes repeat movements and structural navigation, such as `5j`, `3]`,
or `2}`.

| Action | Keys |
| --- | --- |
| Down one visual row | `j`, `e`, `Down`, `Enter`, `Ctrl-E`, `Ctrl-N` |
| Up one visual row | `k`, `y`, `Up`, `Ctrl-Y`, `Ctrl-P` |
| Down one page | `Space`, `f`, `PageDown`, `Ctrl-F`, `Ctrl-V` |
| Up one page | `b`, `PageUp`, `Ctrl-B` |
| Down half a page | `d`, `Ctrl-D` |
| Up half a page | `u`, `Ctrl-U` |
| First line | `g`, `Home` |
| Last loaded line / follow incoming data | `G`, `End` |
| Previous / next hunk | `[`, `]` |
| Previous / next file | `{`, `}` |
| Previous / next commit | `(`, `)` |
| Forward / backward search | `/`, `?` |
| Repeat / reverse the current search | `n`, `N` |
| Open or close the file list | `Tab` |
| Open help | `h`, `H` |
| Toggle whitespace comparison | `W` |
| Repaint the terminal | `Ctrl-L` |
| Suspend and resume with shell job control | `Ctrl-Z` |
| Close an overlay or cancel input | `Esc` |
| Quit | `q` |
| Interrupt and quit with status 130 | `Ctrl-C` |

Search uses Rust regular-expression syntax. A lowercase-only pattern is
case-insensitive; a pattern containing an uppercase character is case-sensitive.
Search previews matches while typing and shows the current match distinctly.
Weak highlights for other visible matches are bounded and may be omitted on dense
lines or when raw or escaped display text exceeds the preload limit; navigation to
the current match is unaffected.

The file list preserves Git's output order. Press `/` inside the list to fuzzy
filter displayed paths, use the navigation keys to select an entry, and press
`Enter` to jump to it.

## Display behavior

Interactive mode reserves one row for a sticky context header and one row for a
status bar. Diff source lines have separate old/new line-number columns. Long
lines hard-wrap at grapheme boundaries; continuation rows leave both number
columns blank and align with the source text. A single grapheme larger than 64 KiB
is split at Unicode scalar boundaries so one crafted cluster cannot require an
unbounded row buffer.

Incoming ANSI, OSC, and DCS sequences are stripped before display. Invalid UTF-8
and unsafe control bytes are shown as `\xNN` in interactive mode instead of being
sent to the terminal. Tabs expand to configurable tab stops.

The built-in dark and light themes degrade from true color to 256 or 16 colors as
needed. Color is never the only indication of addition, deletion, selection, or
search state.

## Supported Git output

Standard single-parent `git diff`, `git show`, and `git log -p` unified diffs get
the complete diff-aware display. Rename and copy entries retain both paths, and
the changed-file list groups repeated file occurrences by commit.

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

To exercise the large-input path with a release build, generate the documented
acceptance stream directly into the pager:

```sh
cargo build --release
scripts/generate-large-fixture.sh 5000000 | target/release/git-ultrapager --paging always
```

The generator writes about 200 payload bytes per logical line by default. Pass a
smaller line count and payload width while iterating locally.

## License

`git-ultrapager` is available under the [MIT License](LICENSE).
