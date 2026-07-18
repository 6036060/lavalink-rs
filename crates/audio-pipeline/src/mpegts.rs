//! 最小限の MPEG-TS デマルチプレクサ（HLS ライブ再生用）。
//!
//! YouTube ライブ等の HLS セグメントは MPEG-TS コンテナ（H.264 映像 + AAC/ADTS 音声）
//! だが、symphonia は MPEG-TS を読めない。ここでは TS パケット列から
//! AAC(ADTS, stream_type 0x0F) の ES バイト列だけを抽出し、そのまま symphonia の
//! AdtsReader（拡張子ヒント "aac"）でデコードできる形にする。映像など他 PID は捨てる。
//!
//! 実装範囲: PAT → 最初のプログラムの PMT → stream_type 0x0F の PID を特定し、
//! その PID の PES ペイロードを連結する。PAT/PMT は 1 TS パケットに収まる前提
//! （YouTube ライブでは十分）。CRC は検証しない。

/// TS バイト列（任意のチャンク境界）を受け取り ADTS を吐き出すステートフル抽出器。
pub struct TsToAdts {
    /// 188 バイト境界に満たない持ち越しバイト。
    pending: Vec<u8>,
    pmt_pid: Option<u16>,
    aac_pid: Option<u16>,
}

impl Default for TsToAdts {
    fn default() -> Self {
        Self::new()
    }
}

impl TsToAdts {
    pub fn new() -> Self {
        Self { pending: Vec::new(), pmt_pid: None, aac_pid: None }
    }

    /// TS チャンクを受け取り、抽出できた ADTS バイト列を返す。
    /// チャンクは任意の位置で切れていてよい（内部で 188 バイト境界に整列する）。
    pub fn push(&mut self, chunk: &[u8]) -> Vec<u8> {
        self.pending.extend_from_slice(chunk);
        let mut out = Vec::new();
        let mut pos = 0usize;
        while self.pending.len().saturating_sub(pos) >= 188 {
            if self.pending[pos] != 0x47 {
                // 同期喪失: 次の sync byte まで 1 バイトずつ読み飛ばす。
                pos += 1;
                continue;
            }
            let mut pkt = [0u8; 188];
            pkt.copy_from_slice(&self.pending[pos..pos + 188]);
            self.handle_packet(&pkt, &mut out);
            pos += 188;
        }
        self.pending.drain(..pos);
        out
    }

    fn handle_packet(&mut self, p: &[u8; 188], out: &mut Vec<u8>) {
        let pusi = p[1] & 0x40 != 0; // payload_unit_start_indicator
        let pid = (((p[1] & 0x1F) as u16) << 8) | p[2] as u16;
        let afc = (p[3] >> 4) & 0x3; // adaptation_field_control
        if afc == 0 || afc == 2 {
            return; // ペイロード無し
        }
        let mut off = 4usize;
        if afc == 3 {
            let af_len = p[4] as usize;
            off = 5 + af_len;
            if off >= 188 {
                return;
            }
        }
        let payload = &p[off..];

        if pid == 0 {
            self.parse_pat(payload, pusi);
        } else if Some(pid) == self.pmt_pid {
            self.parse_pmt(payload, pusi);
        } else if Some(pid) == self.aac_pid {
            let es = if pusi { strip_pes_header(payload) } else { Some(payload) };
            if let Some(es) = es {
                out.extend_from_slice(es);
            }
        }
    }

    /// PAT から最初のプログラムの PMT PID を得る。
    fn parse_pat(&mut self, payload: &[u8], pusi: bool) {
        if !pusi || self.pmt_pid.is_some() {
            return;
        }
        let Some(sec) = section(payload) else { return };
        if sec.first() != Some(&0x00) || sec.len() < 12 {
            return; // table_id != PAT
        }
        let section_length = (((sec[1] & 0x0F) as usize) << 8) | sec[2] as usize;
        let end = (3 + section_length).saturating_sub(4).min(sec.len()); // CRC を除く
        let mut i = 8;
        while i + 4 <= end {
            let program = ((sec[i] as u16) << 8) | sec[i + 1] as u16;
            let pid = (((sec[i + 2] & 0x1F) as u16) << 8) | sec[i + 3] as u16;
            if program != 0 {
                self.pmt_pid = Some(pid);
                return;
            }
            i += 4;
        }
    }

