# git-ultrapager

[English](README.md) | 日本語

`git-ultrapager` は、Git の出力を扱うストリーミング対応のターミナルページャーです。
`less` に近い操作のまま、diff の行番号、シンタックスハイライト、sticky file header、変更ファイル一覧を利用できます。

## 機能

- Git の出力を標準入力から読み、producer が EOF に達する前に起動します。
- `j` / `k` と同じ働きをする `e` / `y` を含め、`less` に近いキーで移動できます。
- old / new のソース行番号を表示し、長い行をターミナル幅で折り返します。
- 変更パスまたは shebang からファイル種別を判定し、ソースコードをハイライトします。
- デフォルトでは ASCII の空白とタブの差を比較から除外し、行内差分を強調します。
- commit と file の境界に context header を表示し、スクロール中は現在の commit の下に file を積んで画面上部に固定します。
- `Tab` で、commit ごとにまとまった検索可能な変更ファイル一覧を開きます。
- 16 MiB 以下の Git logical line を smart-case の正規表現で検索します。
- 未対応または壊れた入力は、安全な plain pager へ切り替えて表示します。
- UTF-8 を扱える macOS と Linux のターミナルに対応します。

## インストール

v0.1.0 はソースからインストールします。

```sh
cargo install --path .
```

Rust 1.96 以上が必要です。
CI では現在の stable toolchain も検証します。
prebuilt release、crates.io、Homebrew、Windows は v0.1.0 の対象外です。

## Git の設定

Git のデフォルトページャーとして設定する場合は、次のコマンドを実行します。

```sh
git config --global core.pager git-ultrapager
```

`git-ultrapager` 自身は global Git config を変更しません。
設定を削除するコマンドは次のとおりです。

```sh
git config --global --unset core.pager
```

## コマンドラインインターフェース

`git-ultrapager` は標準入力だけを受け取ります。
ファイルを指定する positional argument と、`less` のコマンドラインオプションには対応しません。

```text
git-ultrapager [OPTIONS]

--theme dark|light
--no-color
--paging auto|always|never
--tab-width 1..16
--whitespace ignore|show
-h, --help
-V, --version
```

デフォルトは `--theme dark`、`--paging auto`、`--tab-width 4`、`--whitespace ignore` です。
`NO_COLOR` は `--no-color` と同じく色を無効にします。

各 paging mode は次のように動作します。

- `auto`: 最初の byte を受け取った後、表示行がターミナル高に達するか、100 ms が経過すると interactive pager を開き、EOF まで短いままなら直接表示します。
- `always`: 空ではない短い出力でも interactive pager を開きます。
- `never`: interactive UI を開かず、ターミナルへ直接描画します。

空入力は mode にかかわらず何も表示せず、status 0 で終了します。
stdin が TTY の状態で直接起動すると利用例を表示し、status 2 で終了します。
interactive mode は、Git の出力を stdin から読みながら controlling TTY からキー入力を受け取ります。
controlling TTY を開けない場合、`auto` は direct rendering へ切り替わり、`always` は status 1 で終了します。
stdout が TTY でない場合と、`TERM` が未設定、空、または `dumb` の場合は、生成した行番号、折り返し、色を加えず sanitized text を出力します。
未知の option、positional argument、不正な値は status 2 の usage error です。

## キーバインド

数値 prefix は、`5j`、`3]`、`2}` のように移動と構造 navigation を繰り返します。
`/`、`?`、`n`、`N` では N 番目の一致を選びます。

| 操作 | キー |
| --- | --- |
| 1 visual row 下 | `j`, `e`, `Down`, `Enter`, `Ctrl-E`, `Ctrl-N` |
| 1 visual row 上 | `k`, `y`, `Up`, `Ctrl-Y`, `Ctrl-P` |
| 1 page 下 | `Space`, `f`, `PageDown`, `Ctrl-F`, `Ctrl-V` |
| 1 page 上 | `b`, `PageUp`, `Ctrl-B` |
| 半 page 下 | `d`, `Ctrl-D` |
| 半 page 上 | `u`, `Ctrl-U` |
| 先頭行 | `g`, `Home` |
| N 番目の logical input line | `Ng`, `NG` |
| 読み込み済み末尾 / 入力追従 | `G`, `End` |
| 前 / 次の hunk | `[`, `]` |
| 前 / 次の file | `{`, `}` |
| 前 / 次の commit | `(`, `)` |
| 前方 / 後方検索 | `/`, `?` |
| 現在方向 / 逆方向への検索反復 | `n`, `N` |
| file 一覧を開く / 閉じる | `Tab` |
| help を開く | `h`, `H` |
| whitespace 比較を切り替える | `W` |
| ターミナルを再描画する | `Ctrl-L` |
| shell の job control で停止 / 再開する | `Ctrl-Z` |
| overlay を閉じる / 入力を取り消す | `Esc` |
| 終了する | `q` |
| 中断して status 130 で終了する | `Ctrl-C` |

検索には Rust の正規表現 syntax を使います。
大文字を含まない pattern は case-insensitive、大文字を含む pattern は case-sensitive です。
折り返し前の logical line 全体を照合し、同じ行の複数の一致も別々に数えます。
`/`、`?`、`n`、`N` へ付けた count は、その方向の N 番目の一致を選びます。
検索中は有効な pattern の一致を preview し、`Up` / `Down` で `/` と `?` が共有する履歴を移動できます。
`Enter` で pattern を確定し、`Esc` で検索開始前の viewport へ戻ります。
編集には `Left` / `Right`、`Ctrl-B` / `Ctrl-F`、`Home` / `End`、`Ctrl-A` / `Ctrl-E`、`Backspace` / `Delete`、`Ctrl-W`、`Ctrl-U` を使えます。
検索または filter の入力欄が active な間は、`q` を含む printable key が文字として入力されます。
検索は BOF / EOF で wrap しません。
forward search は streaming input の到着を待ち、`Esc` で待機を取り消せます。
空の入力を `Enter` で確定すると直前の pattern を現在の方向で再実行し、履歴がなければ `no previous search` と表示します。
16 MiB を超える logical line は表示したまま検索だけを省略し、件数を status に表示します。
ほかの表示中の一致に付ける弱い highlight は上限付きですが、現在の一致への navigation には影響しません。

