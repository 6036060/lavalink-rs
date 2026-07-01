# フェーズ0 設計検証レポート — Lavalink Rust 再実装

調査日: 2026-06-20 / 対象: Lavalink v4 互換サーバーの Rust 実装
情報源は末尾「情報源」節を参照。各項目に **確度** を付した（◎=一次資料でバイト/仕様レベル確認、○=公式ドキュメント記載、△=二次情報・実装時に再確認推奨）。

---

## 0. 結論サマリ（先に読む）

1. **【最重要・計画外】Discord の音声 E2EE「DAVE プロトコル」が 2026-03-01 から非ステージ音声で必須化された。** DAVE 非対応クライアント／ボットは voice gateway に close code **4017** で拒否される。これは元の TODO リスト（フェーズ0-6 まで）が想定していない要素で、**音声を実際に流すには MLS ベースの E2EE 実装がほぼ必須**。本プロジェクト最大の技術リスク。詳細は §5。
2. アーキテクチャ前提は正しい。**Lavalink 自身が Discord Voice Gateway(WSS) と UDP/RTP に接続して Opus を送出する**（ボットクライアントは VOICE_STATE/VOICE_SERVER の情報を渡すだけ）。TODO 0-4 のカッコ内推測どおり「後者」で確定。§4。
3. encoded track のバイナリ形式は**公式サンプルをバイト単位で解析し完全確定**（§3）。version 2 のレイアウトを実証、version 3 の追加フィールド（artworkUrl, isrc）位置も特定。
4. 必須トランスポート暗号は **`aead_xchacha20_poly1305_rtpsize`（必須）** と **`aead_aes256_gcm_rtpsize`（利用可能なら優先）**。旧 `xsalsa20_poly1305*` 系は 2024-11-18 で全廃止。§5。
5. REST/WS のエンドポイントとイベントは v4 ドキュメントから全件確定（§1, §2）。元 TODO の「queue 系エンドポイント」は **v4 標準には存在しない**（キュー管理はクライアント側責務）。

**計画への影響:** フェーズ3（Discord音声層）に「DAVE/MLS」サブフェーズを追加し、依存 crate に MLS 実装と AES-128-GCM を加える必要がある。詳細な修正提案は §7。

---

## 0-1. REST API 全エンドポイント  ◎（公式ドキュメント全件）

全ルートは `/v4` プレフィックス（例外: `GET /version` のみプレフィックス無し）。`/v3` は v3.7+ 互換用だが新規実装では不要。ほとんどのルートは `Authorization` ヘッダ必須。

| メソッド / パス | 用途 | 備考 |
|---|---|---|
| `GET /v4/loadtracks?identifier=` | トラック解決 | `loadType`: `track`/`playlist`/`search`/`empty`/`error`。検索は `ytsearch:` `ytmsearch:` `scsearch:` プレフィックス |
| `GET /v4/decodetrack?encodedTrack=` | 単一 encoded → info | §3 のバイナリ形式 |
| `POST /v4/decodetracks` | 複数 encoded → info[] | body は string 配列 |
| `GET /v4/sessions/{s}/players` | プレイヤー一覧 | |
| `GET /v4/sessions/{s}/players/{guild}` | 単一プレイヤー取得 | |
| `PATCH /v4/sessions/{s}/players/{guild}?noReplace=` | **プレイヤー作成/更新（最重要・最複雑）** | 部分更新。§2-3 |
| `DELETE /v4/sessions/{s}/players/{guild}` | プレイヤー破棄 | 204 No Content |
| `PATCH /v4/sessions/{s}` | resuming 設定 | `{resuming, timeout}` |
| `GET /v4/info` | ノード情報 | version/git/jvm/sourceManagers/filters/plugins |
| `GET /v4/stats` | 統計 | **frameStats は常に null**（WS 版にのみ含まれる） |
| `GET /version` | バージョン文字列 | 唯一のプレフィックス無しルート |
| `GET /v4/routeplanner/status` | IP ローテ拡張 | 任意。MVP では省略可 |
| `POST /v4/routeplanner/free/address` | 失敗 IP 解除 | 任意 |
| `POST /v4/routeplanner/free/all` | 全失敗 IP 解除 | 任意 |

