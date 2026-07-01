# 公式 Lavalink との互換性検証（フェーズ6-2 / 6-3）

本実装のレスポンスが公式 Lavalink v4 と一致することを検証する仕組み。

## テストの種類

`crates/server/tests/conformance.rs` に集約。`cargo test -p lavalink-server` で実行。

1. **decodetrack 相互運用（6-3, 値レベル）**
   公式の encoded track 文字列（docs の Rick Astley サンプル）を `POST /v4/decodetracks` に投げ、
   公式 docs の Track JSON（`tests/golden/decodetracks_sample.json`）とフィールド単位で一致を確認。
   → encoded バイナリ形式の互換性を実証。

2. **エラー形状（共通）**
   未知セッションへのアクセスが公式と同じ `{timestamp,status,error,message,path}` 形状を返すか。

3. **REST スキーマ（6-2, 形状レベル）**
   `loadtracks`(track/search) と `update_player` のレスポンスが v4 のキー構成（camelCase 名）を
   満たすか。`loadtracks` は現状モック値のため値ではなくスキーマを検証。

4. **WebSocket ワイヤ形状（2-6 の裏取り）**
   `ready`/`playerUpdate`/`stats` と各 `event`（TrackStart/TrackEnd/WebSocketClosed）の serde 出力を
   公式 docs の JSON と直接比較。入れ子内部タグ（`op` → `type`）と
   各 `rename_all`（`guildId`/`byRemote`/`playingPlayers`/`systemLoad` 等）の正しさを担保。

`json_diff()` は JSON を再帰比較し、`ignore` で指定した JSON パスの差分を無視する。

## 既知の保留差分（実装が進めば解消）

| パス | 差分 | 理由 / 解消フェーズ |
|---|---|---|
| `info.artworkUrl`（decodetrack） | 公式=ytimg URL / 本実装=null | artworkUrl は YouTube source manager が識別子から再構築するフィールド。汎用コーデックは v2 encoded から復元できない。**フェーズ5** で source 実装時に解消 |
| `loadtracks` の実値 | 公式=実 YouTube データ / 本実装=モック | YouTube 抽出が **フェーズ5** 未実装のため。現状はスキーマのみ検証 |
| `info.position`（v2 由来） | source 固有データ未対応 | encode は v3 固定・source 固有バイト未書き込み。**フェーズ5** で各 source 実装時に対応 |

## 実 Lavalink との値レベル差分（フェーズ5 以降）

`scripts/capture_golden.sh` で公式 Lavalink を Docker 起動し、
`crates/server/tests/golden/official/` に実レスポンスを保存する。
YouTube の実値 golden が必要な場合は youtube-source プラグインを導入した状態で実行すること。

```sh
bash scripts/capture_golden.sh
```

取得後、本実装を同一 identifier で叩いた出力と `json_diff()` で突き合わせるテストを追加すれば、
実データでの値レベル一致まで検証できる（フェーズ5 完了時の受け入れ条件に組み込む想定）。
