# git-ultrapager v0.1.0 設計仕様

## 1. 目的と境界

`git-ultrapager` は Git が標準出力へ生成した text を標準入力から受け取る pager である。
Git command 自体は起動せず、repository や working tree の file も参照しない。
標準の `git diff`、`git show`、`git log -p` が生成する unified diff を解析し、それ以外の入力は plain text として表示する。

v0.1.0 は macOS と Linux の UTF-8 対応 terminal を対象とする。
Windows、file argument、mouse capture、folding、side-by-side layout、generic unified diff parser、設定 file、plugin API は対象外とする。
package、crate、実行 file の名称は `git-ultrapager`、version は `0.1.0`、Rust edition は2024、`rust-version` は1.96とする。
Rust 1.96以上を対象にし、CIでは current stableも検証する。
1.96より古いtoolchainへの互換性は維持しない。

## 2. 起動と出力経路

stdin reader は background thread で読み込みと incremental parse を続け、main thread は terminal event、state update、描画を担当する。
最初の byte を受け取った後、次のいずれかで paging mode を決める。

- EOF までの rendered row が terminal 高さより1行以上少なければ直接表示する。
- 本文がその高さを超えたら interactive UI を開く。
- 100 ms以内にどちらにも達しなければ interactive UI を開く。
- 0 byte のままなら EOF まで待ち、mode 指定にかかわらず何も出さず status 0 で終了する。

`--paging auto` は上記判定を使う。
`always` は空ではない入力で interactive UI を開き、`never` は TTY へ直接描画する。
direct rendering は file entry 単位で buffer し、行番号 gutter と追加・削除 count を確定してから出力する。
末尾 newline が入力になくても TTY direct output には1つ付け、shell prompt との連結を防ぐ。

stdout が TTY でなければ、行番号、折り返し、色を追加せず、入力から terminal control sequence だけを除いて出力する。
raw byte と末尾 newline の有無は維持する。
downstream の `BrokenPipe` は message を出さず status 0、それ以外の read/write error は status 1とする。

`TERM=dumb` または `TERM` 未設定なら TTY でも plain passthrough にする。
stdout が TTY でも controlling TTY を開けない場合、`auto` と `never` は direct output、`always` は status 1とする。

## 3. Terminal lifecycle

interactive UI は alternate screen と raw mode を使い、終了時に cursor、screen、input mode を復元する。
`q` は stdin を drain せず status 0で終了し、upstream へ closed pipe を伝える。
`Ctrl-C` は mode を問わず terminal を復元して status 130で終了する。

`SIGHUP`、`SIGTERM`、`SIGQUIT` は signal handler から self-pipe または channel へ event を送り、main loop が復元後に `128 + signal` で終了する。
`SIGKILL` と process crash の復元は保証しない。
release でも panic は unwind し、top-level `catch_unwind` で復元して diagnostic を stderr へ出す。
worker panic は task failure event に変換し、その計算だけを fallback させる。

`Ctrl-Z` と外部 `SIGTSTP` は同じ suspend path を使う。
terminal を復元し、foreground process group に属するときは group を、そうでなければ pager 自身を停止する。
`SIGCONT` 後に raw mode と alternate screen を再構築し、保持した viewport を再描画する。

bracketed paste を有効にする。
navigation、help、file list では paste を command として実行せず無視する。
search/filter input では text として挿入し、newline、CR、control byte は可視 escape へ変換する。
mouse event mode は有効にしない。

## 4. 入力の sanitization と encoding

入力済みの ANSI、OSC、DCS sequence は除去し、Git 側の color に意味を依存しない。
interactive 表示では不正 UTF-8 byte と newline/tab 以外の C0/C1 control byte を `\xNN` と表示する。
CRLF は1改行、lone CR は `\x0D` とする。
tab は source 本文の先頭を列0として次の tab stop へ展開し、default 幅4、許容値 `1..=16` とする。

折り返しは terminal の表示 cell 幅で計算し、Unicode grapheme cluster を通常は分割しない。
全角文字、結合文字、ZWJ emoji を考慮する。
1 grapheme が64 KiBを超える場合だけ Unicode scalar 境界で分割する。
1論理行が16 MiBを超える場合、その行の内容は表示するが regex search 対象から外し、skip 数を status に示す。

