# 調査レポート: GitHub Copilot CLI で renga-peers 相当の通信ができるか

- 種別: **机上調査 + バイナリ解析**(コード変更なし。成果物は本ドキュメント1本)
- 調査日: 2026-08-30
- 対象: GitHub Copilot CLI **v1.0.82**(2026-08-29 リリース、`@github/copilot-win32-x64@1.0.82`)
- 手段: 公式 docs / changelog / GitHub issue、および npm tarball を scratchpad に展開して `copilot.exe --help` と各 help topic を実行、`app.js` / `runtime.node` / 同梱 `copilot-sdk/*.d.ts` の文字列解析
- 未実施: 実機での TUI 動作確認(ローカルに Copilot CLI 未インストール、GitHub ログインも未実施)

---

## 0. TL;DR

**できる。しかも Codex より筋の良い受信経路がある。**

| renga が Codex に対してやっていること | Copilot CLI での対応 | 状態 |
|---|---|---|
| MCP stdio サーバー登録 (`codex mcp add`) | `copilot mcp add renga-peers --env ... -- renga.exe mcp-peer` / `~/.copilot/mcp-config.json` | ◎ そのまま |
| `RENGA_PANE_ID` 等の env passthrough (`env_vars = [...]` パッチ) | `env` の値を `"$RENGA_PANE_ID"` のように **`$` 参照**で書く (PATH 以外は継承されない) | ◎ 同種のパッチが必要 |
| `RENGA_PEER_CLIENT_KIND=codex` で client 種別通知 | 同じ仕組みで `=copilot` を渡せる | ◎ (renga 側に enum 追加) |
| `check_messages` / `send_message` の自動承認 | `--allow-tool 'renga-peers'`(サーバー名だけで全ツール許可) / `--yolo` | ◎ |
| MCP `instructions` を system prompt に載せる | **既定では allowlist 済みサーバー以外は載らない** → `--allow-all-mcp-server-instructions` が必須 | △ 起動フラグで解決 |
| OSC タイトルで Codex ペイン検出 | OSC 0 で `GitHub Copilot` / `<session title> - GitHub Copilot` を出す | ◎ `"copilot"` 部分一致で可 |
| Claude channels のような MCP push 通知 | **無い**(MCP client が購読するのは `tools/list_changed` `resources/list_changed` `progress` のみ) | ✕ |
| composer に nudge 文字列を打ち込んで Enter (screen scraping) | 可能だが TUI 描画未検証 | ? |
| (Codex には無い) 外部からの正規のメッセージ注入 | **Extension** (`extension.mjs`) が foreground session に attach して `session.send()` できる。**`--ui-server`** で TUI + JSON-RPC server も可 | ◎ 本命 |

推奨: **Extension 経由の注入を主経路**にし、screen scraping は使わない(または最終フォールバック)。

---

## 1. 確認できた Copilot CLI の仕様

### 1.1 MCP サーバー登録

- 設定ファイル: `~/.copilot/mcp-config.json`(`COPILOT_HOME` で変更可)、workspace は `.mcp.json` / `.github/mcp.json`、セッション限定は `--additional-mcp-config <json|@file>`(複数可、後勝ち)
- CLI: `copilot mcp add <name> [--env K=V]... [--tools "*"] -- <command> [args...]`、`copilot mcp get <name>`、`copilot mcp list`、`copilot mcp remove <name>`
- スキーマ例(公式 docs):
  ```json
  { "mcpServers": { "playwright": { "type": "local", "command": "npx", "args": ["@playwright/mcp@latest"], "env": {}, "tools": ["*"] } } }
  ```
  `type` は `local`(= stdio)/ `http` / `sse`。

### 1.2 環境変数の継承 — **PATH 以外は継承されない**

公式 docs: *"The `PATH` variable is automatically inherited from your environment. All other environment variables must be configured here."*

changelog 0.0.340 (2025-10-13): *env ブロックの値の先頭に `$` を付けると環境変数への参照として扱われる*。runtime 側には `envValueMode`(`session.mcp.setEnvValueMode`)があり、ACP モードだけ `{mode:"direct"}`(値をそのまま渡す)。通常の対話モードは参照モード。

→ renga が Codex 向けにやっている `env_vars = ["RENGA_PANE_ID", ...]` パッチと同等に、Copilot では

```json
"env": {
  "RENGA_PEER_CLIENT_KIND": "copilot",
  "RENGA_PANE_ID": "$RENGA_PANE_ID",
  "RENGA_SOCKET": "$RENGA_SOCKET",
  "RENGA_TOKEN": "$RENGA_TOKEN"
}
```

