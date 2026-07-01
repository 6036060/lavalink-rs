# DAVE op28 キャプチャ & 差分手順

目的: 公式 Discord クライアントが送る **op28 (MLS commit + welcome)** の生バイトを採取し、
本実装のボットが送る op28 とバイト単位で diff して、gateway が我々のコミットを拒否する
原因（最初に食い違うフィールド）を特定する。

## 全体像

1. 公式クライアントの op28 を採取する（方法 A or B）。
2. 本実装のボットの op28 を採取する（既にログ実装済み）。
3. 両者を MLSMessage / Commit / Welcome の構造に沿って比較し、最初の相違点を見つける。

重要な前提:

- **Discord デスクトップアプリ(Electron)は証明書ピンニングや独自実装でキャプチャが難しい。
  必ず「Discord web 版（ブラウザ）」を使う。** ブラウザは独自 CA を信頼させれば MITM 可能。
- **ボイスチャンネルに 2 人以上（DAVE 対応）**を入れること。1 人だとグループが形成されず
  commit が発生しない。例: 「自分のブラウザ web クライアント」＋「本実装のボット」を
  同じ VC に入れる。するとブラウザ側がボットを add する commit(op28) を送るので、それを
  採取できる（＝本実装が送るべき op28 とほぼ同型）。あるいはブラウザ 2 アカウントでも可。

---

## 方法 A: ブラウザの WebSocket をフックして console に出す（手早い）

ブラウザ（Chrome 推奨）で Discord web を開き、F12 で DevTools → Console に以下を貼って
実行する。**その後に**ボイスチャンネルに参加する（フックは VC 接続前に入れる必要がある）。

```javascript
(() => {
  const hex = b => [...new Uint8Array(b)].map(x => x.toString(16).padStart(2,'0')).join('');
  const S = WebSocket.prototype.send;
  WebSocket.prototype.send = function (d) {
    try {
      if (d instanceof ArrayBuffer && d.byteLength > 0) {
        const op = new Uint8Array(d)[0];           // client->server: [op u8][...]
        if (op >= 25 && op <= 31)
          console.log('%cOUT op' + op + ' len=' + d.byteLength, 'color:lime', hex(d));
      }
    } catch (e) {}
    return S.apply(this, arguments);
  };
  const A = WebSocket.prototype.addEventListener;
  WebSocket.prototype.addEventListener = function (t, f, ...r) {
    if (t === 'message' && typeof f === 'function') {
      const g = function (ev) {
        try {
          if (ev.data instanceof ArrayBuffer && ev.data.byteLength > 2) {
            const op = new Uint8Array(ev.data)[2]; // server->client: [seq u16][op u8][...]
            if (op >= 25 && op <= 31)
              console.log('%cIN  op' + op + ' len=' + ev.data.byteLength, 'color:cyan', hex(ev.data));
          }
        } catch (e) {}
        return f.apply(this, arguments);
      };
      return A.call(this, t, g, ...r);
    }
    return A.call(this, t, f, ...r);
  };
  console.log('DAVE WS hook installed. Now join a voice channel with another DAVE peer.');
})();
```

- `OUT op28 ...` の行に出る hex 文字列が公式クライアントの op28（採取対象）。
- うまく出ない場合、Discord は音声接続を **Web Worker** 内で張ることがあり、メインスレッドの
  フックが効かない可能性がある。その場合は方法 B（mitmproxy）を使う。

---

## 方法 B: mitmproxy で WSS を傍受（確実・ネットワーク層）

Worker でも関係なく全フレームを採れる。手順:

1. mitmproxy を入れる（Windows）:
   ```
   pip install mitmproxy
   ```
2. ダンプ用アドオンを用意（`dump_dave.py`）:
   ```python
   from mitmproxy import ctx

   def websocket_message(flow):
       m = flow.websocket.messages[-1]
       if m.is_text or len(m.content) == 0:
           return
       d = m.content
       if m.from_client:                 # ブラウザ -> gateway
           op = d[0]                      # [op u8][...]
           if 25 <= op <= 31:
               ctx.log.info(f"OUT op{op} len={len(d)} {d.hex()}")
       else:                             # gateway -> ブラウザ
           if len(d) > 2:
               op = d[2]                  # [seq u16][op u8][...]
               if 25 <= op <= 31:
                   ctx.log.info(f"IN  op{op} len={len(d)} {d.hex()}")
   ```
