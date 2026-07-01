//! 成長するバッファ上の `MediaSource`（ストリーミングダウンロード用）。
//!
//! ダウンロードタスクが [`SharedBuffer`] にチャンクを追記し、デコーダ（spawn_blocking）が
//! [`StreamingSource`] 経由で読む。未到達バイトの read は短時間スリープして待つ。
//! これにより全曲をメモリに溜めず、ダウンロードしながら逐次デコードできる。

use std::io::{self, Read, Seek, SeekFrom};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use symphonia::core::io::MediaSource;

/// ダウンロード済みバイトを蓄える共有バッファ。
pub struct SharedBuffer {
    data: Vec<u8>,
    done: bool,
    total: Option<u64>,
}

impl SharedBuffer {
    pub fn new(total: Option<u64>) -> Self {
        Self { data: Vec::new(), done: false, total }
    }
    /// ダウンロードしたチャンクを追記する。
    pub fn push(&mut self, chunk: &[u8]) {
        self.data.extend_from_slice(chunk);
    }
    /// ダウンロード完了を通知する。
    pub fn finish(&mut self) {
        self.done = true;
    }
    /// 現在までにダウンロード済みのバイト数。
    pub fn downloaded(&self) -> usize {
        self.data.len()
    }
}

/// [`SharedBuffer`] 上の読み取りカーソル（symphonia の `MediaSource`）。
pub struct StreamingSource {
    buf: Arc<Mutex<SharedBuffer>>,
    pos: u64,
}

impl StreamingSource {
    pub fn new(buf: Arc<Mutex<SharedBuffer>>) -> Self {
        Self { buf, pos: 0 }
    }
}

impl Read for StreamingSource {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        loop {
            {
                let g = self.buf.lock().unwrap();
                let avail = g.data.len() as u64;
                if self.pos < avail {
                    let start = self.pos as usize;
                    let n = ((avail - self.pos) as usize).min(out.len());
                    out[..n].copy_from_slice(&g.data[start..start + n]);
                    self.pos += n as u64;
                    return Ok(n);
                }
                if g.done {
                    return Ok(0); // EOF
                }
            }
            // 未到達バイト: ダウンロードを少し待つ（spawn_blocking スレッド内なので可）。
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Seek for StreamingSource {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let new = match pos {
            SeekFrom::Start(p) => p,
            SeekFrom::Current(d) => (self.pos as i64 + d).max(0) as u64,
            SeekFrom::End(d) => loop {
                let g = self.buf.lock().unwrap();
                if let Some(t) = g.total {
                    break (t as i64 + d).max(0) as u64;
                }
                if g.done {
                    break (g.data.len() as i64 + d).max(0) as u64;
                }
                drop(g);
                std::thread::sleep(Duration::from_millis(10));
            },
        };
        self.pos = new;
        Ok(new)
    }
}

impl MediaSource for StreamingSource {
    // 非シークにして symphonia を sequential 読みに強制する。シーク可だと末尾の moov 確認等で
    // ファイル終端へシークし、フルダウンロードを待ってしまう（ストリーミングにならない）。
    // YouTube itag 140 は moov 先頭(faststart)なので sequential で先頭から逐次デコードできる。
    fn is_seekable(&self) -> bool {
        false
    }
    fn byte_len(&self) -> Option<u64> {
        None
    }
}
