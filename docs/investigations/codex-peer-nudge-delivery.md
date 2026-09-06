# 調査レポート: Codex ペインへの peer message 配送（nudge / 保留ダイアログ）

- 種別: **実機ブラックボックス測定 + ソース照合**（コード変更なし。成果物は本ドキュメント1本）
- 測定日: 2026-08-15
- 対象リビジョン: `26d12ed`（main / v1.3.2）
- 対象実装: `src/app/codex_peer.rs`, `src/app/keyboard_input.rs`
- 測定環境: Windows 11 (10.0.26200), renga 1.3.2, Codex CLI v0.147.0 (gpt-5.6-sol medium)
- 測定手段: MCP `renga-peers` の `send_message` / `inspect_pane` / `send_keys` / `list_panes` / `focus_pane`
- 送信側: Claude Code ペイン (id=1)、受信側: Codex ペイン (`t2`, 履歴クリーンな新規ペイン)

---

> ## ⚠ 訂正 (2026-08-16)
>
> **上記「対象リビジョン: `26d12ed`」は、実機測定については誤りです。**
>
> 測定に使ったバイナリ (`target/release/renga.exe`) は、当日の merge を1件も含まない Aug 14 のビルドでした。
> 文字列検索で確定:
>
> | マーカー | 導入元 | バイナリ内 |
> |---|---|---|
> | `esctointerrupt` / `tabtoqueuemessage` | renga-erg (`fb3549c`, `39b49c9`) | **不在** |
> | `Startup command queued` | renga-aau (`0ce438c`, `b29161e`) | **不在** |
> | `preserving all caller-provided trailing arguments` | renga-jaf | **不在** |
>
> 原因は `cargo build --release 2>&1 \| tail -N; echo $?` という書き方で、`$?` が `tail` の終了コードを拾い、
> cargo の `error: failed to remove file ... (os error 5)` を握り潰していたこと。
> 実行中の renga が exe をロックしているため、ビルドは2回とも失敗していた。
>
> **影響範囲:**
>
> - **§2〜§5 のソース照合と問題の特定は有効。** `26d12ed` のソースに対して行っており、
>   問題① は独立に `src/app/codex_peer.rs:748 / 853 / 978` で再確認済み。
>   起票済みの renga-19x / renga-zqx / renga-8r9 / renga-z7k はいずれも有効。
> - **§6「期待どおりに動作していた項目」(A/B/C/D) は無効。** renga-erg より前のバイナリでの測定であり、
>   renga-erg の実機確認にはなっていない。**再測定が必要。**
> - §1 の状態別挙動表も、測定部分は同じ理由で再確認を要する。
>
> **教訓:** 「起動中プロセスの `ExecutablePath` が正しいか」という確認は、
> 「別のファイルを起動した」を検出するために設計されたもので、
> **「正しいファイルだが中身が古い」を検出できない。**
> Windows では実行中の exe を上書きできずビルドが失敗し、古いファイルが確認箇所にそのまま残るため、
> **後者のほうが起こりやすい失敗**である。
> バイナリを渡す前に、**期待するマーカー文字列が含まれていること**を確認すること。

---

## 0. TL;DR

3点。**①が実装バグ、②③は仕様上の穴。**

1. **`accept_codex_peer_notification()` が nudge を composer に書くだけで Enter を送らない。** ユーザーが保留ダイアログを Ctrl+Enter で accept しても配送は完了せず、さらに手動で Enter を押すまでメッセージは届かない。他の2つの nudge 書き込み経路はいずれも `SubmitAt` をキューしているのに、accept 経路だけ欠けている（`src/app/codex_peer.rs:833-859`）。
2. **`send_message` の戻り値がキュー投入と受領を区別しない。** フォーカス中ペイン宛は手動2操作を経ないと届かないのに、送信側には即座に `Delivered to <name>.` が返る。オーケストレーション（将軍→家来）で「指示した」と誤認する。
3. **ダイアログはフォーカス中のペインにしか描画されない。** 非フォーカス + composer に下書きあり、の組み合わせでは保留が完全に無表示になる。