    /// PMT から AAC(ADTS, stream_type 0x0F) の elementary PID を得る。
    fn parse_pmt(&mut self, payload: &[u8], pusi: bool) {
        if !pusi || self.aac_pid.is_some() {
            return;
        }
        let Some(sec) = section(payload) else { return };
        if sec.first() != Some(&0x02) || sec.len() < 12 {
            return; // table_id != PMT
        }
        let section_length = (((sec[1] & 0x0F) as usize) << 8) | sec[2] as usize;
        let end = (3 + section_length).saturating_sub(4).min(sec.len());
        let program_info_len = (((sec[10] & 0x0F) as usize) << 8) | sec[11] as usize;
        let mut i = 12 + program_info_len;
        while i + 5 <= end {
            let stream_type = sec[i];
            let pid = (((sec[i + 1] & 0x1F) as u16) << 8) | sec[i + 2] as u16;
            let es_info_len = (((sec[i + 3] & 0x0F) as usize) << 8) | sec[i + 4] as usize;
            if stream_type == 0x0F {
                self.aac_pid = Some(pid);
                return;
            }
            i += 5 + es_info_len;
        }
    }
}

/// pointer_field を飛ばしてセクション先頭を返す（PUSI 付き PSI パケット用）。
fn section(payload: &[u8]) -> Option<&[u8]> {
    let ptr = *payload.first()? as usize;
    payload.get(1 + ptr..)
}

