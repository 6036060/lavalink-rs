//! Discord 音声接続層（フェーズ3 / ADR-0001: v0 トランスポートのみ、DAVE は後続）。
//!
//! 確定事項(フェーズ0): **本サーバー自身**が Discord Voice Gateway(WSS, v8) と UDP に接続し
//! Opus を送出する。クライアントからは REST の `voice`(token/endpoint/sessionId) を受け取るだけ。
//!
//! 流れ: Identify(max_dave=0) → Hello/Ready → UDP IP Discovery → Select Protocol →
//!       Session Description(secret_key) → Speaking → 20ms 周期で RTP 送出。

#![forbid(unsafe_code)]

pub mod crypto;
pub mod error;
pub mod rtp;

mod gateway;
mod udp;

#[cfg(feature = "dave")]
pub mod dave;

use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

pub use crypto::{select_mode, Cipher, Mode, PREFERRED_MODE, REQUIRED_MODE};
pub use error::VoiceError;
pub use rtp::SILENCE_FRAME;

/// 本クライアントが対応する DAVE プロトコル最大バージョン（0 = 非対応, ADR-0001）。
pub const MAX_DAVE_PROTOCOL_VERSION: u8 = if cfg!(feature = "dave") { 1 } else { 0 };

type Ws = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;
type WsSink = SplitSink<Ws, Message>;
type WsStream = SplitStream<Ws>;

/// 音声送出タスクと共有する DAVE フレーム暗号器ハンドル。
#[cfg(feature = "dave")]
type DaveCryptor = std::sync::Arc<std::sync::Mutex<Option<crate::dave::cryptor::FrameCryptor>>>;
#[cfg(not(feature = "dave"))]
type DaveCryptor = ();

/// Discord から（クライアント経由で）受け取る音声接続情報。
#[derive(Debug, Clone)]
pub struct VoiceConfig {
    pub guild_id: u64,
    pub user_id: u64,
    /// VOICE_STATE_UPDATE の session_id。
    pub session_id: String,
    /// VOICE_SERVER_UPDATE の token。
    pub token: String,
    /// VOICE_SERVER_UPDATE の endpoint（`host:port`, スキーム無し）。
    pub endpoint: String,
}

/// 確立済みの音声接続。Opus フレームを送ると 20ms 周期で RTP 送出される。
pub struct VoiceConnection {
    audio_tx: mpsc::Sender<Vec<u8>>,
    /// 一時停止フラグ。true の間 audio_task はキューを消費せず無音にする。
    paused: Arc<AtomicBool>,
    tasks: Vec<JoinHandle<()>>,
}