Git が quote した path は C-style escape と octal byte を decode して header/list に表示する。
tab、newline、不正 UTF-8 は安全な可視表記にし、元の diff header line 自体は入力どおり保持する。

## 5. Diff model と parser

model は commit、file、hunk、logical line に append-only の stable ID を付ける。
streaming 中に要素を削除・並べ替えず、viewport、selection、search match は vector index ではなく ID と論理位置へ anchor する。

標準 `commit <hex object-id>` header だけを commit group として認識する。
custom pretty format は推測せず、file entry を単一 unnamed group に置く。
file change のない commit も commit navigation と overlay group header に残し、`(no file changes)` と表示する。
同じ path が複数 commit に現れた場合も occurrence ごとに別 entry とする。

rename/copy は1 entry とし、`old/path → new/path` と `R`/`C` status を表示する。
拡張子が変わる rename/copy では削除側を old path、追加側を new path の言語で highlight する。

通常の single-parent unified diff だけが old/new 行番号、syntax、intraline の対象となる。

- combined diff: `diff --cc` と `@@@` から section は認識するが、親別行番号と syntax を付けない。
- word diff: plain/porcelain は raw style、color/color-words は ANSI 除去後の差分意味を復元しない。
- binary patch: `literal`/`delta` payload を省略せず表示し、source 向け装飾を止める。
- submodule: outer path を1 entry とし、内部 `diff --git` を別 entry にしない。
- malformed section: 認識できた path は残し、次の明確な `diff --git` まで raw fallback にする。
- custom external diff/textconv: Git grammar に一致する場合だけ rich 表示する。
- generic patch と `git log --graph -p`: plain pager とし、rich parse を保証しない。

fallback でも検索、折り返し、sticky header、file navigation は構造を認識できる範囲で使える。
count を算出できない entry は `+? −?`、binary は `binary` と表示する。
parse fallback は stdin 読み込みが成功していれば fatal error とせず status 0とする。

## 6. 画面 layout

interactive mode は最上段に sticky header、最下段に status bar を常時1行ずつ置き、その間を本文領域とする。
小さな terminal で header、old/new gutter、diff marker、本文1 cell または高さ5行を確保できない場合は `terminal too small` 画面に切り替え、resize 後に同じ論理位置へ戻る。

source 行は old/new の2列行番号を持つ。
context 行は両方、削除行は old、追加行は new だけを表示する。
file entry ごとに最低3桁の gutter を取り、streaming 中に必要桁が増えたら file 全体を reflow して論理 anchor を維持する。
metadata、commit、file header、hunk header は gutter を持たず全幅を使う。

長い行は word boundary を探さず、利用可能な最終 cell で hard wrap する。
継続 row は行番号と diff marker を空欄にし、source 本文開始列へ揃える。
`j/k/e/y` は論理行ではなく visual row 単位で移動する。
通常は Unicode grapheme 境界を保ち、1 grapheme が64 KiBを超える場合だけ Unicode scalar 境界で分割する。
横 scroll と no-wrap mode は持たない。
terminal resize 時は全行を reflow する。

sticky header は file status、path、global occurrence `3/12`、`+18 −4` を表示する。
EOF 前の総数は `?` とし、複数 commit では短縮 commit ID を添える。
commit 前文では commit と subject を表示する。
header/status は折り返さず、path 中央を省略して basename、status、count を優先する。
rename は old/new 両 basename を残す。

status bar は位置、読み込み状態、pending task、検索/filter input、error を mode に応じて表示する。
BOF/EOF、無効 key、検索不一致、parse fallback では bell を鳴らさず status message だけを出す。

## 7. File list と help

`Tab` は sticky header と本文を置き換える全高 file-list overlay を開く。
最下段の status bar は残す。
commit group の下に Git 出力順の file occurrence を並べ、filter 後も相対順を変えない。
entry は `status +added -deleted path` の1 visual rowとし、長い path は左側を `…/` で省略して basename を残す。
選択 entry の path は overlay 上部 detail line で折り返す。
描画時に走査する path は先頭または末尾64 KiBまでとし、それを超える detail は末尾に `…` を付けて省略する。