/// PES ヘッダを剥がして ES（ADTS）先頭を返す。
fn strip_pes_header(payload: &[u8]) -> Option<&[u8]> {
    if payload.len() < 9 || payload[0] != 0 || payload[1] != 0 || payload[2] != 1 {
        return None;
    }
    let header_len = payload[8] as usize;
    payload.get(9 + header_len..)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// payload のみ（adaptation field 無し）の TS パケットを作る。
    fn ts_packet(pid: u16, pusi: bool, payload: &[u8]) -> [u8; 188] {
        assert!(payload.len() <= 184);
        let mut p = [0xFFu8; 188];
        p[0] = 0x47;
        p[1] = ((pid >> 8) as u8 & 0x1F) | if pusi { 0x40 } else { 0 };
        p[2] = pid as u8;
        p[3] = 0x10; // adaptation_field_control=01 (payload only), cc=0
        p[4..4 + payload.len()].copy_from_slice(payload);
        p
    }

    fn pat(pmt_pid: u16) -> Vec<u8> {
        // pointer(0), table_id(0), section_length=13, tsid, flags, sec#, last#,
        // program=1 → pmt_pid, CRC(dummy 4B)
        let mut s = vec![
            0x00, // pointer_field
            0x00, // table_id (PAT)
            0xB0, 13, // section_length
            0x00, 0x01, // transport_stream_id
            0xC1, // version/current_next
            0x00, 0x00, // section_number / last_section_number
            0x00, 0x01, // program_number = 1
            0xE0 | ((pmt_pid >> 8) as u8 & 0x1F),
            pmt_pid as u8,
        ];
        s.extend_from_slice(&[0, 0, 0, 0]); // CRC (未検証)
        s
    }

    fn pmt(aac_pid: u16) -> Vec<u8> {
        let mut s = vec![
            0x00, // pointer_field
            0x02, // table_id (PMT)
            0xB0, 18, // section_length = 9 + 5(ES) + 4(CRC)
            0x00, 0x01, // program_number
            0xC1, // version/current_next
            0x00, 0x00, // section_number / last_section_number
            0xE0, 0x00, // PCR PID (dummy)
            0xF0, 0x00, // program_info_length = 0
            0x0F, // stream_type = ADTS AAC
            0xE0 | ((aac_pid >> 8) as u8 & 0x1F),
            aac_pid as u8,
            0xF0, 0x00, // ES_info_length = 0
        ];
        s.extend_from_slice(&[0, 0, 0, 0]); // CRC
        s
    }

    fn pes(adts: &[u8]) -> Vec<u8> {
        let mut s = vec![
            0x00, 0x00, 0x01, // PES start code
            0xC0, // stream_id (audio)
            0x00, 0x00, // PES_packet_length (0 = 不定)
            0x80, 0x80, // flags (PTS あり)
            0x05, // PES_header_data_length
            0x21, 0x00, 0x01, 0x00, 0x01, // PTS (dummy 5B)
        ];
        s.extend_from_slice(adts);
        s
    }

    #[test]
    fn extracts_adts_from_ts() {
        let adts = [0xFFu8, 0xF1, 0x50, 0x80, 0x01, 0x1F, 0xFC, 0xDE, 0xAD, 0xBE, 0xEF];
        let mut stream = Vec::new();
        stream.extend_from_slice(&ts_packet(0, true, &pat(0x0100)));
        stream.extend_from_slice(&ts_packet(0x0100, true, &pmt(0x0101)));
        stream.extend_from_slice(&ts_packet(0x0101, true, &pes(&adts)));

        let mut ts = TsToAdts::new();
        let out = ts.push(&stream);
        // PES パケットの残り（0xFF パディング）も含むため先頭一致で確認。
        assert!(out.starts_with(&adts), "out={out:02X?}");
    }

    #[test]
    fn handles_arbitrary_chunk_boundaries() {
        let adts = [0xFFu8, 0xF1, 0x4C, 0x80, 0x02, 0x00, 0xFC];
        let mut stream = Vec::new();
        stream.extend_from_slice(&ts_packet(0, true, &pat(0x0100)));
        stream.extend_from_slice(&ts_packet(0x0100, true, &pmt(0x0101)));
        stream.extend_from_slice(&ts_packet(0x0101, true, &pes(&adts)));

        let mut ts = TsToAdts::new();
        let mut out = Vec::new();
        for chunk in stream.chunks(61) {
            out.extend_from_slice(&ts.push(chunk));
        }
        assert!(out.starts_with(&adts));
    }

    #[test]
    fn continuation_packets_append_payload() {
        let adts_head = [0xFFu8, 0xF1, 0x4C, 0x80, 0x02, 0x00];
        let cont = [0xAAu8; 10];
        let mut stream = Vec::new();
        stream.extend_from_slice(&ts_packet(0, true, &pat(0x0100)));
        stream.extend_from_slice(&ts_packet(0x0100, true, &pmt(0x0101)));
        stream.extend_from_slice(&ts_packet(0x0101, true, &pes(&adts_head)));
        stream.extend_from_slice(&ts_packet(0x0101, false, &cont));

        let mut ts = TsToAdts::new();
        let out = ts.push(&stream);
        assert!(out.starts_with(&adts_head));
        // 継続パケットのペイロードは PES ヘッダ無しでそのまま連結される。
        assert!(out.windows(cont.len()).any(|w| w == cont));
    }

    #[test]
    fn ignores_other_pids() {
        let mut stream = Vec::new();
        stream.extend_from_slice(&ts_packet(0, true, &pat(0x0100)));
        stream.extend_from_slice(&ts_packet(0x0100, true, &pmt(0x0101)));
        stream.extend_from_slice(&ts_packet(0x0102, true, &pes(&[1, 2, 3]))); // 映像等の別 PID
        let mut ts = TsToAdts::new();
        assert!(ts.push(&stream).is_empty());
    }
}
