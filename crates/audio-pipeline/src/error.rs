//! 音声パイプラインのエラー型。

#[derive(Debug, thiserror::Error)]
pub enum AudioError {
    #[error("opus error: {0}")]
    Opus(String),
    #[error("decode error: {0}")]
    Decode(String),
    #[error("unsupported: {0}")]
    Unsupported(&'static str),
}

impl From<opus::Error> for AudioError {
    fn from(e: opus::Error) -> Self {
        AudioError::Opus(format!("{e:?}"))
    }
}
