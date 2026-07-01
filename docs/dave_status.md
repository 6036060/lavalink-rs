# DAVE (音声 E2EE) 実装ステータス

最終更新: 2026-06-22

このドキュメントは、Discord DAVE プロトコル（MLS ベースの音声 E2EE）を Rust で
実装した際の到達点・確定した知見・残課題・次の手順を記録する。実機（公式仕様
`discord/dave-protocol` の `protocol.md`）と照合しながら反復した結果である。

## 結論（要約）

DAVE のハンドシェイクは**仕様準拠でほぼ完全に実装できている**が、最後の 1 点
——**Voice Gateway が我々の MLS コミット(op28)を無言で拒否する**——が解消できていない。
gateway はエラー(op31 等)を返さず沈黙するため、原因を観測できない。検証可能な
要素はすべて仕様通りであることを確認済みで、残るのは libdave (Discord 公式の C++/
mlspp 実装) とのバイト単位の差分照合という別レベルの作業。

DAVE 必須チャンネルでは E2EE が確立しないと音が出ない（パススルー平文も受信側に
破棄される）ことも実機で確認した。

## 動作している部分

- Voice Gateway v8 ハンドシェイク、`max_dave_protocol_version=1` のアドバタイズ。
  これにより DAVE チャンネルで close code 4017 を回避し、`dave_version=1` で接続成立。
- DAVE オペコードの送受信フレーミング（バイナリ `[seq u16][op u8][payload]` 受信 /
  `[op u8][payload]` 送信、JSON テキスト op21/22/24）。
- **op26 (key package) 送信** … 接続直後に自分の KeyPackage を送出。
- **op25 (external sender) 受信・登録** … `ExternalSender` をグループ拡張に組込み。
- **op27 (proposals) 受信・処理** … external sender の Add プロポーザルを処理し、
  参照によるコミットのため pending に積む。
- **op28 (commit + welcome) 生成・送信** … 有効な MLS コミット（参照によるコミット、
  PublicMessage、Welcome 付き）を生成して送出。`epoch=1`、`has_welcome=true`、
  `n_staged=1` まで毎回到達する。
- フレーム E2EE 基盤（AES-128-GCM 8 byte tag、送信者鍵ラチェット、ULEB128、
  マジックマーカー 0xFAFA、nonce/generation 管理）と exporter による送信者鍵導出。
  単体テストで暗号の正しさは検証済み。

## DAVE v1 の MLS パラメータ（仕様より、実装で遵守）

- ciphersuite = MLS ciphersuite 2 (DHKEMP256_AES128GCM_SHA256_P256, 署名 P256)。
- グループ拡張は **External Senders ただ 1 つ**のみ。
- leaf node 拡張なし。credential は basic のみ。
- basic credential の identity = ユーザーの 64bit snowflake をビッグエンディアン 8 byte。
- group_id = ボイスチャンネルの 8 byte snowflake（プロポーザルから採用する。
  ランダム group_id だと openmls が `WrongGroupId` で拒否する）。
- handshake は平文 **PublicMessage** で送る（`PURE_PLAINTEXT_WIRE_FORMAT_POLICY`）。
- op28 の Welcome は **raw Welcome**（MLSMessage ラップではない）。openmls は welcome を
  MLSMessage ラップ(`[0001][0003][Welcome]`)で返すので、先頭 4 byte を剥がして送る。

## openmls 0.6 の必須パッチ（重要）

openmls 0.6 は **external sender からの Add プロポーザルを未実装**で、`Remove` 以外を
`UnsupportedProposalType` で問答無用に弾く（`src/group/mls_group/processing.rs` の
`Sender::External` 分岐、`// TODO #151/#106`）。DAVE は external Add が必須なので、
openmls をベンダリングして 1 行パッチする:

