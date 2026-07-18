# 映像配信 (実験的) — 調査結果と導入計画

2026-07-12 調査。ボットから Discord のボイスチャンネルへ映像を送る機能の実現可能性と、
本プロジェクトへの段階的導入計画。

## 🚫 最終結論 (2026-07-12 実地検証後): ボットトークンでは不可能

実 VC で送出したが映像タイルは出ず、エラーも出なかった。原因が判明:

> **Discord はボットアカウントからの映像を仕様上ブロックしている。**
> 決定版のリバースエンジニアリング実装 Discord-RE/Discord-video-stream も
> 「Discord blocks video from bots which is why this library uses a selfbot library.
> You must use a user token.」と明記し、**ユーザートークン (selfbot) 専用**。

- 我々の実装 (V1 パケット化 / V2 op12・codecs / V2.5 映像 E2EE) は**プロトコル的には正しく、
  全単体テストが通る**。ただし Discord のサーバーが**アカウント種別で映像をゲート**しており、
  ボットトークンでは接続は受理されても映像が転送・描画されない (だからエラーも出ない)。
- さらにカメラ映像には主ゲートウェイ op4 の `self_video: true` 宣言も要る (songbird は送らない)
  が、これを足してもボットトークンである限り結果は変わらない。
- **ユーザートークンでの selfbot 化は Discord ToS 違反 (アカウント BAN リスク) のため行わない。**

→ **ボット用途では映像は断念。音声 (E2EE 対応済み) に注力する。**
  実装済みコードは `LAVALINK_VIDEO` フラグ下の不活性コードとして残す (音声経路に影響なし)。
  将来 Discord がボット映像を解禁した場合、または別途ユーザートークン運用を選ぶ場合に再利用可能。

---

## (以下は技術調査の記録) 結論

**プロトコル的には実装可能・実装済み。ただし上記のとおり Discord がボット映像をブロック。**

- Discord の音声接続 (Voice Gateway + UDP) は映像もサポートしており、クライアントは
  同じ UDP ソケット・同じトランスポート暗号で映像 RTP (H264/VP8 等, 90kHz クロック,
  別 SSRC / payload type) を送っている。
- **一般の Lavalink や songbird では不可能** (voice gateway の内側に手を入れられない) だが、
  本プロジェクトは voice gateway / UDP / RTP / 暗号を自前実装しているため追加できる立場にある。
- 前例: Node.js の `dank074/Discord-video-stream` 等がリバースエンジニアリングで実証済み。
  「カメラ映像」(VC 内のタイルに映る) はボイスゲートウェイの範囲で完結する。
  「Go Live (配信)」はメインゲートウェイの未文書化オペコード (STREAM_CREATE 等) が
  追加で必要で、ボットトークンでの動作は要検証。**まずはカメラ映像を対象にする。**

## リスク (先に了承しておくこと)

1. **非公式**: ボットの映像送信は API ドキュメントに存在しない。Discord 側の仕様変更で
   突然壊れる可能性があり、規約的にもグレー (BAN 報告は見当たらないが自己責任)。
2. **検証必須の推測が残る**: op 12 / SELECT_PROTOCOL codecs のペイロード形状は
   コミュニティ実装からの推定。実接続でエラーになったら公式クライアントのキャプチャと
   突き合わせて調整が要る。
3. **DAVE (E2EE) 通話は対象外**: E2EE 有効の通話では映像フレームにもコーデック対応の
   フレーム暗号が必要になり難度が跳ね上がる。当面はトランスポート暗号のみの通話に限定
   (dave feature 無効ビルド)。
4. **帯域/CPU**: 再エンコードは重いので、**H264 をパススルー**する設計にする
   (YouTube itag 18/22 は H264+AAC の muxed MP4。映像はそのまま RTP 化できる)。

## プロトコル概要 (カメラ映像)

```
1. Identify (op 0)                          — 既存どおり
2. Ready (op 2)                             — ssrc (音声) を得る
3. SELECT_PROTOCOL (op 1)                   — "codecs" に opus + H264 を宣言 ★追加
   payload_type: opus=120, H264=101, rtx=102 (クライアント慣例値)
4. Session Description (op 4)               — 既存どおり (secret_key)
5. Video (op 12)                            — audio_ssrc / video_ssrc / rtx_ssrc と
   streams[] (解像度・fps・ビットレート) を宣言 ★追加
6. 音声 RTP (既存) と並行して映像 RTP を送出 ★追加
   - 90kHz クロック。1 フレームの全パケットは同 timestamp、最後にマーカービット
   - H264 は RFC 6184 (単一 NAL / FU-A 分割、MTU ~1200)
   - 暗号は音声と同じ (aead: header AAD + ct + nonce4)
   - SPS/PPS はキーフレームごとに再送 (途中参加の視聴者対策)
```