一覧 navigation は `j/k/e/y`、矢印、page key を使い、`Enter` で対象へ jump して閉じる。
`/` は decode 後 path に大文字小文字を無視した subsequence filter を適用する。
filter input 中の printable key は `q/j/k/e/y` を含め query となり、候補移動は Up/Down/PageUp/PageDown を使う。
filter input 中の1回目の `Esc` で query を消して一覧へ戻り、次の `Esc` で閉じる。
query は閉じると破棄する。

一覧はGit出力順の総件数を`u64`で保持し、選択位置の周囲に最大5,000件のstable file IDだけを保持する。
選択がwindow境界を越えた場合は、同じrankを保ったままbackground workerへ次のwindowを要求する。
streaming snapshotが先に届いても、未完了requestのrankとqueryを新しいdocument generationへ引き継ぐ。
filter queryはUTF-8で2,097,157 byteを上限とする。
この値はold/new path各1 MiBと`" → "`の5 byteを合わせた最大表示path長であり、それを超えるqueryはscanを開始せずerrorにする。

`h/H` は help overlay を開く。
画面高を超えた help は本文と同じ scroll/page/g/G で移動し、diff navigation key は実行しない。
`Esc`、`q`、`h` は overlay だけを閉じる。
overlay 外の `q` だけが pager を終了する。

## 8. Navigation

本文では次の key binding を使う。

- visual row: 下 `j/e/Down/Enter/Ctrl-E/Ctrl-N`、上 `k/y/Up/Ctrl-Y/Ctrl-P`
- page: 下 `Space/f/PageDown/Ctrl-F/Ctrl-V`、上 `b/PageUp/Ctrl-B`
- half page: 下 `d/Ctrl-D`、上 `u/Ctrl-U`
- 先頭/末尾: `g/Home`、`G/End`
- hunk: 前後 `[ ]`
- file occurrence: 前後 `{ }`
- commit: 前後 `( )`
- repaint: `Ctrl-L`

1 page は本文高 minus 1 visual row として1行 overlap させる。
half page は本文高の半分を切り上げる。
`G/End` は EOF 前なら現在の末尾へ移動して follow state に入り、新しい row に追従する。
実際に末尾から離れる移動、検索、structure jump、overlay jump が成功した時だけ解除する。
resize、overlay の開閉、`W`、末尾で失敗した移動では維持する。

数値 prefix は line/page movement、`[ ]`、`{ }`、`( )`、`n/N`、`/`、`?` に回数として適用する。
`Ng/NG` は入力全体のN番目の論理行へ移動し、折り返し継続 row は数えない。
未着なら待機し `Esc` で cancel する。
leading zero は許可するが全0は invalid、overflow は実行せず error とする。
`W/Tab/h/q` は prefix を破棄して status message を出す。

EOF 前に `]/}/)` で読み込み済み末尾へ達しても pending jump にはせず、loading 中であることを status に示す。
未着 data を待つ navigation は forward search、`Ng`、bare `G` に限定する。
別の位置移動や検索が発生したら古い pending intent を破棄する。

## 9. Search と text input

`/` と `?` は Git が出力した logical text を Rust regex で検索し、生成した行番号、sticky header、status bar は対象外とする。
ANSI 除去と escape を終えた1 logical line が16 MiBを超える場合は raw data を decode せず検索対象から省き、status の skipped count へ加える。
pattern に大文字がなければ case-insensitive、1文字でもあれば case-sensitive とする。
logical line の折り返し前 text に照合し、match が複数 row へ渡っても1件とする。

入力中は有効な regex ごとに incremental preview し、`Enter` で確定、`Esc` で開始前の viewport と直前 pattern へ戻す。
不正 regex は error を表示し、直前の有効結果を維持する。
空 pattern の `Enter` は直前 regex を現在方向で再実行し、履歴がなければ `no previous search` とする。
履歴は `/`/`?` で共有し、session 中だけ Up/Down で巡回する。

`n` は直前 search の方向、`N` は逆方向へ進む。
`?` 後は `n=backward` となる。
wrap-around はせず、BOF/EOF で位置を維持する。
forward search が読み込み済み末尾に達したら incoming data の待機に入り、match 到着時に jump、EOF で not found とする。
`Esc` または新しい navigation intent で解除する。

