# lavalink-rs-server

Lavalink v4 の REST / WebSocket プロトコルと**完全互換**を目指す Rust 実装。
既存の Java/JS クライアント（`lavalink-client`, `wavelink` 等）からそのまま接続できることがゴール。
YouTube 再生は外部依存（yt-dlp 等）なしの Rust ネイティブ抽出を予定。

> [!NOTE]
> このプロジェクトのコードは AI（Anthropic の Claude）を用いて作成されています。
> 利用の際はご自身でコードの内容・ライセンス・セキュリティをご確認ください。

> 進捗: **フェーズ2 完了**（REST/WS プロトコル層 + モックプレイヤー）。
> 実音声（Discord Voice / 音声処理 / YouTube 抽出）は未実装＝**まだ音は出ない**。
> 現状は「実クライアントが接続・操作でき、状態とイベントが正しく流れる」段階。

## ワークスペース構成

| クレート | 役割 | 状態 |
|---|---|---|
| `protocol` | REST/WS の DTO 定義 (serde, v4 と 1:1) | ✅ フェーズ2 |
| `track-codec` | encoded track の encode/decode（Lavaplayer 互換, v2/v3） | ✅ フェーズ2 |
| `player` | モックプレイヤー状態機械（位置計算・終了検知） | ✅ フェーズ2 |
| `server` | HTTP/WS サーバー（バイナリ `lavalink-rs`） | ✅ フェーズ2 + 実player結線 |
| `audio-pipeline` | デコード/リサンプル/フィルタ/Opus エンコード | 🟡 フェーズ4 (実装済・要コンパイル確認) |
| `discord-voice` | Discord Voice Gateway + UDP/RTP（+ DAVE E2EE） | 🟡 v0実装済 / DAVE土台実装済(MLS結線は3-5c) |
| `source-youtube` | YouTube 抽出（InnerTube, Rust ネイティブ） | 🟡 フェーズ5 (土台・要実機/PoToken) |

## 実装済みエンドポイント（フェーズ2）

- `GET /version`（認証不要）
- `GET /v4/info`, `GET /v4/stats`
- `GET /v4/loadtracks?identifier=`（**モック結果**: `ytsearch:` 等で search、`list=` で playlist、それ以外は track）
- `GET /v4/decodetrack`, `POST /v4/decodetracks`（実バイナリ形式でデコード）
- `PATCH /v4/sessions/{sessionId}`（resuming/timeout）
- `GET|PATCH|DELETE /v4/sessions/{sessionId}/players/{guildId}`（部分更新・`noReplace` 対応）
- `GET /v4/sessions/{sessionId}/players`
- `GET /v4/websocket`（`ready`/`playerUpdate`/`stats`/`event` を送出。resuming 対応）

`PATCH player` はトラック操作で `TrackStartEvent`/`TrackEndEvent`(replaced/stopped) を、
再生位置が length/endTime に達すると `TrackEndEvent(finished)` を WS に push する。

## ビルド / 実行

```sh
# 一部の依存（rustls 系等）のネイティブビルドで CMake が必要。
# 新しめの CMake で古いポリシーが必要な場合は次を設定:
export CMAKE_POLICY_VERSION_MINIMUM=3.5    # Windows: set CMAKE_POLICY_VERSION_MINIMUM=3.5

cargo test --workspace          # ユニットテスト（track-codec round-trip, config, player 位置計算）
cargo run --bin lavalink-rs     # application.yml を読んで 2333 で待受

curl localhost:2333/version
curl -H "Authorization: youshallnotpass" "localhost:2333/v4/loadtracks?identifier=ytsearch:never%20gonna"
```

実クライアントでの疎通（推奨進め方 #2）:
`lavalink-client`(discord.js) や `wavelink`(discord.py) のノード設定を
`host=localhost / port=2333 / password=youshallnotpass` にして接続 →
`ready` 受信、loadtracks、player PATCH（play/pause/seek/volume）まで通ることを確認する。
※ 実際の音声送出はフェーズ3・4 実装後。