**元 TODO で挙がっていた `.../track/...` 等の queue 系エンドポイントは v4 公式 REST には存在しない。** キューは各クライアントライブラリが管理する設計なので、互換のために実装する必要はない。

**`PATCH .../players/{guild}` リクエストボディ（部分更新）の要点:**
- `track`（`{encoded?, identifier?, userData?}`）— `encoded` と `identifier` は排他。`encoded: null` で停止。旧 `encodedTrack`/`identifier` トップレベルは deprecated だが互換のため受理推奨。
- `position`(int), `endTime`(?int), `volume`(0–1000), `paused`(bool), `filters`(全置換), `voice`(`{token, endpoint, sessionId, channelId?}`)。
- **「送られたフィールドのみ更新、`null` で明示クリア」**のセマンティクスを正確に再現する必要あり → serde で `Option<Option<T>>`／`#[serde(default, skip_serializing_if)]` 等の三状態（未指定 / null / 値）設計が要る。

**エラー応答形式:** `{timestamp, status, error, trace?, message, path}`（`?trace=true` で stack trace 付与）。

---

## 0-2. WebSocket `/v4/websocket`  ◎（公式ドキュメント）

**ハンドシェイク必須ヘッダ:**
- `Authorization`: 設定パスワード
- `User-Id`: ボットのユーザー ID
- `Client-Name`: `NAME/VERSION` 形式
- `Session-Id`?: 再接続（resume）時に前回セッション ID

**サーバー→クライアント op（全4種）:**