```rust
// vendor/openmls/src/group/mls_group/processing.rs（および public_group/process.rs）
FramedContentBody::Proposal(Proposal::Remove(_) | Proposal::Add(_)) => {
    // 既存の Remove 用処理（from_authenticated_content_by_ref で正しい ProposalRef を計算）
}
```

ルート `Cargo.toml` に `[patch.crates-io] openmls = { path = "vendor/openmls" }` を追加。
ビルドログに `Compiling openmls v0.6.0 (...\vendor\openmls)` が出れば有効。
（openmls 0.7 の #902「External Add」は NewMember sender 限定で、external-sender の
Add には対応しないため、アップグレードでは解決しない。）

leaf capabilities には `proposals=[Add, Remove]` を明示（openmls の ValSem113 対策）。

## 残課題（ブロッカー）

**op28 送信後、Voice Gateway から op29 (announce_commit) が返らない**。仕様
"Commit Ordering" の「無効と判断したコミットは broadcast しない」に該当し、gateway が
我々のコミットを無効判定していると推測される。op31 等のフィードバックは無い。

実機ログで確認済みの事実（コミットは構造的に正しい）:

- `commit_head = [0,1,0,1,...]` … MLSMessage version=mls10, wire_format=1=**PublicMessage**。
- `welcome_head = [0,1,0,3,0,2]` … MLSMessage ラップの Welcome（4 byte 剥がしで raw 化、正)。
- 受信プロポーザルは標準的な external Add（proposal_type=0x0001, P256 KeyPackage,
  basic credential = 相手の user_id）。
- `n_staged=1`、`has_welcome=true`、`epoch=1` まで到達。
- op26 の有無で結果は変わらない（どちらも沈黙）。
- E2EE をやめて平文（パススルー）で送っても音は出ない（受信側が E2EE を要求）。

検証しきれていない＝原因候補:

1. ProposalRef の計算が libdave/mlspp と微妙に異なり、gateway が「未知のプロポーザル
   参照」とみなしている可能性（openmls の `from_authenticated_content_by_ref` の出力を
   gateway 期待値と突き合わせる必要）。
2. コミットの構造（UpdatePath の有無、framing の細部）が libdave 期待と異なる可能性。
3. 「エポック内の全プロポーザルを参照」ルールの解釈差。

## 次の手順（再開する場合）

1. **公式クライアントのキャプチャ差分**（最有力・決定的）: 公式 Discord クライアント 2 台で
   DAVE 通話を行い、Voice WebSocket(WSS) を傍受して op28 (commit+welcome) の生バイトを採取。
   本実装の op28 とバイト単位で diff し、最初に食い違う箇所を特定する。これが原因を直接示す。
2. **libdave のソース精読**: `discord/libdave`（C++ / mlspp）のコミット生成・ProposalRef
   計算・welcome 生成を読み、openmls の出力との差異を埋める。
3. それでも詰まる場合は MLS 実装を mlspp バインディング等へ切替える選択肢を検討。

## ビルド / 実行

```
# openmls をベンダリング・パッチした上で
set RUST_LOG=info,lavalink_discord_voice=debug
cargo run -p lavalink-server --features dave --bin lavalink-rs
```

DAVE 非対応ビルド（v0 トランスポートのみ）は `--features dave` を外す。DAVE 必須
チャンネルでは 4017 になるが、非必須チャンネルでは音が出る。

## 関連コード

- `crates/discord-voice/src/dave/session.rs` … DAVE 状態機械（opcode ルーティング）。
- `crates/discord-voice/src/dave/mls.rs` … openmls バックエンド（グループ作成・proposal
  処理・コミット・welcome・exporter）。
- `crates/discord-voice/src/dave/{frame,gcm,cryptor,ratchet,uleb128,opcodes}.rs` …
  フレーム E2EE 基盤。
- `crates/discord-voice/src/lib.rs` … VoiceConnection、reader_task（op26 送出、
  DAVE ディスパッチ、op22 で E2EE 有効化）、audio_task（E2EE 未確立時は平文送出）。