current match を強く表示し、同一 logical line の複数 match も1件ずつ移動する。
ほかの visible match は1 frameあたり16,384 span、visible range 直前4 KiBの lookbehind を上限に弱く表示する。
Raw line が1 MiBを超える場合は main thread で text を decode せず、弱い表示を省略する。
Raw line が1 MiB以下でも、escape 後の display text が1 MiBを超える場合は preload 結果を破棄し、同じく弱い表示を省略する。
worker が返した current match は、raw byte 数にかかわらず layout の byte mapping で表示する。
弱い表示を省略した場合の status column は、grapheme 数ではなく1始まりの byte offset へ fallback する。
zero-width match は row marker と status の `line:column` で current だけを示し、反復時は少なくとも1 Unicode scalar進める。
検索起点は viewport 左上の最初に見える grapheme とし、一致 row を本文領域先頭へ置く。

search/filter input は grapheme 単位で cursor 移動と削除を行い、Left/Right、Ctrl-B/F、Home/End、Ctrl-A/E、Backspace/Delete、Ctrl-W、Ctrl-U、Enter、Esc を support する。
固定1行 input は cursor が見えるよう横方向へ pan する。

## 10. Syntax highlight

path の拡張子と既知 file name から syntax を選ぶ。
判定できない場合は、各 hunk の old 側で最初に現れる削除または context 行と、new 側で最初に現れる追加または context 行にある shebang を使う。
該当する source 行がなければ推測しない。
`two-face` が curated した TextMate grammar set と、`syntect` の pure-Rust `fancy-regex` backend を使う。
`fancy-regex` 向け set に JavaScript (Babel) grammar は含まれないため、`.jsx` は JSX expression を解釈できる TypeScriptReact grammar へ割り当てる。

削除側と追加側は別の parse state を持ち、hunk 内で継続し、hunk 境界で reset する。
両 state が diverge した context 行は new side の style、deleted file では old side を使う。
stdin-only のため hunk 冒頭が multiline comment/string の途中なら best-effort となる。

syntax 計算は decoration worker で行い、未計算部分を plain diff style で先に表示する。
現在の hunk を8 MiBの priority result として先に返し、その後 document 先頭から64 MiBの full result を計算する。
ANSI 除去と escape を終えた source text が1 MiBを超える line、または workspace へ収まらない line は syntax を省略する。
Result pool の残量がなくなった場合は、1 line だけを省略して続行せず、後続の decoration span を打ち切る。
巨大 line の viewport は必要な範囲だけ lazy decode と wrap を行う。

## 11. Intraline と whitespace

連続する削除・追加 block は、正規化後に同じ行を patience-style anchor として揃え、その間の unmatched old/new 行を順番に pair する。
move detection や離れた行の fuzzy pairing は行わない。
paired line は Unicode grapheme 単位で diff を取り、syntax token 境界へ span を広げない。

default の `ignore` mode では leading/trailing ASCII space/tab を無視し、途中の連続 space/tab を1区切りへ正規化する。
`foo bar` と `foobar` は別とし、他の Unicode whitespace は通常文字として比較する。
Git が出した `-`/`+` 行自体は隠さず、pairing と強調だけに適用する。
差分行そのものを除外したい場合は Git 側の `-w` を使う。

`W` または `--whitespace show` は whitespace を比較対象にする。
変更 span の space を `·`、tab を `→` と残り cell の `·` で可視化し、unchanged whitespace と context 行は通常表示にする。

計算は background worker で行い、完了前は通常の `-`/`+` style、完了後に viewport を再描画する。
次のいずれかを超える block は intraline を block 単位で省略する。

- 削除行と追加行の合計5,000行
- いずれかの logical line が20,000 grapheme
- old/new UTF-8 payload 合計64 MiB
- grapheme 合計1,000万

combined、word、binary、submodule、malformed fallback では intraline を無効にする。
上の semantic cap 内でも、96 MiBの workspace へ収まらない block は省略する。
8/64 MiBの result pool へ transfer できない場合は、その block 以降の decoration span を打ち切る。
Line または block 単位で省略した場合、status warning は size cap と workspace 不足を区別しない。
syntax は `syntax omitted for some lines`、intraline は省略した block 数を表示する。
Result pool の残量がなく後続 span を打ち切った場合は、件数ではなく `decoration budget exceeded; later spans were omitted` と表示する。