`Ng` と `NG` は入力全体の N 番目の logical line へ移動し、折り返しの継続 row は数えません。
対象の行がまだ届いていない場合は到着を待ち、`Esc` で待機を取り消します。
count のない `G` は読み込み済みの末尾へ移動し、後続の入力に追従します。

file 一覧は Git の出力順を保ちます。
同じ path が複数の commit に現れた場合は、別の occurrence として表示します。
file change のない commit は、unfiltered list に commit header と `(no file changes)` を表示します。
`/` は path に対する case-insensitive の subsequence filter であり、relevance 順には並べ替えません。
一致総数は保持したまま、選択位置の周囲に最大5,000件の stable file ID を読み込みます。

unfiltered list では本文と同じ row / page key と `g` / `G` で選択し、`Enter` で対象へ移動して一覧を閉じます。
filter 入力中の `j` / `k` / `e` / `y` も query へ入り、選択には `Up` / `Down` / `PageUp` / `PageDown` を使います。
最初の `Esc` は filter を消して unfiltered list へ戻り、次の `Esc` は一覧を閉じます。
filter 入力が active でない場合は、`Tab`、`Esc`、`q` のいずれでも一覧を閉じます。
help は本文と同じ row / page / half-page key と `g` / `G` でスクロールし、`Esc`、`q`、`h`、`H` で閉じます。

## 表示動作

interactive mode は sticky commit context に1行、file 内ではその下にもう1行、status bar に1行を使います。
同じ header を各 commit section と file section の直前にも表示し、画面上端へ達すると sticky stack が引き継ぎます。
diff のソース行には old / new の行番号列を別々に表示します。
長い行は grapheme 境界で hard-wrap し、継続 row は両方の行番号列を空にして本文の開始位置へ揃えます。
1 grapheme が64 KiBを超える場合は、1 row の buffer が無制限に増えないよう Unicode scalar 境界で分割します。

入力の ANSI、OSC、DCS sequence は表示前に除去します。
不正な UTF-8 と安全でない control byte はターミナルへ送らず、interactive mode では `\xNN` と表示します。
tab は設定した tab stop へ展開します。

デフォルトの `--whitespace ignore` は、intraline の pairing と highlight で ASCII space / tab の差を無視します。
行頭と行末の ASCII space / tab を除外し、行中の連続部分は1つの区切りとして比較します。
`foo bar` と `foobar` は別の内容として扱い、ほかの Unicode whitespace は通常の文字として比較します。
Git が出した `-` / `+` 行自体は隠しません。
whitespace-only の diff 行を除外する場合は、Git 側で `-w` を指定します。
`W` または `--whitespace show` に切り替えると、変更された space を `·`、tab を `→` の後ろに残りの展開 cell 数だけ `·` を続けて表示します。

組み込みの dark / light theme は、ターミナルに応じて true color、256色、16色へ degrade します。
追加、削除、選択、検索状態を色だけで表すことはありません。

## 対応する Git 出力

標準的な single-parent の `git diff`、`git show`、`git log -p` の unified diff には、diff 向け表示をすべて適用します。
rename / copy entry は両方の path を保持し、変更ファイル一覧では occurrence を commit ごとにまとめます。

combined diff、word diff、submodule 出力、binary patch、壊れた section、custom external-diff format は保守的な fallback で扱います。
構造を認識できる範囲では検索、折り返し、sticky context、file navigation を利用できますが、その section のソース行番号、syntax highlight、intraline highlight は無効になる場合があります。

`git log --graph -p`、`--color-words`、Git の color-moved annotation、mouse 入力、folding、side-by-side layout、Git 以外の一般的な patch parsing、設定ファイルは v0.1.0 では対応しません。

## 開発

code change を提出する前に、次の quality gate を実行します。

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
```

詳細な設計仕様は [docs/design-ja.md](docs/design-ja.md) にあります。
README の内容を変更するときは、このファイルと `README.md` を一緒に更新します。

full test には actual Git を使う PTY end-to-end test と、25,000行で raw / structural spill と viewport 上限を検証する縮小 acceptance test が含まれます。

release build で end-to-end の大容量経路を操作する場合は、fixture を pager へ直接流します。

```sh
cargo build --release
scripts/generate-large-fixture.sh 5000000 200 \
  | target/release/git-ultrapager --paging always
```

parse、spill、cached / cold viewport collection latency を再現可能な形で測るには、profile harness を使います。

```sh
# デフォルト: 5,000,000行、payload 200 byte、101 sample
scripts/profile-large-fixture.sh

# 小さな local run
scripts/profile-large-fixture.sh 25000 96 21
```

fixture、report、release target は、command が表示する `/tmp/git-ultrapager-profile.*` directory に残ります。
report には parse time、raw / structural spill、cached / cold viewport collection の p50 / p95 / max、cache / read counter が含まれます。
16 ms / 50 ms は情報として示す目標値であり、hardware 差を理由に harness が失敗することはありません。
計測範囲は `ViewportLayout::collect` だけであり、key event からターミナル再描画までの end-to-end latency は含みません。

## ライセンス

`git-ultrapager` は [MIT License](LICENSE) で提供します。