## 段階的導入計画

### フェーズ V1: 基盤 (✅ 実装済み・単体テスト付き)

- `discord-voice/src/video.rs`
  - `H264Packetizer` — RFC 6184 パケット化 (単一 NAL / FU-A)、90kHz、マーカービット、暗号化
  - `split_annex_b` / `split_avcc` — NAL 分割ヘルパー (MP4 の AVCC / Annex-B 両対応)
- `gateway.rs` — `select_protocol_with_codecs()` / `video()` ペイロードビルダー (未結線)
- `crypto.rs` — `Cipher::decrypt` (テスト・将来の受信用)

既存の音声経路には一切影響しない (新規モジュールのみ)。

### フェーズ V2: 結線 (✅ 実装済み・実地検証待ち)

1. `VoiceConfig.video: bool` — true のとき:
   - SELECT_PROTOCOL を codecs 付きに切り替え (`select_protocol_with_codecs`)
   - Speaking 直後に op 12 を送信 (video_ssrc = ssrc+1, rtx_ssrc = ssrc+2)
   - `VoiceConnection::video_sender() -> Option<mpsc::Sender<VideoFrame>>`
   - `video_task`: 音声と同じ UDP ソケット (Arc 共有) へ、フレームの 90kHz
     タイムスタンプを壁時計にマップしてペーシング送出。一時停止対応。
     5 秒ごとに `video: send rate` ログ
2. サーバー結線: `LAVALINK_VIDEO=1` で voice 接続が映像対応になる (既定オフ)
3. **テスト手順** (playfile モード 3):
   ```sh
   # 1. テスト用 H264 (Annex-B) を作る
   ffmpeg -i input.mp4 -an -c:v libx264 -profile:v baseline -pix_fmt yuv420p \
          -g 30 -bsf:v h264_mp4toannexb out.h264
   # 2. サーバーを LAVALINK_WRITE_VOICE_ENV=1 で起動し、bot の /play で VC 接続
   #    (voice.env が書き出される)
   # 3. 映像送出 (VC 内の bot タイルに映像が出れば成功)
   #    DAVE 必須チャンネル (4017) なら --features dave が要る。
   cargo run -p lavalink-playfile --features dave -- out.h264
   ```
   失敗パターンの切り分け:
   - **WS が 4017 (DAVE required)** → チャンネルが E2EE 必須。`--features dave` で接続可。
     ただし下記「DAVE と映像」の制約に注意。
   - WS が 4002 (failed to decode payload) で切れる → op12/codecs の形状が違う
   - 接続は生きるが映像が出ない → payload_type / SSRC / パケット化、または DAVE 未暗号化を疑う

### ⚠️ DAVE (E2EE) と映像 — 重要な制約

2025 以降 Discord の多くの VC は **DAVE (E2EE) 必須** (4017)。DAVE 有効時は
**音声だけでなく映像フレームもコーデック対応の E2EE 暗号 (unencrypted ranges 付き)** を
かけないと受信クライアントで復号・描画されない。

- **音声の E2EE は実装済み** (dave feature、`FrameCryptor`、MLS)。音楽再生が E2EE 通話で
  動くのはこのため。
- **映像の E2EE は未実装**。DAVE の映像フレーム暗号は H264 の NAL ヘッダ等を平文範囲に
  残す「codec-aware」変換が必要で、opus 用の `encrypt_opus` (全暗号・range 空) は流用できない。

したがって現状の映像パスを**きれいに検証するには DAVE 非対応の VC が要る**が、
既定 ON の環境では入手が難しい。→ **次のマイルストーン V2.5 を映像 E2EE に充てる。**

### フェーズ V2.5: 映像の DAVE 対応 (✅ 実装済み・実地検証待ち)

libdave (discord/libdave, discord/dave-protocol) の仕様を精読して実装:

