//! UDP IP Discovery（NAT 越えのための外部 IP/port 取得）。

use tokio::net::UdpSocket;

use crate::error::VoiceError;

/// 74 バイトのリクエストを送り、レスポンスから外部 (IP, port) を得る。
pub async fn ip_discovery(socket: &UdpSocket, ssrc: u32) -> Result<(String, u16), VoiceError> {
    let mut req = [0u8; 74];
    req[0..2].copy_from_slice(&1u16.to_be_bytes()); // type = 0x0001 (request)
    req[2..4].copy_from_slice(&70u16.to_be_bytes()); // length (excl. type+length)
    req[4..8].copy_from_slice(&ssrc.to_be_bytes());
    socket.send(&req).await?;

    let mut buf = [0u8; 74];
    let n = socket.recv(&mut buf).await?;
    parse_discovery_response(&buf[..n])
}

/// IP Discovery レスポンス（type=2）を解析する。
#[allow(clippy::result_large_err)]
pub fn parse_discovery_response(buf: &[u8]) -> Result<(String, u16), VoiceError> {
    if buf.len() < 74 {
        return Err(VoiceError::IpDiscovery("short response"));
    }
    if u16::from_be_bytes([buf[0], buf[1]]) != 2 {
        return Err(VoiceError::IpDiscovery("unexpected response type"));
    }
    // [0..2]type [2..4]len [4..8]ssrc [8..72]address(null-terminated) [72..74]port
    let addr = &buf[8..72];
    let end = addr.iter().position(|&b| b == 0).unwrap_or(addr.len());
    let ip = std::str::from_utf8(&addr[..end])
        .map_err(|_| VoiceError::IpDiscovery("non-utf8 address"))?
        .to_string();
    let port = u16::from_be_bytes([buf[72], buf[73]]);
    Ok((ip, port))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_discovery_response() {
        let mut buf = [0u8; 74];
        buf[0..2].copy_from_slice(&2u16.to_be_bytes());
        buf[2..4].copy_from_slice(&70u16.to_be_bytes());
        buf[4..8].copy_from_slice(&123u32.to_be_bytes());
        let ip = b"203.0.113.5";
        buf[8..8 + ip.len()].copy_from_slice(ip);
        buf[72..74].copy_from_slice(&54321u16.to_be_bytes());
        let (got_ip, got_port) = parse_discovery_response(&buf).unwrap();
        assert_eq!(got_ip, "203.0.113.5");
        assert_eq!(got_port, 54321);
    }
}