なお **A（busy 中の配送）、B（turn 中断）、C（下書き保護）、D（承認プロンプト中の干渉）はすべて期待どおり**で、実装は堅い。問題は「届いたことをどう保証し、どう見せるか」に集中している。

---

## 1. 確定した配送メカニズム

renga の nudge は **composer に文字列を打ち込み、少し遅れて Enter を送る**方式。実際に注入される本文（`codex_peer.rs:433,444`）:

```
Peer request from id=1 kind=claude. Run check_messages now. Treat each returned message as a direct coworker
request: do the requested work, and use send_message only when a reply or status update is needed.
```

A-6 のラウンドで**注入直後・Enter 前の中間状態を実測で捕捉**した（`inspect_pane` の連続2回で、1回目が composer に本文・`Working` なし、2回目が `Working (2s)`）。`CODEX_PEER_NUDGE_COMMIT_DELAY = 1000ms`（`codex_peer.rs:4`）と整合する。

### 状態別の挙動（実測）

| ペイン状態 | ダイアログ | 配送 |
|---|---|---|
| 非フォーカス + composer 空 + idle | 出ない | **自動**。turn 終了から 2〜13.5 秒 |
| 非フォーカス + 下書きあり | **出ない** | **保留。画面上の表示ゼロ** |
| フォーカス + composer 空 + idle | 出ない | **即座に自動配送**（`route_focused_codex_peer_message` の ready 経路） |
| フォーカス + busy 中に着信 → turn 終了 | **出る** | **自動配送されない。Ctrl+Enter → Enter の手動2操作が必要** |
| フォーカス + 下書きあり | 出る | 保留。下書きは保護される |
| busy 中の着信（フォーカス問わず） | — | 保留。composer に触らず、turn も中断しない |
| 承認プロンプト表示中の着信 | — | プロンプトに一切干渉しない |

ダイアログはフォーカスに追随する。target にフォーカスを移すと現れ、他ペインへ移すと消える（保留自体は継続）。ソース側も一致:
`codex_peer_notification_is_visible()`（`codex_peer.rs:762`）がフォーカスを要求し、
`materialize_unfocused_codex_peer_notification()`（`codex_peer.rs:816`）が非フォーカス時にキューへ戻す。

### 保留の解除条件（実測）

- composer が空になった瞬間（Backspace のみでも Ctrl+U でも再現）
- 非フォーカスなら `ready_for_nudge` が立った時点で自動（turn 終了直後）
- フォーカス中は上記いずれの経路もダイアログへ吸い込まれる（`codex_peer.rs:933,991,1027,1081` の4箇所がすべて `focused_notifications` へ流す）

---

## 2. 問題① — accept が Enter を送らない（実装バグ）

### 症状

保留ダイアログを **Ctrl+Enter で accept すると、ダイアログが閉じて nudge 文が composer に下書きとして入るだけで、送信されない。** `Working` は出ず、新しい turn も始まらない。ユーザーがさらに **Enter を押して初めて**配送が完了する。

実測（target フォーカス、busy 中に「テストE2」着信 → turn 終了から25秒経過）:

```
───────────────────────────────────────────────────────────────────

• 1から100まで、重複のない一言コメントを添えたテキストを coworker に送信しました。

───────────────────────────────────────────────────────────────────


› Summarize recent commits          ← composer 空。未配送
```

Ctrl+Enter 直後:

```
───────────────────────────────────────────────────────────────────


› Peer request from id=1 kind=claude. Run check_messages now. Treat each returned message as a direct coworker
  request: do the requested work, and use send_message only when a reply or status update is needed.

  gpt-5.6-sol medium · ~          ← Working なし。まだ送信されていない
```

Enter 追加後に `Working (1s)` となり配送完了。

### 原因

nudge 本文を PTY に書く箇所は3つあるが、**accept 経路だけ commit（Enter）をキューしていない。**

| 経路 | 書き込み | commit |
|---|---|---|
| `route_focused_codex_peer_message` (`codex_peer.rs:751`) | `write_input_to_pane` | `queue.push_back(SubmitAt(now + COMMIT_DELAY))` (`:754`) |
| flush の `ready_for_nudge` 分岐 (`codex_peer.rs:978`) | `write_input_to_pane` | `queue.push_front(SubmitAt(now + COMMIT_DELAY))` (`:980`) |
| **`accept_codex_peer_notification` (`codex_peer.rs:853`)** | `write_input_to_pane` | **なし** |