impl VoiceConnection {
    /// ハンドシェイクを行い、送出タスク群を起動する。
    pub async fn connect(cfg: VoiceConfig) -> Result<Self, VoiceError> {
        // rustls 0.23 はプロセス既定の CryptoProvider を要求する。未設定なら ring を導入
        // （既に設定済みなら Err が返るので無視）。
        let _ = rustls::crypto::ring::default_provider().install_default();

        let url = format!("wss://{}/?v=8", cfg.endpoint);
        let (ws, _resp) = tokio_tungstenite::connect_async(url.as_str()).await?;
        let (mut sink, mut stream) = ws.split();

        // --- Identify ---
        send_json(
            &mut sink,
            &gateway::identify(
                cfg.guild_id,
                cfg.user_id,
                &cfg.session_id,
                &cfg.token,
                MAX_DAVE_PROTOCOL_VERSION,
            ),
        )
        .await?;

        // --- Hello + Ready を収集 ---
        let mut interval_ms: Option<f64> = None;
        let mut ready: Option<(u32, String, u16, Vec<String>)> = None;
        while interval_ms.is_none() || ready.is_none() {
            let v = next_json(&mut stream).await?;
            match v.get("op").and_then(Value::as_u64) {
                Some(gateway::op::HELLO) => {
                    interval_ms = v["d"]["heartbeat_interval"].as_f64();
                }
                Some(gateway::op::READY) => {
                    let d = &v["d"];
                    let ssrc =
                        d["ssrc"].as_u64().ok_or(VoiceError::Protocol("ready.ssrc"))? as u32;
                    let ip =
                        d["ip"].as_str().ok_or(VoiceError::Protocol("ready.ip"))?.to_string();
                    let port =
                        d["port"].as_u64().ok_or(VoiceError::Protocol("ready.port"))? as u16;
                    let modes = d["modes"]
                        .as_array()
                        .map(|a| a.iter().filter_map(|m| m.as_str().map(String::from)).collect())
                        .unwrap_or_default();
                    ready = Some((ssrc, ip, port, modes));
                }
                _ => {}
            }
        }
        let (ssrc, ip, port, modes) = ready.unwrap();
        let interval_ms = interval_ms.unwrap_or(41250.0);

        // --- UDP + IP Discovery ---
        let socket = UdpSocket::bind("0.0.0.0:0").await?;
        socket.connect((ip.as_str(), port)).await?;
        let (ext_ip, ext_port) = udp::ip_discovery(&socket, ssrc).await?;

        // --- モード選択 + Select Protocol ---
        let (mode_str, mode) =
            select_mode(&modes).ok_or(VoiceError::NoSupportedMode(REQUIRED_MODE))?;
        send_json(&mut sink, &gateway::select_protocol(&ext_ip, ext_port, mode_str)).await?;

        // --- Session Description（secret_key 取得）---
        let (secret_key, dave_version) = loop {
            let v = next_json(&mut stream).await?;
            if v.get("op").and_then(Value::as_u64) == Some(gateway::op::SESSION_DESCRIPTION) {
                let arr = v["d"]["secret_key"]
                    .as_array()
                    .ok_or(VoiceError::Protocol("session_description.secret_key"))?;
                let mut key = [0u8; 32];
                for (i, b) in arr.iter().take(32).enumerate() {
                    key[i] = b.as_u64().unwrap_or(0) as u8;
                }
                let dave_version = v["d"]["dave_protocol_version"].as_u64().unwrap_or(0) as u16;
                break (key, dave_version);
            }
        };

        // --- Speaking ---
        send_json(&mut sink, &gateway::speaking(ssrc)).await?;
        tracing::info!(guild = cfg.guild_id, mode = mode_str, ssrc, dave_version, "voice connection established");

        // --- タスク起動 ---
        let cipher = Cipher::new(mode, &secret_key);
        let (audio_tx, audio_rx) = mpsc::channel::<Vec<u8>>(64);
        let (gw_tx, gw_rx) = mpsc::unbounded_channel::<Message>();
        let last_seq = Arc::new(AtomicI64::new(-1));

        let writer = tokio::spawn(writer_task(sink, gw_rx));
        let hb = tokio::spawn(heartbeat_task(gw_tx.clone(), interval_ms, last_seq.clone()));
        #[cfg(feature = "dave")]
        let dave_cryptor: DaveCryptor = std::sync::Arc::new(std::sync::Mutex::new(None));
        #[cfg(not(feature = "dave"))]
        let dave_cryptor: DaveCryptor = ();
        let reader = tokio::spawn(reader_task(
            stream,
            last_seq,
            gw_tx,
            cfg.user_id,
            dave_version,
            dave_cryptor.clone(),
        ));
        let paused = Arc::new(AtomicBool::new(false));
        let audio =
            tokio::spawn(audio_task(socket, cipher, ssrc, audio_rx, dave_cryptor, paused.clone()));

        Ok(Self { audio_tx, paused, tasks: vec![writer, hb, reader, audio] })
    }

    /// Opus フレーム（20ms 分）を送出キューに入れる。満杯ならバックプレッシャで待つ。
    pub async fn send_opus_frame(&self, frame: Vec<u8>) -> Result<(), VoiceError> {
        self.audio_tx.send(frame).await.map_err(|_| VoiceError::ClosedEarly)
    }

    /// 送出キューの Sender を複製して返す（再生タスクへ渡す用）。
    pub fn audio_sender(&self) -> mpsc::Sender<Vec<u8>> {
        self.audio_tx.clone()
    }