3. 起動:
   ```
   mitmdump -s dump_dave.py
   ```
   既定で `127.0.0.1:8080` で待ち受ける。
4. ブラウザのプロキシを `127.0.0.1:8080` に設定（Chrome なら `--proxy-server=127.0.0.1:8080`
   付きで起動、または OS のプロキシ設定）。
5. mitmproxy の CA 証明書を信頼させる: プロキシ経由のブラウザで `http://mitm.it` を開き、
   Windows 用証明書を入れて「信頼されたルート証明機関」にインポート。
6. そのブラウザで **Discord web** を開く → VC に参加（もう 1 人 DAVE ピアを入れる）。
7. `mitmdump` のコンソールに `OUT op28 ... <hex>` が流れる。それが採取対象。

注意: HSTS は独自 CA を信頼させれば問題なし（ピンニングしているのはデスクトップだけ）。
それでも繋がらない場合は別ブラウザ/プロファイルで試す。

---

## 本実装（ボット）側の op28 を採る

ボットは既に op28 を全 hex でログ出力するよう実装済み:

```
set RUST_LOG=info,lavalink_discord_voice=debug
cargo run -p lavalink-server --features dave --bin lavalink-rs
```

再生して DAVE ハンドシェイクを走らせると、ログに次が出る:

```
dave -> op28 FULL HEX (for diff)  hex=1c....
```

この `hex=` の文字列が本実装の op28。

---

## 差分の取り方と見るべき箇所

両方の op28 hex を保存し、構造に沿って比較する。op28 の中身（先頭の `1c` = opcode 28 は
ログ上含まれる／含まれないが揃っているか注意。ボット側ログは opcode を含む）:

```
op28 = [opcode=0x1c] [MLSMessage commit] [Welcome]
```

`MLSMessage commit` の TLS 構造（先頭から）:

```
version        u16   = 00 01          (mls10)
wire_format    u16   = 00 01          (public_message)   ← 両方 0001 か
group_id<V>          = 08 <8 bytes>   (チャンネル snowflake)  ← 一致するはず
epoch          u64   = 00..00         (= 0)
sender               = 01 <leaf_index u32>  (member)
authenticated_data<V>= 00
content_type   u8    = 03             (commit)
Commit:
  proposals<V>       = [ ProposalOrRef ... ]   ← ★最重要。ref(0x02)+ProposalRef か、
                                                  value(0x01)+Proposal か。本実装は ref。
  path (optional)    = UpdatePath の有無        ← ★公式に path があるか / 無いか
auth (FramedContentAuthData):
  signature<V>
  confirmation_tag<V>
membership_tag<V>    (member の PublicMessage のみ)
```

比較で特に注目:

1. **proposals の表現**: 公式が `ProposalOrRef` を `reference(2)` で送っているか、その
   ProposalRef のバイトが本実装と一致するか。ここが食い違えば「未知の proposal 参照」で
   拒否される。ProposalRef = RefHash("MLS 1.0 Proposal Reference", AuthenticatedContent)。
2. **path の有無**: 公式の commit が UpdatePath を含むか含まないか。本実装(openmls)は
   含む可能性が高い。公式が含まない（add のみで path 省略）なら、本実装も省略させる必要が
   あるかもしれない（openmls の commit オプションで path を抑制 / 強制）。
3. **wire_format / framing**: 両方 PublicMessage(0001) か。membership_tag の有無。
4. **welcome 部**: 先頭が ciphersuite(00 02) の raw Welcome になっているか（MLSMessage
   ラップ 00 01 00 03 でないか）。inline ratchet tree を含むか。

最初に食い違うフィールドが原因。多くの場合 (1) ProposalRef か (2) path のどちらか。

---

## ヒント: 差分を機械的に取る

両 hex を 1 行ずつファイルに保存し、簡単な Python で TLS フィールドを切り出して並べると
比較しやすい。必要なら、採取した 2 つの hex を渡してもらえれば、本実装側でフィールド単位に
パースして「どこが最初に違うか」を割り出す手伝いができる。