`accept_codex_peer_notification` は書き込み直後に

```rust
self.pending_codex_peer_messages.remove(&notification.target_pane);   // :854
self.codex_peer_notification = None;                                   // :856
```

でキューと通知を両方クリアしてしまうため、後続の commit をトリガーする状態が残らない。

### 修正方針

他2経路と同じく、書き込み後に `SubmitAt` をキューする。`remove` してから `entry().or_default()` で入れ直す形になる:

```rust
write_input_to_pane(pane, payload.as_bytes(), false)?;
let queue = self
    .pending_codex_peer_messages
    .entry(notification.target_pane)
    .or_default();
queue.clear();
queue.push_back(PendingCodexPeerDelivery::SubmitAt(
    Instant::now() + CODEX_PEER_NUDGE_COMMIT_DELAY,
));
self.codex_peer_notification = None;
```

**確認事項:** `SubmitAt` の処理（`codex_peer.rs:1006-1016`）はフォーカス状態を見ないので、accept 後にユーザーが他ペインへ移っても commit は走る。ただし commit までの 1 秒間にユーザーが composer に打つと**その文字が nudge 文の末尾に連結されたまま送信される**（問題③と同根）。`QueueAt` が持っている `expected_composer` 照合（`codex_peer.rs:1024-1026`）と同等のガードを `SubmitAt` にも入れるか、accept 経路だけ `QueueAt` を使うのが安全と思われる。

### 回帰テスト案

`src/app/tests/codex_peer.rs` に、accept 後にキューへ `SubmitAt` が積まれることと、`CODEX_PEER_NUDGE_COMMIT_DELAY` 経過後の `flush_pending_codex_peer_messages` で `\r` が PTY へ書かれることを検証するケースを追加する。

### renga-19x の実装方針 (2026-08-20)

既存の下書きがある場合は accept を拒否し、通知を表示したままにする。下書きを編集するキーでは従来どおり通知を配送キューへ戻し、ユーザーが下書きを送信・退避・消去して composer が空になると、保留中の nudge を安全に配送する。composer が空の状態で accept した場合は、nudge を書き込み、遅延後に自動送信する。下書きと nudge を連結して送信することはない。

また、すべての `SubmitAt` に書き込み直後の正規化済み composer を `expected_composer` として保持する。遅延中に composer が変化した場合は Enter を送らず、キューを維持する。これにより、accept 後を含むすべての自動送信経路で、ユーザーの追加入力を nudge の一部として黙って送信しない。

---

## 3. 問題② — `send_message` の戻り値が受領を保証しない

`send_message` は即座に `Delivered to <name>.` を返すが、フォーカス中ペイン宛の場合、実際の配送はユーザーが Ctrl+Enter → Enter を押すまで発生しない。問題①を直しても Ctrl+Enter は依然必要なので、この乖離は残る。

オーケストレーション用途（`/issue-shogun` のように将軍ペインが家来ペインへ指示を出す）では、将軍が「指示した」と判断して次工程へ進む一方、家来は画面を見ていなければ着手しない。**サイレントな詰まり方をするのが厄介**で、将軍側からは家来がただ遅いのか、そもそも受け取っていないのかが区別できない。

### 方針候補

- **A. 戻り値で状態を区別する。** `Delivered to X.` / `Queued for X (awaiting user confirmation).` のように、即時注入できたか保留になったかを送信側へ返す。将軍側が待つ・催促する・別ペインへ振り直すといった判断を取れるようになる。
- **B. 送信側から保留状態を照会できるようにする。** `list_peers` / `list_panes` の各ペインに保留件数を載せる。A と併用すると、将軍が「まだ受け取られていない」ことをポーリングで確認できる。
- **C. フォーカス中でも自動配送する。** 手数は減るが、ユーザーが操作中のペインに勝手に Enter を送ることになるため、現状のダイアログ設計（ユーザーの操作を奪わない）を捨てることになる。**非推奨。**