    /// 一時停止/再開。停止中は RTP 送出が無音になり、送出キューのフレームは温存される
    /// （デコードはバックプレッシャで自然に停止し、再開時にロスなく続行する）。
    pub fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::Relaxed);
    }

    /// 現在一時停止中か。
    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    /// 明示切断（タスクを停止）。`Drop` でも同等の処理が走る。
    pub fn disconnect(self) {}
}

impl Drop for VoiceConnection {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

// ----------------------------- helpers -----------------------------

async fn send_json(sink: &mut WsSink, v: &Value) -> Result<(), VoiceError> {
    let txt = serde_json::to_string(v)?;
    sink.send(Message::Text(txt.into())).await?;
    Ok(())
}

fn close_code_hint(code: u16) -> &'static str {
    match code {
        4001 => "unknown opcode",
        4002 => "failed to decode payload",
        4003 => "not authenticated",
        4004 => "authentication failed (token/session が無効か期限切れ)",
        4005 => "already authenticated",
        4006 => "session no longer valid (取り直しが必要)",
        4009 => "session timeout",
        4011 => "server not found",
        4012 => "unknown protocol",
        4014 => "disconnected (channel から削除/権限/サーバー移動)",
        4015 => "voice server crashed (再接続)",
        4016 => "unknown encryption mode",
        4017 => "DAVE 必須チャンネル (v0 非対応のため接続不可)",
        _ => "unknown close code",
    }
}

async fn next_json(stream: &mut WsStream) -> Result<Value, VoiceError> {
    loop {
        match stream.next().await {
            Some(Ok(Message::Text(t))) => return Ok(serde_json::from_str(t.as_str())?),
            Some(Ok(Message::Close(frame))) => {
                let (code, reason) = match frame {
                    Some(f) => (u16::from(f.code), f.reason.to_string()),
                    None => (0, String::new()),
                };
                return Err(VoiceError::GatewayClosed { code, reason, hint: close_code_hint(code) });
            }
            None => return Err(VoiceError::ClosedEarly),
            Some(Ok(_)) => continue, // Binary(DAVE)/Ping/Pong/Frame は無視
            Some(Err(e)) => return Err(e.into()),
        }
    }
}

async fn writer_task(mut sink: WsSink, mut rx: mpsc::UnboundedReceiver<Message>) {
    while let Some(m) = rx.recv().await {
        if sink.send(m).await.is_err() {
            break;
        }
    }
}

async fn heartbeat_task(
    tx: mpsc::UnboundedSender<Message>,
    interval_ms: f64,
    last_seq: Arc<AtomicI64>,
) {
    let mut ticker = tokio::time::interval(Duration::from_millis(interval_ms.max(1.0) as u64));
    loop {
        ticker.tick().await;
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let payload = gateway::heartbeat(nonce, last_seq.load(Ordering::Relaxed));
        let txt = match serde_json::to_string(&payload) {
            Ok(t) => t,
            Err(_) => continue,
        };
        if tx.send(Message::Text(txt.into())).is_err() {
            break;
        }
    }
}

