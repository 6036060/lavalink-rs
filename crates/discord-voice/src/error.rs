//! Discord 音声接続のエラー型。

#[derive(Debug, thiserror::Error)]
pub enum VoiceError {
    #[error("websocket error: {0}")]
    Ws(#[from] tokio_tungstenite::tungstenite::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("voice gateway closed before handshake completed")]
    ClosedEarly,
    #[error("voice gateway closed during handshake: code={code} reason='{reason}' ({hint})")]
    GatewayClosed { code: u16, reason: String, hint: &'static str },
    #[error("no supported encryption mode offered by Discord (need {0})")]
    NoSupportedMode(&'static str),
    #[error("malformed gateway payload: {0}")]
    Protocol(&'static str),
    #[error("IP discovery failed: {0}")]
    IpDiscovery(&'static str),
}