A が最小で効果が大きい。B は ② と ③ の両方に効く。

---

## 4. 問題③ — 保留がフォーカス中しか可視化されない

ダイアログは `codex_peer_notification_is_visible()`（`codex_peer.rs:762`）がフォーカスを要求するため、**そのペインにフォーカスを移さない限り保留に気付けない。**

特に **非フォーカス + composer に下書きあり**の組み合わせは、自動配送もされずダイアログも出ないため完全に無表示になる。実測では 60 秒間、画面上に何の痕跡もないまま保留された（下書き自体は無傷で保護されており、その挙動は正しい）。

### 方針候補

- 非フォーカスペインのタイトル / ステータス行に保留件数バッジを出す（例: `t2 ●2`）
- タブバーに、そのタブ内で保留を抱えるペインがある旨のインジケータを出す
- `list_panes` / `list_peers` の出力に保留件数を含める（問題②の B と同じ）

---

## 5. 副次的な指摘

### 5.1 ダイアログ表示中、Ctrl+Enter 以外のキーでダイアログが消える

`src/app/keyboard_input.rs:40-54`:

```rust
if self.codex_peer_notification_is_visible() {
    if matches!(key.code, KeyCode::Esc) || (CONTROL + 'c') {
        self.dismiss_codex_peer_notification();
        return Ok(true);
    }
    if crate::input::overlay::is_overlay_commit_key(key) {
        return self.accept_codex_peer_notification()...;
    }
    self.requeue_codex_peer_notification();   // ← ここ
}
```

Esc / Ctrl+C / commit キー以外の**任意のキー**（矢印キーを含む）が `requeue_codex_peer_notification()` を通り、ダイアログが閉じてキーはそのまま PTY へ落ちる。ユーザー報告でも「左矢印を押したらダイアログが消えて nudge が入った」現象が発生している。

requeue 自体はメッセージを失わない（`restore_codex_peer_notification` でキューへ戻る）ので**データロスではない**が、確認 UI としては素通しに近い。矢印キーやカーソル移動系は requeue せず握り潰す、あるいは requeue 後もバッジを残す、といった扱いが妥当と思われる。

### 5.2 accept 後の composer に打つと nudge 文に連結される

accept で composer に nudge 文が入った状態のまま文字を打つと、末尾に連結される。実測:

```
› Peer request from id=1 kind=claude. Run check_messages now. Treat each returned message as a direct coworker
  request: do the requested work, and use send_message only when a reply or status update is needed.ABC
```

ユーザーは nudge 文が見えている状態なので気付ける余地はあるが、問題①を直して自動 commit するようになると、**commit までの1秒間に打った文字が混入したまま送信される**。§2 の確認事項のとおり `expected_composer` 照合を入れるのが安全。

---

## 6. 期待どおりに動作していた項目

以下は問題なし。実装が堅いことの確認として記録する。

| 項目 | 内容 | 結果 |
|---|---|---|
| **A** | busy 中に着信させ、1秒後・3秒後の composer を確認 ×8 ラウンド | **8/8 混入なし。**保留中は composer に一切書き込まない |
| **B** | 上記各ラウンドで turn が中断されていないか | **8/8 中断なし。**1〜60 / 1〜100 の出力を毎回完走 |
| **C-1** | 下書きあり + 非フォーカスで着信、3/10/20/60秒 | **全時点で下書き無傷。**注入なし・自動送信なし |
| **C-2** | 下書きあり + フォーカスで着信、3/30秒 | **下書き無傷。**ダイアログで通知しつつ下書きを保護 |
| **D** | 承認プロンプト表示中に着信 | **プロンプト無傷。**選択は 1 のまま、文字混入なし。`Test-Path` で未承認コマンドが実行されていないことを実測 |

D の実測画面:

```
  Would you like to run the following command?
  Environment: local
  Reason: C:\Windows\Temp に指定されたテストファイルを書き込んでもよいですか？
  $ Set-Content -LiteralPath 'C:\Windows\Temp\renga-approval-test2.txt' -Value 'hello' -NoNewline -Encoding ascii

› 1. Yes, proceed (y)
  2. Yes, and don't ask again for commands that start with ... (p)
  3. No, and tell Codex what to do differently (esc)

  Press enter to confirm or esc to cancel
```

