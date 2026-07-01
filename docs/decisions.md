# 設計上の決定記録 (ADR)

## ADR-0001: DAVE（音声 E2EE）の MVP スコープ

- 日付: 2026-06-20
- ステータス: **採用**

### 文脈
Discord の音声 E2EE「DAVE プロトコル（MLS ベース）」が 2026-03-01 から非ステージ音声で
事実上必須化された。非対応クライアント/ボットは音声ゲートウェイに close code **4017** で
拒否される（フェーズ0 調査）。DAVE は MLS 鍵交換＋フレーム単位の AES-128-GCM 暗号という
最大の実装コスト・リスク要因。

### 決定
**MVP スコープを「v0 トランスポートのみ（E2EE なし）」とする。** DAVE/MLS は独立した後続
マイルストーン（3-5）に分離し、`lavalink-discord-voice` の `dave` feature 下に置く。

具体的にフェーズ3 は次の順で進める:
1. Voice Gateway v8 ハンドシェイク（Identify で `max_dave_protocol_version=0`）
2. UDP IP Discovery
3. RTP 送出（`aead_xchacha20_poly1305_rtpsize` 必須 / `aead_aes256_gcm_rtpsize` 優先、20ms 周期、無音フレーム）
4. voiceServer 更新時の再接続
5. （後続）DAVE/MLS E2EE

### 理由
1. DAVE 以外の音声スタック（Gateway/UDP/RTP/トランスポート暗号/Opus 送出）は DAVE を作るに
   せよ必ず必要で、v0 を先に作るのは無駄にならない。DAVE はその上に一層を足すだけ。
2. v0 で実際に音が出る到達可能なマイルストーンになり、同時に「どのチャンネルで 4017 が出るか」
   の実データが取れる。
3. DAVE(MLS) は最高リスク・最高コストなので、残りのパイプライン（フェーズ4）を end-to-end で
   検証してから着手する方が安全。

### 影響 / 割り切り
- **DAVE 必須チャンネルでは音が出ない（4017）**。全チャンネル対応には DAVE が必要。これは
  織り込み済みのトレードオフ。
- `discord-voice` は最初から DAVE を `dave` feature として組み込める構造で設計する。
