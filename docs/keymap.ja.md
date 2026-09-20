# キーマップ

*Language: [English](./keymap.md) / 日本語*

renga のフルキーバインド一覧です。README には「最初に覚える 5〜8 個のチートシート」だけを残し、それ以外はすべてこのページにあります。

> **macOS ユーザーへ:** macOS のターミナルは既定で `Option+<キー>` を Unicode 入力 (`å` `∫` など) に割り当てているため、そのままだと `Alt+<キー>` のショートカット — つまりグローバルキーマップのほぼ全部 — が renga まで届きません。1 行の設定で解決できます → [macOS: Option をメタキーにする](#macos-option-をメタキーにする) (WezTerm / iTerm2 / Alacritty / Ghostty / Kitty / Terminal.app 別に記載)。

## ペインモード (通常状態)

renga のグローバルキーはすべて `Alt` 名前空間にあります。素の `Ctrl+<キー>` (`Ctrl+D` `Ctrl+W` `Ctrl+Q` など) は意図的に **バインドしていません** — ペイン内で動くプログラム (シェルの readline、vim、fzf など) にそのまま届きます。例外は `Ctrl+C` (テキスト選択中のみ renga 側でコピー)、`Ctrl+;` (IME overlay)、コピーモードのエイリアス `Ctrl+Shift+M` だけです。

| キー | 動作 |
|-----|--------|
| `Alt+D` | 縦分割 |
| `Alt+E` | 横分割 |
| `Alt+W` | ペインを閉じる (最後の 1 つならタブごと閉じる) |
| `Alt+T` | 新しいタブ |
| `Alt+1..9` | 指定番号のタブに移動 |
| `Alt+Left/Right` | 前 / 次のタブ |
| `Alt+R` | タブ名を変更 (セッション内のみ) |
| `Alt+S` | ステータスバー表示切替 |
| `Alt+Shift+C` | フォーカス中のペインの常時キャプチャリングを、リプレイ可能な `.bin` / `.jsonl` としてプラットフォームのデータディレクトリ (`%LOCALAPPDATA%\renga\pane-captures` on Windows) に保存。完了または失敗はステータスバーに短時間表示される。 |
| `Alt+P` | フォーカス中のペインにメッセージング対応の Claude Code 起動コマンドを入力 ([`peer-messaging.ja.md`](./peer-messaging.ja.md) 参照)。フォーカス中のペインが alt-screen モード (vim / less / lazygit / 起動中の Claude / Codex の TUI 等) かペインタイトルに "claude" を含む場合は仕様として silently no-op になります — 起動中の TUI にコマンドのバイト列がキーストロークとして注入されるのを防ぐためです。シェルプロンプトのペインにフォーカスを移してから再度押してください。 |
| `Alt+F` | ファイルツリー表示切替 |
| `Alt+O` | プレビューとターミナルの位置を入れ替え |
| `Alt+Up/Down` | サイドバー / プレビュー / ペイン間のフォーカス移動 |
| `Alt+PageUp/PageDown` | フォーカス中のペインを半ページ分スクロールバック (コピーモード不要) |
| `Alt+Home` / `Alt+End` | スクロールバック最上部へ / 最下部 (ライブ表示) へ戻る |
| `Ctrl+;` / `Alt+;` / `Alt+I` | IME 合成 overlay を開く (中央に複数行入力ボックス — [`ime.ja.md`](./ime.ja.md) 参照)。`Alt+;` / `Alt+I` は `Ctrl+;` を奪うターミナル (WSL + Windows Terminal、Linux 上の VS Code ターミナル、一部の tmux 設定など) のフォールバック。 |
| `Alt+M` / `Ctrl+Shift+M` | フォーカス中のペインでキーボードコピーモードに入る ([コピーモード](#コピーモード-altm-押下後) 参照) |
| `Alt+Q` | renga 終了 |

Git Bash は従来の `ESC C` シーケンスを `do-lowercase-version` 経由で
`capitalize-word` に割り当てています。一部のターミナルでは
`Alt+Shift+C` も同じシーケンスになるため、renga はこの使用頻度の低い
エイリアスをキャプチャ用に消費します。Caps Lock と `Alt+c` の組み合わせも
同じ符号化になる場合があり、その場合はダンプが発火します。自動化では、
明示的な `target` または `all: true` を指定する `dump_pane_capture` MCP
ツールを使用できます。

## コピーモード (`Alt+M` 押下後)

Windows Terminal の mark mode を手本にした、キーボードだけでのテキスト選択・コピー。ペイン下部ボーダーに `COPY` ヒントが表示され、ペインのカーソル位置にブロックカーソルが現れます。モード中はキー入力もペーストも PTY には一切届きません。

> `Ctrl+Shift+M` は Windows Terminal 本家の mark mode と同じキーですが、ほとんどのホストターミナル (Windows Terminal 自身を含む) が renga に届く前に横取りします — 確実に効くのは `Alt+M` です。

| キー | 動作 |
|-----|--------|
| `←` `↑` `↓` `→` | カーソル移動。上端で `↑` を押すとスクロールバックへ |
| `Home` / `End` | 行頭 / 行末へ移動 |
| `PageUp` / `PageDown` | 1 ページ分ビューをスクロール |
| `Shift` + 上記の移動キー | 選択範囲を拡張 (最初の Shift 移動の直前のカーソル位置がアンカー) |
| Shift なしの移動 | 選択を解除 |
| `Enter` / `Ctrl+C` | 選択をクリップボードへコピーしてモード終了 |
| `Esc` | コピーせずモード終了 |
| マウスクリック / ホイール | モード解除 (マウス選択に切り替わる) |

## macOS: Option をメタキーにする

macOS のターミナルは既定で `Option+<キー>` を Unicode 入力 (`å` `∫` `π` など) に割り当てているため、renga の `Alt` 系ショートカット — 分割・タブ・終了を含むグローバルキーマップのほぼ全部 — が発火しません。Option をメタキー扱いに切り替えれば解決します。どの端末も 1 行の設定変更で済みます。**素の Terminal.app はあまりおすすめしません** — IME 対応・ligature・画像プレビューのどれも下表のモダン端末の方が良いため、乗り換えが結局近道です。

| ターミナル | 設定 |
|---|---|
| **WezTerm** (`~/.wezterm.lua`) | `config.send_composed_key_when_left_alt_is_pressed = false` <br> `config.send_composed_key_when_right_alt_is_pressed = false` |
| **iTerm2** | Settings → Profiles → Keys → **Left Option key** と **Right Option key** を **Esc+** に設定 |
| **Alacritty** (`~/.config/alacritty/alacritty.toml`) | `[window]` <br> `option_as_alt = "Both"` (`"OnlyLeft"` / `"OnlyRight"` も可) |
| **Ghostty** (`~/.config/ghostty/config`) | `macos-option-as-alt = true` |
| **Kitty** (`~/.config/kitty/kitty.conf`) | `macos_option_as_alt yes` |
| **Terminal.app** | Settings → Profiles → Keyboard → **Use Option as Meta key** にチェック |

**既知の制約**

- 一部の macOS IME (ことえりの「英字」切替、日本語キー配列など) は Option を独自に使っているため、Meta 化と競合する場合があります。IME が壊れたら `OnlyLeft` / `OnlyRight` で片側だけ Meta にするのが妥協点です。
- `Alt+1..9` は macOS の Mission Control / Spaces のショートカットと衝突することがあります。OS 側に奪われる場合はタブ巡回を `Alt+Left/Right` で代替してください。

## ファイルツリーモード (`Alt+F` 押下後)

| キー | 動作 |
|-----|--------|
| `j` / `k` | 上下移動 |
| `Enter` | ファイルを開く / ディレクトリ展開 (インライン) |
| `h` | ツリーの root を 1 つ上のディレクトリに変更 |
| `l` | 選択したディレクトリに root を移動 (ファイルに対しては no-op) |
| `c` | 現ペインを左右分割し、右側の新ペインを選択位置のディレクトリ (ファイル選択時は親、空のときはツリー root) で開いた上で、Claude + peer MCP の起動コマンドをプロンプトに流し込む。`Alt+P` と同様に Enter 未押下なので、内容を確認してから Enter で起動できる。 |
| `v` | `c` と同じ挙動で上下分割 (下側に新ペイン) |
| `.` | 隠しファイル表示切替 |
| `Esc` | ペインに戻る |

## プレビューモード (プレビューにフォーカス中)

| キー | 動作 |
|-----|--------|
| `j` / `k` | 縦スクロール |
| `h` / `l` | 横スクロール |
| `Alt+W` | プレビューを閉じる |
| `Esc` | ペインに戻る |

## マウス

| 操作 | 動作 |
|--------|--------|
| ペインをクリック | そのペインにフォーカス |
| タブをクリック | タブ切替 |
| タブをダブルクリック | タブ名変更 |
| `+` をクリック | 新しいタブ |
| ペイン外周をダブルクリック | クリックした側へ分割。上 / 左はクリック側に新ペインを配置、下 / 右は奥側に配置。角のセルは無視。分割後のペインが `min_pane_width` / `min_pane_height` より小さくなる場合、またはワークスペースが既にペイン上限に達している場合は拒否。 |
| 共有境界をダブルクリック | 分割線の手前側のペインを分割し、新ペインを境界上 (2 つの兄弟ペインの間) に配置。縦の分割線は左ペインを右へ、横の分割線は上ペインを下へ分割する。2 本の分割線が交差する接合セルは無視。`min_pane_width` / `min_pane_height` / ペイン上限の拒否条件は上記と同じ。 |
| 境界をドラッグ | パネルのリサイズ |
| ホイールスクロール | ファイルツリー / プレビュー / ターミナル履歴。マウスレポートに opt-in している TUI (Claude Code `/tui fullscreen`, vim, lazygit, less 等) では代わりにアプリ側にホイールイベントが届く。 |
| ペイン内クリック / ドラッグ | 通常はテキスト選択 (コピー用)。マウスレポートに opt-in している TUI 上ではアプリにクリックが転送され、アプリ側のボタンやカーソル移動が動く。`Shift` を押しながらドラッグすると強制的に renga のテキスト選択 (tmux / alacritty と同じ escape hatch)。 |

ホイール / クリックいずれの転送も `RENGA_DISABLE_MOUSE_FORWARD=1` で無効化できます。renga をネストしている場合や、マウスプロトコル encoding が合わなくて内側アプリが混乱する環境向けの逃げ道です。

## IME overlay 内のキーマップ

IME 合成ボックスの **内側** で押すキー (送信、移動、閉じてもドラフト保持など) は [`ime.ja.md`](./ime.ja.md#overlay-内のキーマップ) を参照してください。本ページのペインモード表には、overlay を **開く** ための chord (`Ctrl+;` / `Alt+;` / `Alt+I`) だけを記載しています。