着信5秒後も同一。`Test-Path 'C:\Windows\Temp\renga-approval-test2.txt'` → `False`。

---

## 7. 再現手順

### 問題①（accept が送信しない）

1. Codex ペインを1つ立てる
2. 送信側ペインにフォーカスした状態で、Codex に長めの作業を `send_message` で依頼する
3. Codex が `Working (...s • esc to interrupt)` になったのを確認する
4. **Codex ペインにフォーカスを移す**
5. その状態でもう1通 `send_message` する
6. Codex の turn が終わるのを待つ → 保留ダイアログが出る
7. **Ctrl+Enter を押す** → ダイアログが閉じ、composer に nudge 文が入る
8. **ここで `Working` にならない**（＝未配送）。Enter を追加で押すと初めて配送される

### 問題③（非フォーカス + 下書きで無表示）

1. Codex ペインを idle にする
2. 送信側にフォーカスしたまま、Codex の composer に下書きを入れる（`send_keys` で text のみ、Enter なし）
3. `send_message` する
4. 60秒待っても配送されず、画面上に保留を示す表示は一切ない
5. Codex ペインにフォーカスを移すとダイアログが現れる（フォーカスを戻すと消える）
6. composer を空にすると即座に配送される

---

## 8. ack 後の follow-up nudge（renga-fmx）

nudge が pending の間に複数の peer message が届くと、後続分は既存の nudge
に合流する。このため先頭本文の ack 後、pull inbox に次の head が残る場合は、
mcp-peer が additive IPC request `peer_inbox_head_acknowledged` を App へ 1 回だけ
送り、通常の pending nudge queue に新しい Draft を積む。Request には次の head
の送信者 id/name/kind を載せ、既存の nudge 文面を保つ。残り 0 件では送らない。

この経路は best-effort である。新しい mcp-peer が長時間稼働中の古い TUI に
接続すると未知 Request は `parse` error になるが、ack 自体は既に local inbox
で成立しており、その成功応答を変えない。呼び手は `pending_after > 0` なら nudge
を待たず、従来どおり直ちに `check_messages({})` を再実行する。

debug JSONL では mcp-peer の `check_messages` record の `renudge_after_ack` が
`sent` / `rejected` / `skipped_none_pending` 等を示す。App 側は
`renudge_after_ack_enqueued`、`renudge_after_ack_skipped_queue_occupied`、
`renudge_after_ack_skipped_none_pending` を記録する。

---

## 9. 測定上の注意（後続調査者向け）

本調査は途中で3回、誤った結論に到達した。いずれも測定手順の欠陥が原因なので記録しておく。

1. **フォーカス状態は測定点ごとに `list_panes` で実測すること。** `focus_pane` を呼んだ時点の状態が続くとは限らない。特に**ユーザーがチャットで返信するたびフォーカスは送信側ペインへ移る**ため、対話しながら測ると条件が勝手に変わる。「フォーカス中は自動配送されない」という本質的な分岐を、これで2回取り違えた。
2. **`spawn_pane` / `spawn_codex_pane` は生成したペインを自動でフォーカスする。** 直後に非フォーカス条件で測るなら明示的に戻す必要がある。
3. **保留の観察中に `send_keys` を送らないこと。** composer の内容が変わると保留解除条件を踏むため、「永久に届かない」という誤った結論に至る。実際には非フォーカスなら例外なく自動配送される。
4. **`inspect_pane` は PTY のグリッドしか返さない。** renga が合成するダイアログは映らないので、その確認は目視に頼るしかない。read-only であり配送挙動には影響しない（当初 `inspect_pane` が配送を止めていると疑ったが、フォーカス取り違えによる誤認だった）。
5. **Codex ペインで Ctrl+Enter を押すとき、ダイアログが出ていなければキーは Codex へ落ちる。** composer に下書きがあると、その下書きが Codex へ送信され会話履歴に残る。汚染されたペインは以降のターンで残留内容に言及し続けるため、測定は新規ペインで行うこと。