async fn reader_task(
    mut stream: WsStream,
    last_seq: Arc<AtomicI64>,
    gw_tx: mpsc::UnboundedSender<Message>,
    user_id: u64,
    dave_version: u16,
    dave_cryptor: DaveCryptor,
) {
    #[cfg(not(feature = "dave"))]
    let _ = (&gw_tx, user_id, dave_version, &dave_cryptor);

    #[cfg(feature = "dave-mls")]
    let mut dave = crate::dave::session::DaveSession::new(
        user_id,
        dave_version,
        crate::dave::mls::OpenMlsBackend::new(user_id),
    );
    #[cfg(all(feature = "dave", not(feature = "dave-mls")))]
    let mut dave =
        crate::dave::session::DaveSession::new(user_id, dave_version, crate::dave::session::NoopMls);

    // 仕様(Key Packages): dave_version >= 1 なら接続直後に op26(自分の KeyPackage)を送る。
    #[cfg(feature = "dave")]
    if dave_version >= 1 {
        if let Some(kp) = dave.key_package() {
            let mut m = Vec::with_capacity(1 + kp.len());
            m.push(26u8);
            m.extend_from_slice(&kp);
            tracing::info!(
                hex = %m.iter().map(|x| format!("{:02x}", x)).collect::<String>(),
                "dave -> op26 FULL HEX (for diff)"
            );
            tracing::info!(len = m.len(), "dave -> binary opcode=26 (key package)");
            let _ = gw_tx.send(Message::Binary(m.into()));
        }
    }

    while let Some(msg) = stream.next().await {
        match msg {
            Ok(Message::Text(t)) => {
                if let Ok(v) = serde_json::from_str::<Value>(t.as_str()) {
                    if let Some(seq) = v.get("seq").and_then(Value::as_i64) {
                        last_seq.store(seq, Ordering::Relaxed);
                    }
                    let op = v.get("op").and_then(Value::as_u64).unwrap_or(0);
                    if op == gateway::op::CLIENT_DISCONNECT {
                        tracing::debug!("voice: client disconnect");
                    }
                    // DAVE JSON opcode: 21 prepare_transition / 22 execute / 24 prepare_epoch
                    #[cfg(feature = "dave")]
                    if matches!(op, 21 | 22 | 24) {
                        tracing::info!(op, payload = %t.as_str(), "dave json opcode");
                        let outs = dave.handle_json(op, v.get("d").unwrap_or(&Value::Null));
                        dispatch_dave(&gw_tx, &dave, &dave_cryptor, outs);
                        // op22(execute_transition) で初めて E2EE を有効化する（仕様）。
                        if op == 22 {
                            if let Ok(mut guard) = dave_cryptor.lock() {
                                if let Some(secret) = dave.sender_secret() {
                                    *guard = Some(crate::dave::cryptor::FrameCryptor::with_epoch(
                                        secret,
                                        dave.epoch(),
                                    ));
                                    tracing::info!(epoch = dave.epoch(), "dave: transition executed -> E2EE active");
                                }
                            }
                        }
                    }
                }
            }
            #[cfg(feature = "dave")]
            Ok(Message::Binary(b)) => {
                if let Some((seq, opcode, payload)) = crate::dave::opcodes::parse_server_binary(&b) {
                    last_seq.store(seq as i64, Ordering::Relaxed);
                    tracing::info!(opcode, len = payload.len(), "dave binary opcode");
                    if opcode == 29 || opcode == 30 {
                        tracing::info!(
                            head = %payload.iter().take(4).map(|x| format!("{:02x}", x)).collect::<String>(),
                            "dave: op29/30 head (transition_id 先頭2バイト)"
                        );
                    }
                    let outs = dave.handle_binary(opcode, payload);
                    dispatch_dave(&gw_tx, &dave, &dave_cryptor, outs);
                    // op29/op30 のうち、初期確立(transition_id=0, op22 が来ない)のみ即時 E2EE 有効化。
                    // tid>0 の再キーは op22(execute_transition) まで旧 epoch の鍵を維持する
                    // （受信側も op22 まで旧 epoch のため、早く切替えると churn 中に復号できず途切れる）。
                    if opcode == 29 || opcode == 30 {
                        let tid = if payload.len() >= 2 {
                            u16::from_be_bytes([payload[0], payload[1]])
                        } else {
                            0
                        };
                        if tid == 0 {
                            if let Ok(mut guard) = dave_cryptor.lock() {
                                let ep = dave.epoch();
                                if ep >= 1 {
                                    if let Some(secret) = dave.sender_secret() {
                                        *guard = Some(crate::dave::cryptor::FrameCryptor::with_epoch(secret, ep));
                                        tracing::info!(epoch = ep, "dave: initial transition -> E2EE active");
                                    }
                                }
                            }
                        }
                    }
                }
            }
            Ok(Message::Close(_)) | Err(_) => break,
            _ => {}
        }
    }
    tracing::info!("voice gateway reader stopped");
}

