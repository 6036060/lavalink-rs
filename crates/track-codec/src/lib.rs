//! Lavaplayer 互換の encoded track コーデック（チケット 2-5 / フェーズ0-3）。
//!
//! バイト実証済みフォーマット（version 2 は公式サンプルで全バイト検証）:
//! ```text
//! [i32 header: bit30=versioned flag, 下位30bit=body size]
//! body:
//!   [u8 version]                 // versioned flag が立つ場合
//!   title:      JavaUTF
//!   author:     JavaUTF
//!   length:     i64 (ms)
//!   identifier: JavaUTF
//!   isStream:   u8(bool)
//!   uri:        nullable text    // version >= 2
//!   artworkUrl: nullable text    // version >= 3
//!   isrc:       nullable text    // version >= 3
//!   sourceName: JavaUTF
//!   (source 固有データ)          // source manager 依存・本コーデックは読み飛ばす
//!   position:   i64 (ms)         // body の末尾 8 バイト
//! ```
//! JavaUTF = [u16 BE 長][修正 UTF-8]（`DataOutput.writeUTF` 互換）。
//! nullable text = [u8 存在フラグ][存在時のみ JavaUTF]。

#![forbid(unsafe_code)]

use base64::Engine as _;
use lavalink_protocol::TrackInfo;

pub const TRACK_INFO_VERSIONED_FLAG: i32 = 0x4000_0000;
pub const SIZE_MASK: i32 = 0x3FFF_FFFF;
/// 本コーデックがエンコード時に出力するバージョン。
pub const TRACK_INFO_VERSION: u8 = 3;

#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("base64 decode failed: {0}")]
    Base64(#[from] base64::DecodeError),
    #[error("unexpected end of buffer")]
    Truncated,
    #[error("invalid modified UTF-8")]
    InvalidUtf8,
}

const B64: base64::engine::general_purpose::GeneralPurpose = base64::engine::general_purpose::STANDARD;

// ------------------------------- decode -------------------------------

/// Base64 の encoded track を [`TrackInfo`] にデコードする。
/// `info.position` には末尾に格納された位置（ms）が入る。
pub fn decode(encoded: &str) -> Result<TrackInfo, CodecError> {
    let raw = B64.decode(encoded.trim())?;
    let mut r = Reader::new(&raw);

    let header = r.i32()?;
    let versioned = header & TRACK_INFO_VERSIONED_FLAG != 0;
    let size = (header & SIZE_MASK) as usize;
    let version = if versioned { r.u8()? } else { 1 };

    let title = r.java_utf()?;
    let author = r.java_utf()?;
    let length = r.i64()? as u64;
    let identifier = r.java_utf()?;
    let is_stream = r.u8()? != 0;
    let uri = if version >= 2 { r.nullable_utf()? } else { None };
    let artwork_url = if version >= 3 { r.nullable_utf()? } else { None };
    let isrc = if version >= 3 { r.nullable_utf()? } else { None };
    let source_name = r.java_utf()?;

    // position は body 末尾の 8 バイト（source 固有データは読み飛ばす）。
    let body_end = (4 + size).min(raw.len());
    if body_end < 8 {
        return Err(CodecError::Truncated);
    }
    let position = i64::from_be_bytes(
        raw[body_end - 8..body_end].try_into().map_err(|_| CodecError::Truncated)?,
    ) as u64;

    Ok(TrackInfo {
        identifier,
        is_seekable: !is_stream,
        author,
        length,
        is_stream,
        position,
        title,
        uri,
        artwork_url,
        isrc,
        source_name,
    })
}

// ------------------------------- encode -------------------------------

/// [`TrackInfo`] を version 3 形式の Base64 encoded track にエンコードする。
/// source 固有データは書かない（自前生成トラック用。公式 source との完全 round-trip は
/// フェーズ5 で各 source manager 実装時に対応）。
pub fn encode(info: &TrackInfo) -> String {
    let mut body: Vec<u8> = Vec::with_capacity(128);
    body.push(TRACK_INFO_VERSION);
    write_java_utf(&mut body, &info.title);
    write_java_utf(&mut body, &info.author);
    body.extend_from_slice(&(info.length as i64).to_be_bytes());
    write_java_utf(&mut body, &info.identifier);
    body.push(info.is_stream as u8);
    write_nullable(&mut body, info.uri.as_deref());
    write_nullable(&mut body, info.artwork_url.as_deref());
    write_nullable(&mut body, info.isrc.as_deref());
    write_java_utf(&mut body, &info.source_name);
    body.extend_from_slice(&(info.position as i64).to_be_bytes());

    let header = (body.len() as i32) | TRACK_INFO_VERSIONED_FLAG;
    let mut out = Vec::with_capacity(body.len() + 4);
    out.extend_from_slice(&header.to_be_bytes());
    out.extend_from_slice(&body);
    B64.encode(out)
}

// ------------------------------- helpers -------------------------------

fn write_nullable(buf: &mut Vec<u8>, s: Option<&str>) {
    match s {
        Some(v) => {
            buf.push(1);
            write_java_utf(buf, v);
        }
        None => buf.push(0),
    }
}