を書き込む必要がある(`copilot mcp add --env RENGA_PANE_ID='$RENGA_PANE_ID'` でも可、`--show-secrets` 無しだと表示上マスクされる)。
おまけ: MCP サーバーとシェルには `COPILOT_AGENT_SESSION_ID` が自動注入される (1.0.29)。`--secret-env-vars` で指定した変数は MCP 環境から剥がされるので、renga の変数を指定しないこと。

### 1.3 ツール承認

`copilot help permissions`:

```
<mcp-server-name>(tool-name?)
    Exactly matches a specific tool from a specific MCP server, or all tools from that server if omitted.
```

- `--allow-tool 'renga-peers'` で renga-peers の全ツールを無承認化(`--allow-tool 'renga-peers(check_messages)'` で個別も可)
- `--allow-all-tools` / `--allow-all` / `--yolo`(= all-tools + all-paths + all-urls)、対話中は `/allow-all` `/yolo`
- 承認結果は `~/.copilot/permissions-config.json` に保存(git root / cwd 単位)
- `defaultPermissionMode: "allow-all"` を config.json に置けば新規対話セッションが常時 allow-all で起動 (1.0.82)
- Codex の「承認がペインローカル」問題に相当するものは無さそう(保存単位は repo root)

### 1.4 MCP サーバーの `instructions` は既定で無視される

`--allow-all-mcp-server-instructions`: *"Include initialization instructions from all MCP servers in the system prompt instead of only allowlisted servers"*。runtime に `mcpRegistryServerInstructionAllowlist`(GitHub 純正サーバーの固定リストと思われる)がある。

→ renga の `instructions_blob()`(「peer message が来たら即応答せよ」等)は**このフラグ無しでは Copilot に読まれない**。`spawn_copilot_pane` の起動コマンドに必ず付けるか、guidance を別経路(extension の `onSessionStart` → `additionalContext`、または tool description)で渡す。

### 1.5 ターミナルタイトル (ペイン検出用)

- 起動時 OSC 0 で `GitHub Copilot`、セッション自動命名後は `<session title> - GitHub Copilot` (1.0.66 "Format terminal titles with the session title and GitHub Copilot suffix"; `app.js` の `setTitle(n?`${n} - GitHub Copilot`:"GitHub Copilot")` で確認)
- 無効化: config `updateTerminalTitle: false` または env `COPILOT_DISABLE_TERMINAL_TITLE=1|true`
- 既知バグ: #4121 (1.0.70 で SSH 経由だとタイトルが `;` に化ける)
- OSC 9;4 進捗インジケータも出す(`terminalProgress`)

→ `pane.rs` の `title_mentions_client(&t, "copilot")` で `is_copilot_running()` / `copilot_ever_seen()` を Codex と同型で作れる。ただし renga #209 と同じく、セッション自動命名で `codex` / `claude` という語がタイトルに混ざる可能性があるので、`peer_client_kinds` 登録を優先する現行ロジックを維持すること。

### 1.6 起動フラグ(`copilot --help` より抜粋、v1.0.82)

公開: `-i/--interactive <prompt>`(対話モードで初期プロンプト自動実行)、`-p/--prompt`(非対話)、`--acp`、`--allow-tool`、`--deny-tool`、`--allow-all-tools`、`--allow-all`/`--yolo`、`--allow-all-mcp-server-instructions`、`--additional-mcp-config`、`--disable-mcp-server`/`--enable-mcp-server`、`--experimental`、`--mode interactive|plan|autopilot`、`--model`、`-n/--name`、`--session-id`、`-r/--resume`、`--continue`、`--no-ask-user`、`--secret-env-vars`、`-C <dir>`

**隠しフラグ**(`.hideHelp()`、`app.js` で確認):

| フラグ | 説明文 |
|---|---|
| `--server` | Enable headless JSON-RPC server mode |
| `--headless` | alias for `--server` |
| `--ui-server` | **Enable TUI with embedded JSON-RPC server** |
| `--managed-server` | requires `--server` |
| `--port <port>` | Port to listen on when in server mode (default: random available port) |
| `--host` | (ACP/server の bind host) |
| `--session-idle-timeout <seconds>` | |

`--server`/`--headless` と `--ui-server` は同時指定不可。server 起動時は stdout に `CLI server listening on port N.` を出力。`COPILOT_CONNECTION_TOKEN` 未設定だと *"connections will be accepted from any client"* 警告。

### 1.7 Extensions(本命)

同梱 `copilot-sdk/docs/extensions.md` / `agent-author.md` / `examples.md` と `runtime.node` の文字列から:

