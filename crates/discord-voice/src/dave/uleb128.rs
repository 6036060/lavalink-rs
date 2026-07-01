//! ULEB128（unsigned little-endian base-128）可変長エンコード。
//! Whitepaper の C 擬似コードに準拠。

/// 値を ULEB128 でエンコードして末尾に追記する。
pub fn encode_to(buf: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        buf.push(0x80 | (value as u8 & 0x7F));
        value >>= 7;
    }
    buf.push(value as u8);
}

pub fn encode(value: u64) -> Vec<u8> {
    let mut v = Vec::new();
    encode_to(&mut v, value);
    v
}

/// 先頭から ULEB128 を 1 つデコードし、(値, 消費バイト数) を返す。
pub fn decode(buf: &[u8]) -> Option<(u64, usize)> {
    let mut result: u64 = 0;
    let mut shift = 0u32;
    for (i, &b) in buf.iter().enumerate() {
        if shift >= 64 {
            return None; // overflow
        }
        result |= ((b & 0x7F) as u64) << shift;
        if b & 0x80 == 0 {
            return Some((result, i + 1));
        }
        shift += 7;
    }
    None // truncated
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_various() {
        for v in [0u64, 1, 127, 128, 255, 300, 16383, 16384, 0xDEAD_BEEF, u32::MAX as u64] {
            let enc = encode(v);
            let (dec, n) = decode(&enc).unwrap();
            assert_eq!(dec, v);
            assert_eq!(n, enc.len());
        }
    }

    #[test]
    fn known_encodings() {
        assert_eq!(encode(0), vec![0x00]);
        assert_eq!(encode(127), vec![0x7F]);
        assert_eq!(encode(128), vec![0x80, 0x01]);
        assert_eq!(encode(300), vec![0xAC, 0x02]);
    }
}