---

### 付録: 参照箇所インデックス

- nudge 本文の組み立て: `src/app/codex_peer.rs:432-446`
- 送信エントリポイント / フォーカス分岐: `src/app/codex_peer.rs:473-560`（`handle_peer_send`）、`:518-531`
- フォーカス中ペインへの即時経路: `src/app/codex_peer.rs:716-760`（`route_focused_codex_peer_message`）
- 保留キューの投入: `src/app/codex_peer.rs:669-691`
- 通知の表示 / 可視性 / 再キュー: `src/app/codex_peer.rs:693-831`
- **accept（問題①）**: `src/app/codex_peer.rs:833-859`
- 画面解析（`ready_for_nudge` / `has_draft` / `can_queue_message`）: `src/app/codex_peer.rs:159-225,359-416`
- 配送ステートマシン: `src/app/codex_peer.rs:903-1114`（`flush_pending_codex_peer_messages`）
- キー処理（問題 5.1）: `src/app/keyboard_input.rs:40-54`
- タイミング定数: `src/app/codex_peer.rs:4-9`
  - `CODEX_PEER_NUDGE_COMMIT_DELAY = 1000ms`
  - `CODEX_PEER_NUDGE_COMMIT_TIMEOUT = 5s`
  - `CODEX_PEER_DRAFT_STALL_TIMEOUT = 1500ms`
  - `CODEX_PEER_NUDGE_MAX_RETRIES = 1`

---

## 9. 追補: background terminal 待機中の native queue（2026-09-06）

gameocr のフィールド trace で、非フォーカス・composer 空の Codex が次の表示のまま
25分20秒 peer nudge を受け取れない事例を捕捉した。

```text
• Waiting for background terminal (8m 55s • esc to interrupt) · 1 background terminal running · /ps to view · /st…
└ python .repro/seal_gameocr_c5rl_causal_trace.py
```

従来の画面解析は、prompt の上にある status 行が `Working (` で始まる場合だけ
`native_queue_busy` と認識していた。このため上記では `can_queue_message=false`、
一方で `esc to interrupt` により idle の Enter 経路も拒否され、turn 終了まで
`AwaitFocus` に留まった。

Codex CLI v0.153.4 の実機で同じ状態を作り、composer 空では footer が
`gpt-5.6-sol medium · <cwd>`、文字を入れると
`tab to queue message                98% context left` に変わることを確認した。
したがって Codex 自身がこの状態でも native queue を提供している。

狭いペインでは同じ status が次のように末尾省略されることも実測した。

```text
◦ Waiting for background terminal (1m 24s • esc to inte…
```

この場合も busy と認識できないと idle の Enter 経路が開く。そのため認識条件は
完全な `esc to interrupt` ではなく、prompt 上の status 行が既知 label と
数字で始まる elapsed を持つこととする。認識する status label は現在次の3つ。

- `Working`
- `Thinking`
- `Waiting for background terminal`

いずれも prompt 上の status 行が `<label> (<数字>` で始まる必要がある。
認識済み status が残る間は Enter を許可しない。実際に Tab を押す条件は従来どおり
composer 下の `tab to queue message` footer で二重に確認する。transcript 内の
`Thinking (see below)` のような数字で始まらない文言、途中で折り返された status、
未知の label は native queue に使わない。未知 label でも完全な
`esc to interrupt` が見える場合は従来どおり Enter を拒否する。

さらに label の追加や改名で同じ危険が再発しないよう、prompt 上の未知 label でも
`<英字 label> (<数字>…` の形で行末が `…` なら、Codex が status を省略したものとして
idle の Enter 経路を拒否する。この安全網は `native_queue_busy` を立てないため、未知
status 中に draft を書くことも Tab を押すこともない。数字がない
`Reticulating (see below)…` のような行は対象外で、従来の挙動を維持する。

debug trace には `native_queue_status_label` を追加した。認識済みなら上記 label、
数字 elapsed を持つ未知 label なら `unknown`、該当 status がなければ `null` を
記録する。環境変数
`RENGA_DEBUG_CODEX_PEER_LOG` が未設定なら、従来どおり trace は出力しない。