- 配置: `.github/extensions/<name>/extension.mjs`(project)/ **`~/.copilot/extensions/<name>/extension.mjs`**(user)/ plugin 同梱 / session 限定。`.mjs` のみ(`extension.cjs` `extension.js` も discovery 対象の文字列は存在)
- 起動: CLI が `preloads/extension_bootstrap.mjs` を **CLI 自身の Node ランタイム**(`launcherProgram: process.execPath`、copilot.exe は Node SEA)で子プロセス起動。`EXTENSION_PATH` / `SESSION_ID` / `COPILOT_SDK_PATH` / `COPILOT_EXTENSION_PARENT_PID` を渡し、親 PID 監視で自動終了。**別途 Node.js のインストールは不要**と読める
- env: `blockedEnv`(`--secret-env-vars` 等)を除いて CLI の環境を継承(`app.js` の spawn 設定より) → `RENGA_SOCKET` / `RENGA_TOKEN` / `RENGA_PANE_ID` が読める
- 接続: `import { joinSession } from "@github/copilot-sdk/extension"` → `await joinSession({tools, hooks})` で **ユーザーの現在の foreground session に attach**
- 注入 API:
  ```js
  await session.send({
    prompt: "...",                  // モデルに渡す本文
    displayPrompt: "...",           // タイムライン表示用(省略時は prompt)
    mode: "enqueue" | "immediate",  // enqueue=次ターン待ち(既定) / immediate=実行中ターンへの割り込み(steering lane)
  });
  ```
  `user.message` イベントには `source`(出所ラベル。`agent-<id>` 等)と `delivery: "steering"` が乗る。`session.log(msg, {level, ephemeral})` でタイムラインに非モデル向け表示も出せる
- hooks: `onSessionStart`(`additionalContext` を返せる)、`onUserPromptSubmitted`、`onPreToolUse`、`onPostToolUse`、`onPostToolUseFailure`、`onSessionEnd`、`onErrorOccurred`
- ライフサイクル: `/clear` や foreground session 差し替えで再ロード、CLI 終了で SIGTERM
- **ゲート**: feature flag `EXTENSIONS`("Enable extensions ... programmatic tools and hooks via @github/copilot-sdk")は availability が `experimental` / `staff-or-experimental` → **`--experimental`(または config `experimental: true`)が必要**。加えて config `extensions.mode` が `disabled | load_only | load_and_augment`(既定 `load_and_augment`)。`/extensions` コマンドで切替
- 注意: extension の stdout は JSON-RPC 用なので `console.log` 禁止(stderr は `~/.copilot/logs` に落ちる)

### 1.8 `--ui-server`(次点)

- 同梱 SDK `client.d.ts`: `getForegroundSessionId()` / `setForegroundSessionId()` / `onLifecycle("session.foreground", ...)` は *"Only available when connecting to a server running in TUI+server mode (--ui-server)"*
- SDK 側は `RuntimeConnection.forUri("localhost:<port>")` で既存 runtime に接続 → `resumeSession(id)` → `send()`
- **公式 docs には未掲載**。copilot-sdk Discussion #1114 / Issue #1134 で「動くが undocumented。ポートの発見方法が無い」とされている。renga から使うなら `--ui-server --port <renga が決めた port>` + `COPILOT_CONNECTION_TOKEN` を渡し、renga が JSON-RPC を直接叩く(Node SDK に依存しないなら `copilot-sdk/generated/rpc.d.ts` / `schemas/api.schema.json` からメソッドを起こす)
- Agents ビューに "live / ui-server" セッションとして列挙され、他の CLI から見えるレジストリがある(`LiveCamelEntry`、`sessionId@pid@host:port`)

### 1.9 その他の補助手段

- **hooks** (`~/.copilot/hooks/*.json` / config `hooks`): `agentStop` で `{"decision":"block","reason":"..."}` を返すと reason が次ターンの user message として enqueue され、ターンが継続する(8 回連続で打ち切り)。`notification` hook(CLI のみ、非同期、`additionalContext` 可)は permission prompt / elicitation / agent completion / shell completion で発火。「ターン終了時に renga inbox を見て、残っていたら check_messages を強制」という pull 型の保険に使える
- **`/every` `/after`** スケジュールプロンプト(feature flag `EVERY_AND_AFTER` も experimental 系)。busy 中は steering として配送 (1.0.72)。`/every 1m` で `check_messages` をポーリングさせる最低限の代替になる
- **`-i "<prompt>"`** で起動直後に 1 発だけ注入できる(spawn 時の初期指示に使える)
- MCP `elicitation/create` / `sampling` は runtime に文字列あり(1.0.81 で MCP 2026-07-28 対応)。ただし server→client の任意 push は無い

---

## 2. renga に必要な変更(概算)