## YouTube ソース（フェーズ5 土台）

`source-youtube`（`YoutubeClient`）が InnerTube `/youtubei/v1/player`(ANDROID/IOS/TVHTML5 フォールバック)
と `/youtubei/v1/search`(WEB) を叩く。`signatureCipher` を伴わず直接 `url` を持つ AAC/m4a(itag 140)
を選ぶ（symphonia でデコード可能なため）。server 結線:

- `GET /v4/loadtracks?identifier=ytsearch:<query>` → 検索結果（複数 track）
- `GET /v4/loadtracks?identifier=<youtube URL or 11文字ID>` → 単曲
- 再生時に playback が `stream_url(videoId)` で**その場で直リンクを再解決**してから取得・デコード
  （URL の有効期限対策）

```sh
cargo build --workspace
# Discord クライアントから: /play ytsearch:never gonna give you up  /  /play <youtube URL>
```

### 推奨: invidious-companion 連携（PoToken/署名復号を丸ごと委譲）

PoToken の自前取得が難しいので、`iv-org/invidious-companion`（Deno サービス）に解決を委譲できる。
companion は `bgutils-js` + `YouTube.js` で **PoToken 生成・署名復号・URL 解決**を行い、
`POST /companion/youtubei/v1/player`（Bearer 認証, body `{"videoId":..}`）で
**復号済みの再生可能 URL を含む player JSON** を返す（レスポンス形は YouTube と同じ）。

導入:
1. companion を起動（例）: `SERVER_SECRET_KEY=CHANGEME deno task dev`（既定 `127.0.0.1:8282`）
2. 本サーバーに環境変数を設定して起動:
```bat
set YT_COMPANION_URL=http://127.0.0.1:8282
set YT_COMPANION_SECRET=CHANGEME
cargo run --bin lavalink-rs
```
これで `resolve_stream`/`resolve_meta` は companion を優先し、`no playable format` を回避できる。
検索（`ytsearch:`）は従来どおり直 InnerTube（PoToken 不要）。companion 未設定時は下記の直接 PoToken 配管にフォールバック。

### PoToken（YouTube 直リンク取得に必須）

YouTube の `/player` は PoToken が無いと直リンクを返さない（`no playable format found`）。
PoToken は BotGuard（難読化 JS）で生成されるため **純 Rust では生成不可**。本実装は
「外部供給 or 手動トークンを差し込む配管」だけを持ち、生成は外部に委譲する（yt-dlp / lavalink
youtube-source と同方針）。環境変数で与える:

- `YT_POTOKEN` + `YT_VISITOR_DATA`: ブラウザの DevTools → `/youtubei/v1/player` リクエストの
  `serviceIntegrityDimensions.poToken` と `context.client.visitorData` を貼る（手早い検証用・数時間で失効）。
- `YT_POTOKEN_PROVIDER`: bgutil 互換の供給サーバ URL（`POST {url}/get_pot {"content_binding":..}` →
  `{"po_token":..}`）。自動化したい場合はこちら（別途 Node サービスを常駐）。

```bat
set YT_VISITOR_DATA=<ブラウザの visitorData>
set YT_POTOKEN=<ブラウザの poToken>
cargo run --bin lavalink-rs
```

署名復号(JS)を避けるため ANDROID/IOS クライアントを使用（直 `url` 形式）。それでも client/format の
正しい組合せは流動的で、実機での調整が要る場合がある。

> ⚠️ **2026 の YouTube は PoToken / BotGuard が必須化されつつあり、本実装だけでは直リンクが
> 取得できない（403/throttled）場合がある。** その場合は PoToken/OAuth 供給（5-2 / 5-7）が必要で、
> これが本プロジェクトで最も壊れやすく継続メンテが要る部分。クライアントのバージョン文字列も要定期更新。
> また DAVE 必須チャンネルでは音声接続自体が 4017 になる点は従来どおり（非DAVE/Stage で動作）。

## DAVE（音声 E2EE）— フェーズ3-5（ADR-0001 の本命）