/// Java `DataOutput.writeUTF` 互換（修正 UTF-8 + u16 BE 長）。
fn write_java_utf(buf: &mut Vec<u8>, s: &str) {
    let start = buf.len();
    buf.extend_from_slice(&[0, 0]); // 長さプレースホルダ
    for u in s.encode_utf16() {
        match u {
            0x0001..=0x007F => buf.push(u as u8),
            0 | 0x0080..=0x07FF => {
                buf.push(0xC0 | (u >> 6) as u8);
                buf.push(0x80 | (u & 0x3F) as u8);
            }
            _ => {
                buf.push(0xE0 | (u >> 12) as u8);
                buf.push(0x80 | ((u >> 6) & 0x3F) as u8);
                buf.push(0x80 | (u & 0x3F) as u8);
            }
        }
    }
    let len = (buf.len() - start - 2) as u16;
    buf[start..start + 2].copy_from_slice(&len.to_be_bytes());
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8], CodecError> {
        if self.pos + n > self.buf.len() {
            return Err(CodecError::Truncated);
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8, CodecError> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16, CodecError> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn i32(&mut self) -> Result<i32, CodecError> {
        Ok(i32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn i64(&mut self) -> Result<i64, CodecError> {
        Ok(i64::from_be_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn nullable_utf(&mut self) -> Result<Option<String>, CodecError> {
        if self.u8()? != 0 {
            Ok(Some(self.java_utf()?))
        } else {
            Ok(None)
        }
    }
    /// 修正 UTF-8 文字列を読む。
    fn java_utf(&mut self) -> Result<String, CodecError> {
        let len = self.u16()? as usize;
        let bytes = self.take(len)?;
        let mut units: Vec<u16> = Vec::with_capacity(len);
        let mut i = 0;
        while i < len {
            let a = bytes[i];
            if a & 0x80 == 0 {
                units.push(a as u16);
                i += 1;
            } else if a & 0xE0 == 0xC0 {
                if i + 1 >= len {
                    return Err(CodecError::InvalidUtf8);
                }
                let b = bytes[i + 1];
                units.push(((a as u16 & 0x1F) << 6) | (b as u16 & 0x3F));
                i += 2;
            } else {
                if i + 2 >= len {
                    return Err(CodecError::InvalidUtf8);
                }
                let b = bytes[i + 1];
                let c = bytes[i + 2];
                units.push(((a as u16 & 0x0F) << 12) | ((b as u16 & 0x3F) << 6) | (c as u16 & 0x3F));
                i += 3;
            }
        }
        String::from_utf16(&units).map_err(|_| CodecError::InvalidUtf8)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 公式ドキュメントのサンプル（version 2, Rick Astley）。
    const SAMPLE: &str = "QAAAjQIAJVJpY2sgQXN0bGV5IC0gTmV2ZXIgR29ubmEgR2l2ZSBZb3UgVXAADlJpY2tBc3RsZXlWRVZPAAAAAAADPCAAC2RRdzR3OVdnWGNRAAEAK2h0dHBzOi8vd3d3LnlvdXR1YmUuY29tL3dhdGNoP3Y9ZFF3NHc5V2dYY1EAB3lvdXR1YmUAAAAAAAAAAA==";

    #[test]
    fn decodes_official_v2_sample() {
        let info = decode(SAMPLE).unwrap();
        assert_eq!(info.title, "Rick Astley - Never Gonna Give You Up");
        assert_eq!(info.author, "RickAstleyVEVO");
        assert_eq!(info.length, 212_000);
        assert_eq!(info.identifier, "dQw4w9WgXcQ");
        assert!(!info.is_stream);
        assert!(info.is_seekable);
        assert_eq!(info.uri.as_deref(), Some("https://www.youtube.com/watch?v=dQw4w9WgXcQ"));
        assert_eq!(info.source_name, "youtube");
        assert_eq!(info.position, 0);
        assert_eq!(info.artwork_url, None);
        assert_eq!(info.isrc, None);
    }

    #[test]
    fn round_trip_v3() {
        let info = TrackInfo {
            identifier: "abc123".into(),
            is_seekable: true,
            author: "Författare / 著者 \u{1F600}".into(),
            length: 240_000,
            is_stream: false,
            position: 1234,
            title: "Tëst — タイトル".into(),
            uri: Some("https://example.com/x".into()),
            artwork_url: Some("https://img/x.jpg".into()),
            isrc: Some("US-ABC-12-34567".into()),
            source_name: "youtube".into(),
        };
        let enc = encode(&info);
        let back = decode(&enc).unwrap();
        assert_eq!(back.identifier, info.identifier);
        assert_eq!(back.author, info.author);
        assert_eq!(back.title, info.title);
        assert_eq!(back.length, info.length);
        assert_eq!(back.position, info.position);
        assert_eq!(back.uri, info.uri);
        assert_eq!(back.artwork_url, info.artwork_url);
        assert_eq!(back.isrc, info.isrc);
        assert_eq!(back.source_name, info.source_name);
        assert_eq!(back.is_seekable, !info.is_stream);
    }
}