## 12. Theme と表示 style

built-in theme は `dark` と `light`、default は `dark` とする。
`COLORTERM` と `TERM` から capability を判定し、24-bit、256色、16色へ順に量子化する。
判定不能なら16色を使う。
`NO_COLOR` または `--no-color` は syntax と diff background を無効にするが、UI 自体は維持する。

色だけに依存せず、`+/-` marker、old/new 行番号、太字、下線、反転 selection を併用する。
Git の `--color-moved`、`--color-moved-ws`、`--ws-error-highlight`、`color.diff.*` の意味は引き継がない。

## 13. Buffer と memory budget

raw input は最初の64 MiBまで memory に置き、超過時は permission `0600` で作成直後に unlink した temporary file へ spill する。
終了時は descriptor を閉じる。
tempfile 作成または書き込みに失敗した場合は unbounded memory へ切り替えず、terminal を復元して OS error を stderr へ出し status 1とする。

structural index は別枠256 MiBを上限とし、超過 record を temporary file へ spill して sparse index だけを RAM に残す。
Raw data と structural identity は eviction しない。

Viewport layout と decoration の owned data には、別枠256 MiBの固定 partition を割り当てる。
pool 間で未使用分は融通しない。

- viewport layout 24 MiB：eager layout LRU 12 MiB、巨大行 navigator と current/prepared streaming frame 8 MiB、layout transaction と row/index bookkeeping 用4 MiB
- renderer へ適用中の decoration result 64 MiB
- background で作る次の full decoration result 64 MiB
- current hunk を先に返す priority decoration result 8 MiB
- decode、syntax、intraline が計算中だけ使う workspace 96 MiB

Applied result は旧 payload を解放してから、同じ ledger transition 内で置き換える。
Next/Priority result は bounded latest-wins channel が所有し、stale、cancel、panic では lease を返す。
Viewport の eager layout だけを LRU eviction し、decoration result は whole-result 単位で破棄して再計算する。
Bundled syntax grammar と theme の immutable program asset は入力由来ではないため、この256 MiBには含めない。

この256 MiBは process 全体の resident-memory 上限ではない。
Viewport、Search worker、file-filter worker、`FrameLines` の line reader buffer は raw/display 上限と chunk decode によって別に制限する。
Search worker が保持する searchable text は1 logical lineあたり最大16 MiBとする。
File-filter の AppState と latest request が保持する query はそれぞれUTF-8で最大2,097,157 byteとし、worker の lowercase scalar vector はこの入力を2回走査して算出した要素数だけを確保する。
File-filter の path reader は現在走査中の old/new path blob だけを保持し、各blobは parser の1 MiB上限に従う。
描画前の `FrameLines` は metadata と range を4 MiBまで保持し、raw line と escape 後の display text がともに1 MiB以下の場合だけ preload 中に weak match 用 text を一時 decode する。
これらの bounded buffer、thread stack、dependency の program asset は256 MiB ledger には計上しない。

1 GiBまたは500万 logical line の先に達する方を performance acceptance workload とし、それ以上は best effort とする。
cached viewport の操作は16 ms付近、spill 後の未cache領域を含む通常操作は key input から再描画まで50 ms以内を目標にする。
全文 regex、初回 syntax、intraline は background task とし、この latency budget から除外する。

`scripts/profile-large-fixture.sh`は既定で500万行、payload 200 byteのfixtureを生成し、release buildの `examples/large_profile.rs`を101 sampleで実行する。
fixture、report、`CARGO_TARGET_DIR`は同じ `/tmp/git-ultrapager-profile.*`に置き、repository、global Git config、installed binaryを変更しない。
reportにはparse時間、raw/structural spill、cached/cold viewport collectionのp50/p95/max、cache/read counterを出力する。
Cold sampleは各sampleの前にlayout cacheを空にした測定であり、fixtureがspillしたことを意味しない。
16 msと50 msはhardwareごとの比較に使うviewport collectionのlabelであり、harnessの終了statusを決めるassertionにはしない。
harnessの計測範囲は`ViewportLayout::collect`だけであり、上記のkey inputから再描画までの目標は検証しない。