`4017 'E2EE/DAVE protocol required'` の実測により、通常VCでは DAVE が必須と確定。
`discord-voice` の `dave` feature 配下に **フレームレベル E2EE の土台**を実装済み（検証可能な純ロジック）:

- `dave/uleb128.rs` ULEB128 可変長エンコード
- `dave/frame.rs` AES-128-GCM フレーム暗号（**8byte 切詰めタグ**, 96bit nonce 展開, `0xFAFA` フッタ）
- `dave/ratchet.rs` 送信者鍵ラチェット（MLS ExpandWithLabel / HKDF-SHA256）
- `dave/opcodes.rs` Voice Gateway バイナリ opcode 21-31 フレーミング
- `dave/mod.rs` 定数（ciphersuite `0x0002`, exporter label "Discord Secure Frames v0"）と `GroupKeySource` トレイト

```sh
cargo test -p lavalink-discord-voice --features dave
```

検証: ULEB128・nonce展開・フッタ構成・ExpandWithLabel形式を Python で独立確認。
フレームE2EE(AES-128-GCM/8byteタグ)の OPUS 往復・改ざん検知も Python(cryptography) で再現一致。

### フェーズB（gateway 結線・骨組み）実装済み

- `dave/session.rs` opcode 21-31 状態機械（`MlsBackend` トレイト + `NoopMls` スタブ）
- `dave/cryptor.rs` 送信フレーム暗号器（ラチェット鍵 + nonce 管理 → `frame::encrypt_opus`）
- gateway 結線: Identify の `max_dave_protocol_version`、Session Description の `dave_protocol_version` 取得、
  reader でのバイナリ WS（opcode 21-31）送受信ルーティング

```sh
cargo test -p lavalink-discord-voice --features dave
```

### フェーズ結線（DAVE を音声経路へ接続）

opcode の正確な形式（whitepaper）に合わせて実装:
- JSON: 21 prepare_transition / 22 execute / 24 prepare_epoch /（送信）23 ready / 31 invalid
- binary: 25 external_sender / 27 proposals / 29 announce_commit / 30 welcome /（送信）26 key_package / 28 commit_welcome
- 29/30 は先頭 `uint16 transition_id`、27 は先頭 `operation_type`

`session.rs` を JSON/binary 分離で書き直し、`reader_task` が両者を処理して応答送信、
グループ確立後 `sender_base_secret` から `FrameCryptor` を用意。`audio_task` は DAVE 有効時に
OPUS を E2EE 暗号化してから RTP トランスポート暗号で包む。受信した DAVE opcode は生バイトを
ログ出力する（実機デバッグ用）。

ビルド/実行（DAVE 有効）:
```bat
:: invidious-companion を起動（YouTube 解決用）
:: サーバーを DAVE 有効でビルド
cargo run -p lavalink-server --features dave --bin lavalink-rs
```

> ⚠️ **DAVE は実 Discord での反復デバッグが必須。** best-effort のため初回から通常VCで鳴くとは限らない。
> 未確定の要素（実機ログで詰める）: `handle_proposals`（当面 None=他メンバーの welcome に依存）、
> external sender extension のグループ組込み、op26 の MLSMessage ラップ、op30 の raw Welcome 解釈。
> 接続後に出る `dave json opcode` / `dave binary opcode` / `mls ...` ログを採取して順次修正する。

### フェーズA（openmls による MLS 鍵交換）実装中

- `dave/mls.rs`（feature = `dave-mls`）= openmls 0.6 で `OpenMlsBackend: MlsBackend` を実装。
  ciphersuite `MLS_128_DHKEMP256_AES128GCM_SHA256_P256`、key package 生成、Welcome 参加、
  commit 適用、`export_secret("Discord Secure Frames v0", LE(sender_id), 16)`。
- **Discord 抜きの自己テスト** `local_two_member_exporter_matches`: ローカル2者でグループを作り、
  両者が同一の sender base secret を導出できることを検証（openmls 機構＋exporter の確定用）。

```sh
cargo test -p lavalink-discord-voice --features dave-mls
```