| op | 内容 |
|---|---|
| `ready` | 接続確立直後。`{resumed: bool, sessionId: string}` |
| `playerUpdate` | x 秒毎（既定 5s, application.yml で設定）。`{guildId, state}` |
| `stats` | 1分毎。players/playingPlayers/uptime/memory/cpu/**frameStats**(WS版のみ非null) |
| `event` | プレイヤー/音声イベント（下表） |

**Player State オブジェクト:** `{time(ms unix), position(ms), connected(bool), ping(ms; 未接続は -1)}`

**event の type:**

| type | 追加フィールド |
|---|---|
| `TrackStartEvent` | `track` |
| `TrackEndEvent` | `track`, `reason`(`finished`/`loadFailed`/`stopped`/`replaced`/`cleanup`) |
| `TrackExceptionEvent` | `track`, `exception{message?, severity(common/suspicious/fault), cause, causeStackTrace}` |
| `TrackStuckEvent` | `track`, `thresholdMs` |
| `WebSocketClosedEvent` | `code`(Discord voice close code), `reason`, `byRemote` — Discord 音声 WS が閉じた時 |

`TrackEndReason` の `mayStartNext` フラグ（finished/loadFailed=true, それ以外=false）はクライアントの自動次曲再生判断に使われるため挙動を合わせる。

frameStats の `deficit`: 期待値はプレイヤーあたり 3000 フレーム/分（20ms毎=1分で3000）。正なら送出不足、負なら過剰。

---

## 0-3. encoded track バイナリフォーマット  ◎（公式サンプルをバイト解析で実証）

公式ドキュメントのサンプル encoded（Rick Astley）を base64 デコードし、145 バイトを完全にパースして以下を実証した。

**外側メッセージヘッダ（Lavaplayer の MessageOutput 相当）:**
- 先頭 `int32`（big-endian）: 上位2ビット = flags、下位30ビット = メッセージ長。
  - 実測: `0x4000008D` → flags=`0b01`, size=141。**flags のビット `0x40000000` が「バージョン付きトラック情報」を示す。**
- flags にバージョンビットが立っていれば、続く `1 byte` が **version**（実測 = 2）。

**version 2 のフィールド順（実測で全145バイト消費を確認）:**

| 順 | フィールド | 型 | 実測値 |
|---|---|---|---|
| 1 | title | Java UTF（u16 長 + UTF-8） | "Rick Astley - Never Gonna Give You Up" |
| 2 | author | Java UTF | "RickAstleyVEVO" |
| 3 | length | int64 (ms) | 212000 |
| 4 | identifier | Java UTF | "dQw4w9WgXcQ" |
| 5 | isStream | bool (1 byte) | 0 |
| 6 | uri | nullable text（present 1 byte + UTF） | "https://www.youtube.com/watch?v=dQw4w9WgXcQ" |
| 7 | sourceName | Java UTF | "youtube" |
| — | (source 固有データ) | source 依存 | この例では無し |
| 8 | position | int64 (ms) | 0（最後に外側エンコーダが書く） |

**version 3 の追加（Lavaplayer fork / Lavalink v4 が現行で出力）:** uri と sourceName の**間**に次の2つの nullable text が挿入される。
- artworkUrl: nullable text（version ≥ 3 のみ）
- isrc: nullable text（version ≥ 3 のみ）

確度: version 2 はバイト実証で ◎。version 3 のフィールド位置は Lavaplayer の PR #101 と version≥3 リーダ仕様から特定（○）。**実装時は Lavalink v4 が実際に発行する version 3 サンプルでラウンドトリップ検証すること**（フェーズ6-3 が担保）。

**実装メモ（Rust）:**
- Java の `DataOutput.writeUTF` 互換が必要（u16 長 + 修正 UTF-8）。`identifier` 等に通常の ASCII しか来なければ標準 UTF-8 で足りるが、サロゲート/NUL の修正 UTF-8 仕様差に注意。
- nullable text = `[1 byte: 存在フラグ][存在時のみ UTF]`。
- ヘッダの「サイズ書き戻し」はストリームに一旦書いてから長さを先頭に埋める2パス方式。Rust では `Vec<u8>` に本体を書いてから長さを前置すると楽。
- source 固有データ（YouTube/HTTP 等のマネージャが `encodeTrack` で書く追加バイト）は **sourceName の後・position の前**に入る。互換のため、自前 YouTube source も Lavaplayer YouTube マネージャと同じ追加フィールドを書く必要がある（多くは空、または `0`/フォーマット情報）。要実装時確認。

---

## 0-4 / 0-5. 音声送出アーキテクチャと Discord Voice プロトコル  ◎（公式ドキュメント）

**アーキテクチャ確定（TODO 0-4 の最優先検証項目）:**
Voice State オブジェクトの定義に「`token`, `endpoint`, `sessionId`, `channelId` は **Discord の音声サーバーに接続するための4値**」と明記され、`WebSocketClosedEvent` は「**Discord への音声 WebSocket が閉じた時**」に発火する。つまり **Lavalink 自身が Discord Voice Gateway と UDP に接続して Opus を送る**。ボットクライアント（discord.js 等）は Discord メインゲートウェイ経由で得た VOICE_STATE_UPDATE / VOICE_SERVER_UPDATE の情報を REST の `voice` フィールドで Lavalink に渡すだけ。→ 設計全体はこの前提で正しい。

**Discord Voice Gateway（WSS, opcode ベース, `?v=8` 推奨/必須）の接続手順:**
1. メインゲートウェイへ Opcode 4 `Voice State Update` 送信（これはボットクライアント側の責務）→ Discord が VOICE_STATE_UPDATE(session_id) と VOICE_SERVER_UPDATE(token, endpoint) を返す。
2. `wss://{endpoint}?v=8` へ接続。Opcode 0 `Identify`（`server_id, user_id, session_id, token, max_dave_protocol_version`）。
3. Opcode 8 `Hello`（heartbeat_interval）→ Opcode 3 `Heartbeat`（v8 は `seq_ack` 必須）/ Opcode 6 `Heartbeat ACK`。
4. Opcode 2 `Ready`（ssrc, ip, port, modes[]）。
5. **UDP IP Discovery**（0x1 タイプ要求, 長さ70, SSRC, アドレス64バイト, ポート2バイト）で外部 IP/port 取得。
6. Opcode 1 `Select Protocol`（`{protocol:"udp", data:{address, port, mode}}`）。
7. Opcode 4 `Session Description`（`mode`, `secret_key`[32], `dave_protocol_version`）→ ここから送出開始。
8. Opcode 5 `Speaking`（音声送出前に最低1回必須。SSRC を確定）。

**v8 の追加点（実装必須）:** サーバーメッセージにシーケンス番号、Heartbeat/Resume に `seq_ack`、Buffered Resume（取りこぼし再送）。v1〜v3 および無指定は 2024-11-18 で廃止済み。

**RTP パケット構造:** `0x80`(1) + `0x78`(1) + sequence(u16 BE) + timestamp(u32 BE) + ssrc(u32 BE) + 暗号化済み Opus。20ms 毎送出。途切れ時は**無音フレーム `0xF8,0xFF,0xFE` を5回**送ってから停止（Opus 補間対策）。

**voiceServer 更新（サーバー移動）:** 同一ギルドのチャンネル移動でも endpoint が同じことがあるが token は必ず変わり、旧セッションは再利用不可 → 再接続必須。

---

## 0-6. 必須暗号化方式と DAVE 必須化  ◎（公式ドキュメント＋公式告知で確認）

### トランスポート暗号（SFU との間。E2EE 有効時も併用）

| モード | 状態 |
|---|---|
| `aead_aes256_gcm_rtpsize` | 利用可・**優先（ハードウェア対応時）** |
| `aead_xchacha20_poly1305_rtpsize` | 利用可・**必須サポート** |
| `xsalsa20_poly1305_lite_rtpsize` | deprecated |
| その他 `xsalsa20_poly1305*` 系 | **2024-11-18 で全廃止**（接続拒否） |

→ `aead_xchacha20_poly1305_rtpsize` は必ず実装、`aead_aes256_gcm_rtpsize` を Ready の modes に含まれていれば優先選択。nonce は 32bit インクリメンタル値をペイロード末尾に付加（暗号化前/復号前にペイロードから剥がす）。Rust crate: `aes-gcm`, `chacha20poly1305`（元 TODO の選定で妥当）。

### 【計画外・最重要】DAVE プロトコル（音声 E2EE）の必須化

- Discord は 2024-09 から音声/映像の E2EE 移行を開始。**2026-03-01 以降、非ステージの音声（DM/GDM/ボイスチャンネル/Go Live）は E2EE 通話のみサポート**。
- **DAVE 非対応クライアント/アプリ/ボットは voice gateway に接続できず close code 4017 で拒否される**（公式サポート記事 2026-03-03 付で施行を確認）。
- DAVE は **MLS（RFC 9420, Messaging Layer Security）** でグループ鍵を交換し、**送信者ごとの ratchet 鍵で OPUS フレームを AES-128-GCM フレーム暗号**する（トランスポート暗号とは別レイヤ＝フレームレベル）。voice gateway は MLS の external sender として参加者の add/remove を proposal する。
- 必要 opcode 群（バイナリ WS メッセージ含む）: Identify の `max_dave_protocol_version`、Session Description の `dave_protocol_version`、Opcode 21〜31（Prepare Transition / Execute Transition / Prepare Epoch / External Sender / Key Package / Proposals / Commit Welcome / Announce Commit / Welcome / Invalid Commit Welcome）。
- フレームペイロード形式: `[E2EE OPUS暗号文][AES-GCM認証タグ8byte][ULEB128 nonce][ULEB128 unencrypted ranges(OPUSは空)][supplemental data size 1byte][magic marker 0xFAFA 2byte]`。

**downgrade（protocol version 0 = 非E2EE）の余地について（△・要注意）:**
仕様上は「DAVE 非対応クライアントが通話に入ると Opcode 21 で version 0 へダウングレード」する経路が今も記載されている。しかし公式の施行告知は「DAVE 対応クライアントが必須」「非対応は 4017 で拒否」と明言しており、ダウングレードが将来も常に許容される保証はない（「一部クライアントは今後ダウングレードを拒否」との情報あり）。
- **公式 Lavalink(Java) 側は最新リリースで DAVE 対応を取り込み済み**（Koe/JDA 向けに KyokoBot/libdave-jvm 等の Java MLS 実装が存在）。つまり「ボットだから DAVE 不要」という抜け道は**もはや無い前提で設計すべき**。
- 参考: discord.js の `@discordjs/voice` でも DAVE 周りの不具合（再接続ループ・無音）が報告されており、実装難度・不安定さが高い領域。

---

## 7. 元 TODO への修正提案

1. **フェーズ3 に「3-5. DAVE/MLS E2EE 実装」を新設（最重要）。**
   - MLS: Rust なら `openmls` を評価（external sender 対応の確認が必要）。あるいは `libdave`(C++) の FFI バインディング。
   - フレーム暗号: `aes-gcm`（AES-128-GCM, タグ8byte切詰め）, nonce/ratchet 管理, ULEB128 エンコード, `0xFAFA` マーカー。
   - voice gateway をバイナリ opcode（21〜31）対応に拡張。
   - **MVP 範囲をどうするか要判断**: (a) DAVE まで実装して実際に音が出る完全 MVP、(b) protocol version 0 で接続を試み「DAVE 必須チャンネルでは 4017 で繋がらない」ことを許容する暫定 MVP、(c) フェーズ2（プロトコル疑似応答・モックプレイヤー）までを当面のゴールにする。元 TODO の「まずモックプレイヤーで疎通」方針は (c) と整合的で、DAVE の重さを踏まえると現実的。
2. **依存 crate 追加:** `openmls`（or libdave FFI）, `aes-gcm`（既出だが DAVE フレーム用途も明記）。Voice Gateway は v8 必須なので `seq_ack`/Buffered Resume をプロトコル層に組み込む。
3. **0-1 の queue 系エンドポイントは削除**（v4 に存在せず、互換上も不要）。
4. **track codec の version 対応:** v2 と v3 の両方をデコードできるようにし、エンコードは v3（artworkUrl/isrc 込み）を既定にすると公式 v4 と揃う。source 固有追加バイトの互換は YouTube source 実装時に要確認。
5. **フェーズ順の現実的見直し:** DAVE の重さから、音が出る完全 MVP の難度が当初想定より大幅に上がった。フェーズ2（REST/WS 完全互換＋モックプレイヤーでの実クライアント疎通）を確実なマイルストーンとし、フェーズ3 の DAVE を独立した重タスクとして切り出すのが安全。

---

## 8. 残課題・実装時に再確認すべき点

- version 3 encoded track の正確なバイト列（公式 v4 発行サンプルでのラウンドトリップ）。
- YouTube/HTTP source の `encodeTrack` 追加フィールドの中身（sourceName と position の間）。
- `openmls` が Discord DAVE の MLS パラメータ（暗号スイート, external sender 拡張）と完全互換か。非互換なら libdave FFI へ。
- DAVE のダウングレード(v0)が実運用でどこまで許容されるか（チャンネル種別・相手クライアント依存）。
- Java の修正 UTF-8 と Rust UTF-8 の差異が出るエッジケース（NUL, サロゲートペア）。

---

## 情報源

- [Lavalink REST API ドキュメント](https://lavalink.dev/api/rest)
- [Lavalink WebSocket API ドキュメント](https://lavalink.dev/api/websocket)
- [Discord Voice Connections ドキュメント（DAVE/暗号化/RTP/IP Discovery 含む）](https://docs.discord.com/developers/topics/voice-connections)
- [Discord サポート: A/V E2EE Enforcement for Non-Stage Voice Calls（2026-03-03, close code 4017）](https://support.discord.com/hc/en-us/articles/38749827197591-A-V-E2EE-Enforcement-for-Non-Stage-Voice-Calls)
- [Discord Blog: Every Voice and Video Call on Discord Is Now End-to-End Encrypted](https://discord.com/blog/every-voice-and-video-call-on-discord-is-now-end-to-end-encrypted)
- [Discord Blog: Bringing DAVE to All Discord Platforms](https://discord.com/blog/bringing-dave-to-all-discord-platforms)
- [DAVE Protocol Whitepaper](https://daveprotocol.com/)
- [discord/libdave（公式 C++ 実装）](https://github.com/discord/libdave)
- [KyokoBot/libdave-jvm（JVM 向け MLS/DAVE 実装）](https://github.com/KyokoBot/libdave-jvm)
- [Lavaplayer fork (lavalink-devs)](https://github.com/lavalink-devs/lavaplayer)
- [Lavaplayer PR #101: Add artwork URL to AudioTrackInfo](https://github.com/sedmelluq/lavaplayer/pull/101/files)
- encoded track のバイト解析: 公式サンプルを本調査で base64 デコード・全145バイト検証