## 14. Architecture

single Cargo package とし、薄い `src/main.rs` と testable な `src/lib.rs` に分ける。
public library API の外部互換性は約束しない。
project code は `#![forbid(unsafe_code)]` とし、production code の `unwrap()`/`expect()` を lint で禁止する。
dependency 内部の unsafe は対象外とする。

interactive core は `Event → update(AppState) → Effect` の一方向 state machine とする。
Renderer は AppState、theme、適用中 decoration、draw 前に preload した bounded `FrameLines` だけを読み、I/O や worker を直接呼ばない。

- main thread：controlling TTY の event、state update、Ratatui 描画
- reader thread：stdin read、sanitization、spool、incremental parse
- search worker：latest request を scan し、capacity 1の request/response channel で stale result を捨てる
- file-filter worker：latest query と selection rank を scan し、capacity 1の request/response channel で stale result を捨てる
- decoration worker：syntax と intraline を直列計算し、capacity 1の request と priority/full 用 capacity 2の response channel を使う
- channel：parse event、search result、file-filter result、decoration result
- generation ID：古い非同期 result の破棄

terminal UI は `ratatui` と `crossterm`、CLI は `clap` derive を使う。
Tokio runtime は導入しない。
optional Cargo feature は作らず、同じ binary に全機能を含める。
`Cargo.lock` は repository に含める。

## 15. CLI と終了 status

CLI surface は次に限定し、未知 option と positional argument は usage error 2とする。
less option の compatibility は持たない。

```text
-h, --help
-V, --version
--theme dark|light
--no-color
--paging auto|always|never
--tab-width 1..16
--whitespace ignore|show
```

stdin が TTY の direct invocation は入力を待たず、Git 設定例と `cat patch.diff | git-ultrapager` を示して status 2で終了する。
Git pager invocation と空 pipe を区別できない環境では、empty-input の silent exit を優先する。

## 16. Test と completion criteria

v0.1.0 は次を満たした時点で完成とする。

- standard、rename/copy、binary、submodule、combined/word fallback、malformed の parser fixture
- dark/light/no-color、複数幅、Unicode、tab、wrap の renderer snapshot
- 全 key binding、search、overlay、streaming update の state-machine test
- actual Git pipe、resize、quit、signal、suspend 後の terminal 復元を扱う PTY integration test
- ANSI sanitizer と diff parser が任意 byte で panic しない `proptest`
- CI 向け縮小 dataset と、手動の1 GiB/500万行 benchmark/profile

CI向け縮小datasetは25,000 changed lineを生成し、小さいresident limitでraw storeとstructural storeの両方をspillさせる。
先頭・中央・末尾のdecode、先頭・末尾viewport、memory cap、read/layout counterが入力行数に比例しないことを`tests/large_input.rs`で検証する。
手動profileは `scripts/profile-large-fixture.sh`と`examples/large_profile.rs`を正とする。

snapshot は raw ANSI ではなく Ratatui cell grid を `character + semantic style token` へ正規化する。
dark/light の具体的 palette は別 unit test にする。
coverage-guided `cargo-fuzz` harness は v0.1.0 の scope 外とし、stable の `proptest` を使う。

GitHub Actions は Linux/macOS で次を実行する。

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
```

benchmark の最大 fixture は通常 CI に置かない。
README pair の片方だけを変更した change set は CI で失敗させる。
documentation は `README.md` を英語 default、`README-ja.md` を同じ構造と内容の日本語 counterpart とし、詳細 design はこの日本語 file だけに置く。

## 17. Distribution と repository policy

v0.1.0 は `cargo install --path .` による source install のみを対象とする。
crates.io、Homebrew、prebuilt binary、release signing、cross-build automation は後続 milestone とする。
global Git config と installed binary は開発・test 中に変更せず、`target/debug/git-ultrapager` と `/tmp` の fixture repository だけを使う。

license は MIT、copyright holder は `Copyright (c) 2026 git-ultrapager contributors` とする。
implementation は `main` branch 上で coherent milestone ごとに commit し、branch を追加しない。