> openmls 0.6 は API が広く、初回は型/メソッドの微差を要調整する可能性が高い（コンパイル/実機で反復）。
> 残課題: `process_proposals`（external sender からの proposal→commit 生成）、external sender extension の
> グループ組込み、各 opcode の正確なシリアライズ、reader への `OpenMlsBackend` 結線、`FrameCryptor` の送出結線。

> ⚠️ **まだ通常VCで音は出ません。** フェーズB は「DAVE対応を名乗り、opcode を授受する疎通の骨組み」まで。
> 実際に鍵交換が成立して音が流れるには **フェーズA（3-5c: openmls による MLS 鍵交換）** と、
> 送出経路への `FrameCryptor` 結線、実Discordでの反復テストが必要。MLS は実機検証が必須のため分離している。

## 音声処理パイプライン（フェーズ4）

`audio-pipeline` クレート: 圧縮音声 → デコード(symphonia) → 48kHz/stereo化 →
フィルタチェーン → Opus エンコード → 20ms フレーム列 → `discord-voice` へ供給。

フィルタ実装状況（`filters.rs`）:
- **厳密**: volume / channelMix / lowPass(one-pole) / tremolo
- **機能実装（数値は Lavaplayer と完全一致ではない・フェーズ6 で精緻化）**:
  equalizer(15バンド peaking biquad) / karaoke(センター除去) / vibrato(可変ディレイ) /
  rotation(オートパン) / distortion(波形整形)
- **未実装スタブ**: timescale(speed/pitch/rate) — 位相ボコーダ/WSOLA が必要なため後続

パススルー最適化(4-5): `can_passthrough(source_is_opus, filters)` = ソース Opus かつフィルタ無変更。

> 注: opus(libopus バインディング) と symphonia はネイティブ依存のため生成環境では未コンパイル。
> `cargo build -p lavalink-audio-pipeline` で要確認。フィルタ DSP の数式は Python で独立検証済み。

## 音声接続（フェーズ3, ADR-0001: v0 トランスポート）

`discord-voice` クレートに Voice Gateway v8 ハンドシェイク / UDP IP Discovery /
トランスポート暗号（`aead_xchacha20_poly1305_rtpsize` 必須・`aead_aes256_gcm_rtpsize` 優先）/
RTP 20ms 送出を実装。**DAVE(E2EE) は未実装**のため、DAVE 必須チャンネルでは `4017` で繋がらない
（ADR-0001 の割り切り。`docs/decisions.md` 参照）。

音声経路の単体疎通テスト（Bot が VC 参加時に得る値を環境変数で渡す）:

```sh
GUILD_ID=... USER_ID=... SESSION_ID=... VOICE_TOKEN=... VOICE_ENDPOINT=host:port \
  cargo run -p lavalink-discord-voice --example voice_smoke
```

5 秒間 無音 Opus を送出し、成功すれば Discord 上で Bot が「発話中」表示になる。

> 注: この段階では server の player とはまだ結線していない（実トラックの Opus を流すのは
> フェーズ4: 音声処理パイプライン）。`discord-voice` は ~600 行の async + 暗号 + WS のため、
> 生成環境では未コンパイル。`cargo build -p lavalink-discord-voice` で要確認。

## サーバー本体での実再生（server 結線）

server が REST の player 操作に応じて **実際に Discord 音声へ接続し、トラックの音声を流す**ように
結線済み（`crates/server/src/playback.rs`）。経路は playfile と同じ:
voice 受信 → `VoiceConnection::connect` / トラック → fetch → `AudioPipeline` → Opus → 送出。

- 現状ソースは **HTTP 直リンク**: `GET /v4/loadtracks?identifier=https://.../song.m4a` が再生可能な
  track を返し、`PATCH player` で voice + track を渡すと server がその URL を取得して再生する。
- 完了/失敗で `TrackEndEvent`(finished/loadFailed/stopped) を WS に push。

使い方（Discord クライアントから）: 通常どおり Bot を VC に入れ、`/play <直リンクURL>` を実行。