- `dave/video_frame.rs` (新規):
  - `encrypt_h264(key, nonce, frame)` — H264 Annex-B の codec-aware 部分暗号
    - 各 NAL の前に 4byte スタートコードを平文で正規化
    - VCL (slice=1/IDR=5): NAL ヘッダ(1) + スライスヘッダ先頭 (exp-golomb 3 値 =
      `BytesCoveringH264PPS` 相当) を平文、残りを暗号化
    - 非 VCL (SPS/PPS/SEI/AUD): NAL 全体を平文
    - **AAD = 全平文レンジの結合** (libdave encryptor.cpp 準拠)、tag 8byte 切詰め
    - supplemental: `[tag8][nonce uleb][unencrypted ranges (uleb offset/size pairs)][size 1][magic FAFA]`
  - `decrypt_h264` (round-trip テスト用)、`VideoFrameCryptor` (nonce/generation 前進)
  - 単体テスト: NAL 分割・exp-golomb・暗号/復号 round-trip・鍵誤り・非VCL全平文
- `lib.rs` video_task: DAVE 有効時は「AU 組立 → `VideoFrameCryptor::encrypt` → NAL 再分割
  → `H264Packetizer`」。音声 MLS と同じ sender secret / epoch から鍵導出 (reader_task で
  audio と同時に `VideoFrameCryptor` を確立)。

**既知の未実装 (要フォロー):**
- **エミュレーション防止スキャン**: 暗号文/supplemental 中に `00 00 01` が出ると受信側
  depacketizer が誤検出する。libdave は追加スキャンで回避。未実装のため稀に 1 フレームが乱れ得る。
- supplemental size は 1byte 前提。多スライスフレームでレンジ列が 255byte を超えると破綻
  (baseline 単一スライスなら問題なし)。

### 実地テスト手順 (DAVE チャンネル + 映像 E2EE)

playfile とサーバーが**同じ voice セッションを二重に使うと 4006** になるため、
サーバーは接続せず voice.env だけ書く設定にして playfile を唯一の voice クライアントにする。

```sh
# サーバー起動 (voice.env を書き、サーバー自身は接続しない):
#   LAVALINK_WRITE_VOICE_ENV=1 LAVALINK_SKIP_VOICE_CONNECT=1 で起動
# bot で /join (VC 参加。これで voice.env が最新になる)
# 直後に (session が失効する前に) playfile を DAVE 有効で実行:
cargo run -p lavalink-playfile --features dave -- out.h264
```
`out.h264` は baseline/単一スライス推奨:
```sh
ffmpeg -i input.mp4 -t 10 -an -c:v libx264 -profile:v baseline -x264-params slices=1 \
       -pix_fmt yuv420p -g 30 -bsf:v h264_mp4toannexb out.h264
```
ログの `dave: ... E2EE active (audio+video)` と `video: send rate` を確認。
描画されない場合は 1 フレームの hex を公式クライアントのキャプチャと突き合わせて範囲/AAD を調整。

### フェーズ V3: ソース (MP4 デマクサ)

1. workspace に `mp4` クレート (または自前の最小 moov/mdat パーサ) を追加
2. `source-youtube`: `stream_url_video()` — itag 18 (360p H264+AAC muxed) を優先取得
   (ANDROID クライアントの formats から `mimeType: video/mp4; codecs="avc1..."` を選ぶ)
3. `audio-pipeline` に `Mp4Demuxer`: 既存 SharedBuffer から
   - 音声トラック (AAC) → 既存デコードパイプラインへ
   - 映像トラック (H264, AVCC) → NAL + タイムスタンプで V2 の video_sender へ
   - A/V 同期: 音声側の送出時刻を基準に映像フレームをスケジュール

### フェーズ V4: サーバー/ボット結線

1. `application.yml` に `lavalink.server.video: false` (既定オフ)
2. loadtracks の identifier プレフィックス `ytvideo:` (または userData フラグ) で映像モード指定
3. ボットに `/playvideo` コマンド

## 見積り

| フェーズ | 規模感 | 検証手段 |
|---|---|---|
| V1 基盤 | 済 | cargo test (純ロジック) |
| V2 結線 | 中 (~300 行) | 実 VC で bot のタイルに映像が出るか |
| V3 MP4 | 大 (~500 行+依存) | ローカル MP4 → 実 VC |
| V4 結線 | 小 | E2E |

V2 以降は「実 VC に繋いで試す → ログ/挙動を見て直す」の反復が必須。
