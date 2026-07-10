# git-ultrapager

[English](README.md) | 日本語

`git-ultrapager` は、Git 出力を扱うストリーミング対応の terminal pager です。
使い慣れた `less` の移動操作を保ちながら、diff 用の行番号、syntax highlight、
sticky な file context、高速な file navigation を追加します。

> [!NOTE]
> version 0.1.0 は開発中です。この文書の interface は最初の release で満たす
> contract を示しています。

## 機能

- Git 出力を標準入力から読み、producer が EOF に達する前に起動します。
- `j`/`k` と同じ働きをする `e`/`y` を含め、使い慣れた `less` の移動操作を使えます。
- old/new の source 行番号を表示し、長い行を terminal 幅で折り返します。
- 変更 path または検出した shebang に応じて source syntax を highlight します。
- whitespace 差分を default で無視しながら、行内の変更箇所を highlight します。
- scroll 中の file を sticky header に表示します。
- `Tab` で filter 可能な commit group 付き変更 file 一覧を開きます。
- Git 出力全体を smart case の正規表現で検索します。
- 未対応または壊れた入力を安全な plain pager で表示します。
- UTF-8 出力に対応する macOS と Linux の terminal を support します。

## Install

最初の release は source から install します。

```sh
cargo install --path .
```

Rust stable が必要です。prebuilt release、crates.io への publish、Homebrew、
Windows support は 0.1.0 の scope 外です。

## Git の設定

Git の default pager として設定します。

```sh
git config --global core.pager git-ultrapager
```

program 自身は global Git config を変更しません。設定を削除する command は
次のとおりです。

```sh
git config --global --unset core.pager
```

## Command-line interface

`git-ultrapager` は標準入力だけを受け取ります。file を指定する positional argument
と `less` の command-line option には対応しません。

```text
-h, --help
-V, --version
--theme dark|light
--no-color
--paging auto|always|never
--tab-width 1..16
--whitespace ignore|show
```

default は `--theme dark`、`--paging auto`、`--tab-width 4`、
`--whitespace ignore` です。`NO_COLOR` は `--no-color` と同じく色を無効にします。

paging mode は次のように動きます。

- `auto`: 短い出力は直接表示し、長い出力または streaming 中の出力では
  interactive pager を開きます。
- `always`: 空ではない短い出力でも interactive pager を開きます。
- `never`: interactive UI を開かず、terminal へ直接描画します。

stdout が terminal ではない場合、生成した行番号、折り返し、色を追加せずに
出力を通過させます。入力に含まれる terminal control sequence は除去します。

## Key binding

数値 prefix は `5j`、`3]`、`2}` のように、移動と構造 navigation を繰り返します。

| 操作 | Key |
| --- | --- |
| 1 visual row 下 | `j`, `e`, `Down`, `Enter`, `Ctrl-E`, `Ctrl-N` |
| 1 visual row 上 | `k`, `y`, `Up`, `Ctrl-Y`, `Ctrl-P` |
| 1 page 下 | `Space`, `f`, `PageDown`, `Ctrl-F`, `Ctrl-V` |
| 1 page 上 | `b`, `PageUp`, `Ctrl-B` |
| 半 page 下 | `d`, `Ctrl-D` |
| 半 page 上 | `u`, `Ctrl-U` |
| 先頭行 | `g`, `Home` |
| 読み込み済み末尾 / 入力追従 | `G`, `End` |
| 前 / 次の hunk | `[`, `]` |
| 前 / 次の file | `{`, `}` |
| 前 / 次の commit | `(`, `)` |
| 前方 / 後方検索 | `/`, `?` |
| 現在方向 / 逆方向への検索反復 | `n`, `N` |
| file 一覧を開く / 閉じる | `Tab` |
| help を開く | `h`, `H` |
| whitespace 比較を切り替える | `W` |
| terminal を再描画する | `Ctrl-L` |
| shell の job control で停止 / 再開する | `Ctrl-Z` |
| overlay を閉じる / 入力を取り消す | `Esc` |
| 終了する | `q` |
| 中断して status 130 で終了する | `Ctrl-C` |

検索には Rust の正規表現 syntax を使います。小文字だけの pattern は大文字と
小文字を区別せず、大文字を含む pattern は区別します。入力中に一致箇所を preview
し、表示中の全一致を highlight したうえで、現在の一致を別の表示にします。

file 一覧は Git の出力順を保ちます。一覧内で `/` を押すと表示 path を fuzzy
filter できます。navigation key で entry を選び、`Enter` で移動します。

## 表示動作

interactive mode は sticky context header に1行、status bar に1行を使います。
diff の source 行には old/new の行番号列を別々に表示します。長い行は grapheme
境界で hard wrap し、継続 row は両方の行番号列を空にして source text の開始位置へ
揃えます。

入力の ANSI、OSC、DCS sequence は表示前に除去します。不正な UTF-8 と安全でない
control byte は terminal へ送らず、interactive mode では `\xNN` と表示します。
tab は設定した tab stop へ展開します。

組み込みの dark/light theme は terminal に応じて true color、256色、16色へ
degrade します。追加、削除、選択、検索状態を色だけで表すことはありません。

## 対応する Git 出力

標準的な single-parent の `git diff`、`git show`、`git log -p` の unified diff
には、diff 向け表示をすべて適用します。rename/copy entry は両方の path を保持し、
変更 file 一覧では同じ file の出現を commit ごとに group 化します。

combined diff、word diff、submodule 出力、binary patch、壊れた section、custom
external-diff format は保守的な fallback で扱います。構造を認識できる範囲では
検索、折り返し、sticky context、file navigation を使えますが、その section の
source 行番号、syntax highlight、intraline highlight は無効になる場合があります。

`git log --graph -p`、`--color-words`、Git の color-moved annotation、mouse 入力、
folding、side-by-side layout、Git 以外の一般的な patch parsing、設定 file は
0.1.0 では対応しません。

## 開発

code change を提出する前に quality gate を実行します。

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
```

詳細な design contract は [docs/design-ja.md](docs/design-ja.md) にあります。
README の内容を変更するときは、この file と `README.md` を一緒に更新します。

## License

`git-ultrapager` は [MIT License](LICENSE) で提供します。