/// DaveOut を WS へ送出し、グループ確立後は送信者鍵で FrameCryptor を用意する。
#[cfg(feature = "dave")]
fn dispatch_dave<M: crate::dave::session::MlsBackend>(
    gw_tx: &mpsc::UnboundedSender<Message>,
    dave: &crate::dave::session::DaveSession<M>,
    dave_cryptor: &DaveCryptor,
    outs: Vec<crate::dave::session::DaveOut>,
) {
    use crate::dave::session::DaveOut;
    for out in outs {
        match out {
            DaveOut::Json(s) => {
                tracing::info!(msg = %s, "dave -> json");
                let _ = gw_tx.send(Message::Text(s.into()));
            }
            DaveOut::Binary(b) => {
                let opc = b.first().copied().unwrap_or(0);
                // op28(commit+welcome) は公式クライアントとの diff 用に全バイトを hex 出力する。
                if opc == 28 {
                    tracing::info!(
                        hex = %b.iter().map(|x| format!("{:02x}", x)).collect::<String>(),
                        "dave -> op28 FULL HEX (for diff)"
                    );
                }
                tracing::info!(opcode = opc, len = b.len(), "dave -> binary");
                let _ = gw_tx.send(Message::Binary(b.into()));
            }
        }
    }
    // 注: E2EE の有効化は op22(execute_transition) 受信時に行う（reader_task 側）。
    // それまではパススルー（平文 OPUS）で送る。MLS 未確立中に E2EE 化すると受信側が
    // 復号できず破棄され無音になるため。
    let _ = (dave, dave_cryptor);
}

async fn audio_task(
    socket: UdpSocket,
    cipher: Cipher,
    ssrc: u32,
    mut rx: mpsc::Receiver<Vec<u8>>,
    dave_cryptor: DaveCryptor,
    paused: Arc<AtomicBool>,
) {
    use rtp::Packetizer;
    use tokio::sync::mpsc::error::TryRecvError;

    #[cfg(not(feature = "dave"))]
    let _ = &dave_cryptor;

    let mut pk = Packetizer::new(ssrc);
    let mut ticker = tokio::time::interval(Duration::from_millis(20));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut silence_left: u8 = 0;
    // 送出レート計測（20ms間隔なら 50fps が正常）。
    let mut sent_count: u32 = 0;
    let mut last_log = tokio::time::Instant::now();

    loop {
        ticker.tick().await;
        if last_log.elapsed() >= Duration::from_secs(1) {
            tracing::info!(frames_per_sec = sent_count, "audio: send rate");
            sent_count = 0;
            last_log = tokio::time::Instant::now();
        }
        let frame = if paused.load(Ordering::Relaxed) {
            // 一時停止中: 送出キューは消費せず（フレームを温存）、直近フレーム後の無音 5 個を
            // 流し切ってから無音(None)。デコード側はキュー満杯でブロックし自然に停止する。
            if silence_left > 0 {
                silence_left -= 1;
                Some(SILENCE_FRAME.to_vec())
            } else {
                None
            }
        } else {
            match rx.try_recv() {
                Ok(f) => {
                    silence_left = 5;
                    Some(f)
                }
                Err(TryRecvError::Empty) => {
                    if silence_left > 0 {
                        silence_left -= 1;
                        Some(SILENCE_FRAME.to_vec())
                    } else {
                        None
                    }
                }
                Err(TryRecvError::Disconnected) => break,
            }
        };
        if let Some(opus) = frame {
            // DAVE 有効かつ鍵確立後は OPUS を E2EE 暗号化してから RTP トランスポート暗号で包む。
            #[cfg(feature = "dave")]
            let opus = match dave_cryptor.lock() {
                Ok(mut guard) => match guard.as_mut() {
                    Some(fc) => fc.encrypt(&opus).unwrap_or(opus),
                    None => opus,
                },
                Err(_) => opus,
            };
            if let Some(pkt) = pk.build(&cipher, &opus) {
                let _ = socket.send(&pkt).await;
                sent_count += 1;
            }
        }
    }
}