> ⚠️ **DAVE 必須チャンネルでは依然 4017 で繋がりません**（音声送出は v0 のまま）。実際に音が出るのは
> 非 DAVE / Stage チャンネル。通常 VC で鳴らすにはフェーズ3-5 の DAVE 統合完了が必要。
> YouTube ソース（フェーズ5）も同じ再生経路に差し込む形で実装する。

## ローカルファイルで「実際に音を鳴らす」(playfile)

フェーズ3(音声送出) と フェーズ4(音声処理) を YouTube 無しで結線した end-to-end 疎通ツール。

手順:
1. サーバーを起動し、Bot を VC に参加させて `/play` などを 1 回実行する。
   サーバーログに音声情報が出る（`voice state received ... endpoint=.. session_id=.. token=..`）。
   ※ token/session は短命なので、取得したらすぐ次へ。
2. その値とファイルパスを渡して再生:

```sh
GUILD_ID=.. USER_ID=<botのID> SESSION_ID=.. VOICE_TOKEN=.. VOICE_ENDPOINT=host:port \
  cargo run -p lavalink-playfile -- path/to/audio.m4a
```

成功すれば VC で音声が再生される（送出側チャネルのバックプレッシャでリアルタイム整流）。
これが本プロジェクトで **初めて実際に音が出る** マイルストーン。

> 対応: MP4/AAC(.m4a) / WAV / MP3 / FLAC / Ogg-Vorbis / WebM-Vorbis。
> WebM/**Opus** は現状デコード非対応（パススルー実装はフェーズ5 以降）。

## テスト / 互換性検証

```sh
cargo test --workspace          # 全ユニット + 差分テスト
cargo test -p lavalink-server   # 公式 Lavalink との差分テストのみ
```

`crates/server/tests/conformance.rs`（公式 docs 由来の golden と突き合わせ）:
- `decodetrack` の出力が公式 Track JSON と一致（encoded バイナリ互換の実証, 6-3）
- エラー形状 `{timestamp,status,error,message,path}` の一致
- `loadtracks` / `update_player` の v4 スキーマ（camelCase キー）一致, 6-2
- WebSocket の `op`/`event` serde 出力が公式 docs と一致（入れ子タグ `op`→`type`、
  `guildId`/`byRemote`/`playingPlayers` 等の rename を検証）

実 Lavalink との値レベル差分は `scripts/capture_golden.sh`（Docker 起動 → golden 取得）。
既知の保留差分・手順は `docs/conformance.md` 参照。

## ⚠️ フェーズ0 の重要な調査結果（再掲）

`docs/phase0_design_verification.md` 参照。

- **Discord DAVE プロトコル（音声 E2EE）が 2026-03-01 から非ステージ音声で必須化**。
  非対応ボットは voice gateway に close code **4017** で拒否される。`discord-voice` の `dave`
  feature（MLS/AES-128-GCM）が事実上必須。プロジェクト最大のリスク。
- アーキテクチャ: **本サーバー自身**が Discord Voice Gateway(WSS v8)+UDP に接続して Opus を送出する。
- トランスポート暗号は `aead_xchacha20_poly1305_rtpsize`（必須）/ `aead_aes256_gcm_rtpsize`（優先）。

## 検証メモ

このスキャフォールドは cargo 不在の環境で生成しているため、コンパイルは利用者側 / CI で行う。
生成時には次の静的検証を実施済み:
- 全 `Cargo.toml` の TOML 妥当性・workspace 依存と package 名の一致
- `application.example.yml` と config 構造体キーの対応
- 全 `.rs` のデリミタ均衡
- track-codec の encode/decode ロジックを Python に移植し、公式 v2 サンプルのデコード一致＋v3 ラウンドトリップ（多言語/絵文字/ISRC）を確認

残リスク（要 `cargo test` での確認）: serde の入れ子内部タグ（`op`→`type`）と
変種ごとの `rename_all` の出力が Lavalink と一致するか。フェーズ6-2/6-3 の差分テストで担保予定。
