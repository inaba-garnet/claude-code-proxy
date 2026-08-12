# claude-code-proxy — Docker 運用

LAN 上の他端末から Claude Code のセッションを受け付けるためのコンテナ構成。

- プロキシ: `http://<host-ip>:18765`
- 認証情報: `./config` に永続化 (`config/codex/auth.json` 等、mode 0600)。`.gitignore` / `.dockerignore` 済み。

## 起動 / 停止

```sh
docker compose up -d          # 起動
docker compose logs -f        # ログ
docker compose down           # 停止
docker compose build          # ソース変更後の再ビルド
```

`serve` は stdout が TTY のときだけ TUI を出すため、コンテナ内では通常のログ出力になる。

## ダッシュボード

```
http://<host-ip>:18765/monitor
```

端末版の TUI と同じ画面がブラウザで見られる。`serve` は stdout が TTY のときだけ
TUI を出すため、コンテナでは端末版が使えない。compose.yml で
`CCP_WEB_MONITOR=1` を設定してあり、これが無いと**監視データ自体が集まらない**。

中身は TUI の描画をそのまま HTML に変換したもので、セッション一覧・実行中リクエスト・
完了履歴・エラーが同じレイアウトで出る。ブラウザの幅に応じて列数が変わるのも端末と同じ
(広いほど列が増え、170桁以上でスパークラインが出る)。更新は 250ms 間隔、画面に変化が
無い間は転送しない。

読み取り専用で、キー操作 (選択・詳細表示・終了) は送れない。停止は
`docker compose down` で行う。

**注意**: プロキシ本体と同じ無認証ポートに出る。18765 に到達できる端末からは、
セッションID・プロジェクト名・使用モデル・トークン消費量がすべて見える。絞るなら
compose.yml の ports を `127.0.0.1:18765:18765` のように bind アドレス指定にして、
SSH ポートフォワード経由で見ること。ダッシュボードだけ止めたい場合は
`CCP_WEB_MONITOR` を消せば `/monitor` は生えなくなる。

## Codex 認証

コールバック待ち受けは **1455 番ポート固定**。`redirect_uri` が
`http://localhost:1455/auth/callback` にハードコードされている点が要注意で、
ブラウザを動かす端末の **localhost:1455** がコンテナに届く必要がある。

コンテナ内の login コマンドは 127.0.0.1:1455 にしか bind しないため、
`entrypoint.sh` が socat でコンテナIP:1455 → 127.0.0.1:1455 を中継している
(0.0.0.0 で bind すると login 側の bind と衝突して EADDRINUSE になるので不可)。

### 方法A: デバイスコードフロー (推奨・ポート不要)

```sh
docker compose exec claude-code-proxy claude-code-proxy codex auth device
```

表示された URL とコードを手元のブラウザで入力するだけ。トンネル不要。

### 方法B: ブラウザOAuth (1455 経由)

作業する端末側で SSH トンネルを張り、その端末の localhost:1455 をホストに転送する:

```sh
# ← 手元のPC 側で実行
ssh -L 1455:localhost:1455 <user>@<host-ip>
```

トンネルを張ったまま、ホスト側で:

```sh
docker compose exec claude-code-proxy claude-code-proxy codex auth login
```

表示された URL を手元のブラウザで開く。認証後のリダイレクト先
`http://localhost:1455/auth/callback` がトンネル経由でコンテナに届く。

### 確認 / 解除

```sh
docker compose exec claude-code-proxy claude-code-proxy codex auth status
docker compose exec claude-code-proxy claude-code-proxy codex auth logout
```

Kimi / Grok / Cursor も同様 (`kimi auth login` は元々デバイスコードフロー)。

## OpenCode (OpenAI互換) のAPIキー

OpenCode 系モデル (deepseek / qwen / glm / kimi / minimax 等) は OAuth ではなく
APIキーを使う。`opencode.env` に置くと compose が env_file として読み込む。

```sh
cp opencode.env.example opencode.env
$EDITOR opencode.env          # CCP_OPENCODE_API_KEY= に値を入れる
docker compose up -d          # コンテナ再作成で反映
```

`opencode.env` は `.gitignore` / `.dockerignore` 済み。APIキーを compose.yml に
直接書かないために `environment:` ではなく `env_file` を使っている。

反映確認 (キー自体は表示しない):

```sh
docker compose exec claude-code-proxy sh -c 'echo "${CCP_OPENCODE_API_KEY:+set}"'
```

`CCP_OPENCODE_BASE_URL` は未設定でも空文字でも既定値
(`https://opencode.ai/zen/go/v1`) が使われる。別エンドポイントを使うときだけ設定する。

`grok-4.5` / `kimi-k3` / `kimi-k2.6` / `gpt-5.6-luna` はネイティブのプロバイダーと
ID が衝突するため、OpenCode 経由で使うには `opencode-go/` を前置する
(例: `opencode-go/kimi-k3`)。衝突しない ID はそのまま使える。

## エイリアスのルーティング (CCP_ALIAS_PROVIDER)

compose.yml で `CCP_ALIAS_PROVIDER=anthropic` を設定している。これにより:

- `claude-*` と エイリアス (`opus` / `sonnet` / `haiku`) → **api.anthropic.com へそのまま中継**
- `gpt-5.*` → Codex、`glm-*` 等 → OpenCode (従来どおり)

未設定時の既定は `codex` で、その場合 `claude-*` は Codex に翻訳されて処理される。

パススルーの認証は、クライアントが送った `x-api-key` / `authorization` ヘッダを
そのまま上流へ転送する方式 (`src/passthrough.rs` の HOP_BY_HOP に含まれないため)。
**サーバ側に Anthropic のキーを置く必要は無いし、置く場所も無い。**

**注意**: この設定下では `ANTHROPIC_AUTH_TOKEN="anything"` のようなダミー値では
`claude-*` 系が 401 になる。ダミー値で使いたい場合は `ANTHROPIC_MODEL` に
`gpt-5.*` や OpenCode のモデルを明示すること。

また `claude-*` はセッションアフィニティの対象外で、同一セッション内で Codex を
使っていても常に Anthropic へ向かう (「Claudeモデルは Claude のまま」という意図)。

このモードでは、プロキシが実装していないパスも Anthropic へ透過転送される
(Claude Code が起動時に `HEAD /` を叩くため)。他の alias provider では従来どおり
404 が返る。

## クライアント側の設定

他端末の Claude Code で:

```sh
export ANTHROPIC_BASE_URL="http://<host-ip>:18765"
export ANTHROPIC_AUTH_TOKEN="anything"
export ANTHROPIC_MODEL="gpt-5.6-sol"
export ANTHROPIC_SMALL_FAST_MODEL="gpt-5.6-luna"
export CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1
```

利用可能なモデル一覧:

```sh
docker compose exec claude-code-proxy claude-code-proxy models --full
```

## 注意点

- **プロキシに認証が無い**。18765 に到達できる端末は誰でも認証済みアカウントの
  クォータを消費できる。信頼できるネットワーク内に限定する前提の構成。外部に晒さないこと。
  必要なら compose.yml の ports を `<host-ip>:18765:18765` のように bind アドレス指定で絞る。
- レート制限は上流アカウント単位で全クライアント共有。
- Remote Control は `ANTHROPIC_BASE_URL` が api.anthropic.com 以外だと Claude Code 側が無効化する。
- `config/` にはリフレッシュトークンが平文で入る。バックアップ対象に含める場合は暗号化されることを確認する。