1. `PeerClientKind::Copilot` 追加(`mcp_peer/mod.rs` の `parse_client_kind`、`instructions_blob`、`initialize` の capability 分岐。Claude 用 channel capability は出さない)
2. `renga mcp install --client copilot`:
   - `copilot mcp add renga-peers --env RENGA_PEER_CLIENT_KIND=copilot --env 'RENGA_PANE_ID=$RENGA_PANE_ID' --env 'RENGA_SOCKET=$RENGA_SOCKET' --env 'RENGA_TOKEN=$RENGA_TOKEN' --tools '*' -- <renga.exe> mcp-peer`
   - もしくは `~/.copilot/mcp-config.json` を直接 upsert(Codex の `ensure_codex_env_var_passthrough` 相当)
   - `~/.copilot/extensions/renga-peers/extension.mjs` を書き出す(renga バイナリに埋め込み)
   - 検証関数 `verify_copilot_renga_peers_install()`(env の `$` 参照と extension の存在)
3. `spawn_copilot_pane`: 既定コマンド
   `copilot --experimental --allow-all-mcp-server-instructions --allow-tool renga-peers [--yolo] [--name <pane name>] [-i "<initial prompt>"]`
   (`--yolo` は Codex と同様に memory の運用方針に従う)
4. `pane.rs`: `copilot_seen` / `is_copilot_running()`(タイトル `"copilot"` 部分一致)
5. 配送:
   - **A. extension 直接注入**: `handle_peer_send` で `PeerClientKind::Copilot` の場合は `PeerInbox` イベントを出すだけにし、extension 側が renga の IPC(`RENGA_SOCKET`)に接続して `PeerInbox` を受け、`session.send({prompt: <banner付き本文>, displayPrompt: "📡 peer message from <name>", mode: "immediate"})` する。Codex 用の nudge ステートマシン(`codex_peer.rs`)は不要
   - **B. nudge のみ注入 + check_messages で pull**: extension が `send({prompt: format_codex_peer_message(...)})` だけ行い、本文は MCP `check_messages` で取る。Codex と挙動を揃えたい場合
   - A の方が単純で、busy 中も steering で即届く。ただし immediate はユーザーの操作中ターンに割り込むので、renga 側で「focus 中のペインには enqueue、非 focus には immediate」等の方針は要検討
6. スキル/ドキュメント: `renga-issue-shogun` 等で Copilot 家来を選べるように

---

## 3. 未検証事項(実機で確認が必要)

1. `--experimental` 無しで extension がロードされるか(フラグ判定は `experimental` / `staff-or-experimental` availability。一般ユーザーは `--experimental` 必須と読んでいる)
2. extension の `session.send()` が TUI のタイムラインにどう描画されるか(`source` / `delivery:"steering"` の見え方、`displayPrompt` の反映)
3. `--ui-server --port` で外部 client から foreground session に `send()` できるか(SDK docs の記述からは可能だが undocumented)
4. TUI の composer 形状(screen scraping fallback を作る場合のみ必要)
5. Windows での extension 子プロセス起動と env 継承(`blockedEnv` の中身)
6. `copilot mcp add --env 'K=$VAR'` が値をそのまま(展開せず)書き込むか — PowerShell 5.1 の引用に注意

---

## 4. 参照

- GitHub Docs: [Adding MCP servers for Copilot CLI](https://docs.github.com/en/copilot/how-tos/copilot-cli/customize-copilot/add-mcp-servers) / [Allowing and denying tool use](https://docs.github.com/en/copilot/how-tos/copilot-cli/use-copilot-cli/allowing-tools) / [Hooks reference](https://docs.github.com/en/copilot/reference/hooks-reference) / [Copilot SDK backend services (server mode)](https://docs.github.com/en/copilot/how-tos/copilot-sdk/setup/backend-services)
- copilot-cli changelog: 0.0.340 (env `$` 参照)、1.0.18 (notification hook)、1.0.28 (`COPILOT_DISABLE_TERMINAL_TITLE`)、1.0.29 (`COPILOT_AGENT_SESSION_ID`)、1.0.49 (`--additional-mcp-config` in server mode)、1.0.66 (title format)、1.0.72 (scheduled prompts as steering)、1.0.76 (queue manager)、1.0.82 (`defaultPermissionMode`)
- Issues/Discussions: [copilot-sdk#1134 TUI server mode](https://github.com/github/copilot-sdk/issues/1134)、[copilot-sdk discussion #1114 `--ui-server`](https://github.com/github/copilot-sdk/discussions/1114)、[copilot-cli#2676 title 無効化](https://github.com/github/copilot-cli/issues/2676)、[copilot-cli#4121 title 化け](https://github.com/github/copilot-cli/issues/4121)、[copilot-cli#2966 複数セッション管理](https://github.com/github/copilot-cli/issues/2966)
- 同梱ファイル(`@github/copilot-win32-x64@1.0.82`): `copilot-sdk/docs/{extensions,agent-author,examples}.md`、`copilot-sdk/{client,session,types,extension}.d.ts`、`copilot-sdk/generated/rpc.d.ts`、`preloads/extension_bootstrap.mjs`、`app.js`、`prebuilds/win32-x64/runtime.node`
