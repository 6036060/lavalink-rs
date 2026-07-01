//! YouTube 抽出（InnerTube API, yt-dlp 非依存）。フェーズ5 の土台。
//!
//! 戦略: InnerTube `/youtubei/v1/player` を複数クライアント（ANDROID/IOS/TVHTML5）で叩き、
//! `signatureCipher` を伴わず直接 `url` を持つ音声フォーマット（AAC/m4a, itag 140 優先）を選ぶ。
//! 検索は `/youtubei/v1/search`（WEB クライアント）。再生は既存の playback 経路で URL を取得して
//! デコードする（symphonia が AAC を扱えるため itag 140 を採用。Opus/WebM はパススルー実装後に対応）。
//!
//! ⚠️ 2026 時点で YouTube は PoToken / BotGuard による保護を強めており、本実装だけでは
//! 直リンクが取得できない（403/ throttled）場合がある。その場合は PoToken/OAuth 供給
//! （チケット 5-2 / 5-7）が必要。クライアントのバージョン文字列も定期更新が前提。

#![forbid(unsafe_code)]

use lavalink_protocol::TrackInfo;
use serde_json::{json, Value};

#[derive(Debug, thiserror::Error)]
pub enum YtError {
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("no playable format found (PoToken required?)")]
    NoFormat,
    #[error("video unavailable: {0}")]
    Unavailable(String),
    #[error("invalid identifier")]
    InvalidId,
}

/// InnerTube クライアント定義（player 用）。バージョンは要定期更新。
struct InnertubeClient {
    name: &'static str,
    version: &'static str,
    api_key: &'static str,
    user_agent: &'static str,
    /// context.client に追加する固有フィールド。
    extra: fn() -> Value,
}

const ANDROID: InnertubeClient = InnertubeClient {
    name: "ANDROID",
    version: "19.09.37",
    api_key: "AIzaSyA8eiZmM1FaDVjRy-df2KTyQ_vz_yYM39w",
    user_agent: "com.google.android.youtube/19.09.37 (Linux; U; Android 11) gzip",
    extra: || json!({ "androidSdkVersion": 30, "osName": "Android", "osVersion": "11" }),
};
const IOS: InnertubeClient = InnertubeClient {
    name: "IOS",
    version: "19.09.3",
    api_key: "AIzaSyB-63vPrdThhKuerbB2N_l7Kwwcxj6yUAc",
    user_agent: "com.google.ios.youtube/19.09.3 (iPhone14,3; U; CPU iOS 15_6 like Mac OS X)",
    extra: || json!({ "deviceModel": "iPhone14,3", "osName": "iOS", "osVersion": "15.6.0.19G71" }),
};
const TVHTML5: InnertubeClient = InnertubeClient {
    name: "TVHTML5_SIMPLY_EMBEDDED_PLAYER",
    version: "2.0",
    api_key: "AIzaSyAO_FJ2SlqU8Q4STEHLGCilw_Y9_11qcW8",
    user_agent: "Mozilla/5.0 (PlayStation; PlayStation 4/12.00) AppleWebKit/605.1.15 (KHTML, like Gecko)",
    extra: || json!({}),
};

const PLAYER_CLIENTS: &[&InnertubeClient] = &[&ANDROID, &IOS, &TVHTML5];
const WEB_KEY: &str = "AIzaSyAO_FJ2SlqU8Q4STEHLGCilw_Y9_11qcW8";

impl InnertubeClient {
    fn context(&self) -> Value {
        let mut client = json!({
            "clientName": self.name,
            "clientVersion": self.version,
            "hl": "en",
            "gl": "US",
        });
        if let (Value::Object(c), Value::Object(extra)) = (&mut client, (self.extra)()) {
            for (k, v) in extra {
                c.insert(k, v);
            }
        }
        json!({ "client": client })
    }
}

pub struct YoutubeClient {
    http: reqwest::Client,
    /// 直接指定された PoToken（環境変数 YT_POTOKEN）。ブラウザから貼って試せる。
    potoken: Option<String>,
    /// PoToken 供給サーバの URL（YT_POTOKEN_PROVIDER, 例: bgutil の http://127.0.0.1:4416）。
    provider_url: Option<String>,
    /// visitorData（YT_VISITOR_DATA）。PoToken のバインドに使う。
    visitor_data: Option<String>,
    /// invidious-companion の URL（YT_COMPANION_URL, 例 http://127.0.0.1:8282）。
    /// 設定時は PoToken / 署名復号を companion に委譲して解決する（推奨）。
    companion_url: Option<String>,
    /// companion の Bearer シークレット（YT_COMPANION_SECRET = companion の SERVER_SECRET_KEY）。
    companion_secret: Option<String>,
}

impl Default for YoutubeClient {
    fn default() -> Self {
        Self::new()
    }
}

impl YoutubeClient {
    pub fn new() -> Self {
        let http = reqwest::Client::builder()
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        Self {
            http,
            potoken: env("YT_POTOKEN"),
            provider_url: env("YT_POTOKEN_PROVIDER"),
            visitor_data: env("YT_VISITOR_DATA"),
            companion_url: env("YT_COMPANION_URL"),
            companion_secret: env("YT_COMPANION_SECRET"),
        }
    }

    /// PoToken を取得（直接指定 > 供給サーバ）。無ければ None。
    /// 供給サーバは bgutil 互換: POST {url}/get_pot {"content_binding": <vd or id>} -> {"po_token": ".."}。
    async fn po_token(&self, content_binding: &str) -> Option<String> {
        if let Some(t) = &self.potoken {
            return Some(t.clone());
        }
        let url = self.provider_url.as_ref()?;
        let binding = self.visitor_data.clone().unwrap_or_else(|| content_binding.to_string());
        let resp = self
            .http
            .post(format!("{url}/get_pot"))
            .json(&json!({ "content_binding": binding }))
            .send()
            .await
            .ok()?;
        let v: Value = resp.json().await.ok()?;
        v.get("po_token")
            .or_else(|| v.get("poToken"))
            .and_then(Value::as_str)
            .map(String::from)
    }

    /// invidious-companion 経由で player レスポンス（videoplayback URL 復号済み）を得る。
    async fn companion_player(&self, video_id: &str) -> Result<Value, YtError> {
        let base = self.companion_url.as_ref().ok_or(YtError::NoFormat)?;
        let mut req = self
            .http
            .post(format!("{base}/companion/youtubei/v1/player"))
            .header("Content-Type", "application/json")
            .json(&json!({ "videoId": video_id }));
        if let Some(secret) = &self.companion_secret {
            req = req.header("Authorization", format!("Bearer {secret}"));
        }
        let resp = req.send().await?.error_for_status()?;
        Ok(resp.json::<Value>().await?)
    }

    async fn call_player(
        &self,
        video_id: &str,
        client: &InnertubeClient,
        po_token: Option<&str>,
    ) -> Result<Value, YtError> {
        let url = format!("https://www.youtube.com/youtubei/v1/player?key={}", client.api_key);
        let mut context = client.context();
        if let Some(vd) = &self.visitor_data {
            if let Some(c) = context.get_mut("client").and_then(Value::as_object_mut) {
                c.insert("visitorData".to_string(), json!(vd));
            }
        }
        let mut body = json!({
            "videoId": video_id,
            "contentCheckOk": true,
            "racyCheckOk": true,
            "context": context,
        });
        if let Some(pot) = po_token {
            body["serviceIntegrityDimensions"] = json!({ "poToken": pot });
        }
        let resp = self
            .http
            .post(&url)
            .header("User-Agent", client.user_agent)
            .header("X-Goog-Api-Format-Version", "2")
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await?
            .error_for_status()?;
        Ok(resp.json::<Value>().await?)
    }

    /// メタデータ + 直接ストリーム URL を解決（複数クライアントをフォールバック）。
    pub async fn resolve_stream(&self, video_id: &str) -> Result<(TrackInfo, String), YtError> {
        // invidious-companion 経由（URL は復号済み・PoToken/署名復号は companion が処理）。
        if self.companion_url.is_some() {
            let resp = self.companion_player(video_id).await?;
            if let Some(reason) = playability_error(&resp) {
                return Err(YtError::Unavailable(reason));
            }
            return parse_player(&resp, video_id).ok_or(YtError::NoFormat);
        }

        let po_token = self.po_token(video_id).await;
        let mut last_status: Option<String> = None;
        for client in PLAYER_CLIENTS {
            match self.call_player(video_id, client, po_token.as_deref()).await {
                Ok(resp) => {
                    if let Some(reason) = playability_error(&resp) {
                        last_status = Some(reason);
                        continue;
                    }
                    if let Some((info, mut url)) = parse_player(&resp, video_id) {
                        if let Some(pot) = &po_token {
                            let sep = if url.contains('?') { '&' } else { '?' };
                            url = format!("{url}{sep}pot={pot}");
                        }
                        tracing::debug!(client = client.name, "resolved youtube stream");
                        return Ok((info, url));
                    }
                }
                Err(e) => {
                    tracing::debug!(client = client.name, error = %e, "player call failed");
                }
            }
        }
        match last_status {
            Some(s) => Err(YtError::Unavailable(s)),
            None => Err(YtError::NoFormat),
        }
    }

    /// メタデータのみ解決（uri=watch URL）。loadtracks の単曲用。
    pub async fn resolve_meta(&self, video_id: &str) -> Result<TrackInfo, YtError> {
        if self.companion_url.is_some() {
            let resp = self.companion_player(video_id).await?;
            if playability_error(&resp).is_none() {
                if let Some(info) = parse_meta(&resp, video_id) {
                    return Ok(info);
                }
            }
            return Err(YtError::NoFormat);
        }
        let po_token = self.po_token(video_id).await;
        for client in PLAYER_CLIENTS {
            if let Ok(resp) = self.call_player(video_id, client, po_token.as_deref()).await {
                if playability_error(&resp).is_none() {
                    if let Some(info) = parse_meta(&resp, video_id) {
                        return Ok(info);
                    }
                }
            }
        }
        Err(YtError::NoFormat)
    }

    /// 再生時の再解決: videoId → 直接ストリーム URL。
    pub async fn stream_url(&self, video_id: &str) -> Result<String, YtError> {
        Ok(self.resolve_stream(video_id).await?.1)
    }

    /// `ytsearch:` 検索。WEB クライアントの search を叩き videoRenderer を収集する。
    pub async fn search(&self, query: &str) -> Result<Vec<TrackInfo>, YtError> {
        let url = format!("https://www.youtube.com/youtubei/v1/search?key={WEB_KEY}");
        let body = json!({
            "query": query,
            "context": { "client": { "clientName": "WEB", "clientVersion": "2.20240101.00.00", "hl": "en", "gl": "US" } },
        });
        let resp = self
            .http
            .post(&url)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await?
            .error_for_status()?
            .json::<Value>()
            .await?;
        let mut out = Vec::new();
        collect_video_renderers(&resp, &mut out);
        Ok(out)
    }
}

/// playabilityStatus が OK 以外ならその理由を返す。
fn playability_error(resp: &Value) -> Option<String> {
    let status = resp.get("playabilityStatus")?;
    let s = status.get("status").and_then(Value::as_str).unwrap_or("OK");
    if s == "OK" {
        None
    } else {
        let reason = status
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or(s)
            .to_string();
        Some(reason)
    }
}

fn meta_from_details(details: &Value, video_id: &str) -> Option<TrackInfo> {
    let title = details.get("title")?.as_str()?.to_string();
    let author = details
        .get("author")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let length = details
        .get("lengthSeconds")
        .and_then(Value::as_str)
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0)
        * 1000;
    let is_live = details
        .get("isLiveContent")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Some(TrackInfo {
        identifier: video_id.to_string(),
        is_seekable: !is_live,
        author,
        length,
        is_stream: is_live,
        position: 0,
        title,
        uri: Some(format!("https://www.youtube.com/watch?v={video_id}")),
        artwork_url: Some(format!("https://i.ytimg.com/vi/{video_id}/hqdefault.jpg")),
        isrc: None,
        source_name: "youtube".to_string(),
    })
}

fn parse_meta(resp: &Value, video_id: &str) -> Option<TrackInfo> {
    meta_from_details(resp.get("videoDetails")?, video_id)
}

/// player レスポンスから (メタ, 直接ストリームURL) を作る。AAC/m4a(itag 140)を優先。
fn parse_player(resp: &Value, video_id: &str) -> Option<(TrackInfo, String)> {
    let info = parse_meta(resp, video_id)?;
    let formats = resp.get("streamingData")?.get("adaptiveFormats")?.as_array()?;

    let mut best: Option<(i64, String)> = None; // (優先度, url)
    for f in formats {
        let url = match f.get("url").and_then(Value::as_str) {
            Some(u) => u,
            None => continue, // signatureCipher のみ → JS 復号未対応のためスキップ
        };
        let mime = f.get("mimeType").and_then(Value::as_str).unwrap_or("");
        if !mime.starts_with("audio/") {
            continue;
        }
        let itag = f.get("itag").and_then(Value::as_i64).unwrap_or(0);
        // 優先度: itag 140(m4a/AAC, symphonia でデコード可) を最優先。
        let score = if itag == 140 {
            100
        } else if mime.starts_with("audio/mp4") {
            50
        } else {
            10 // audio/webm(opus) 等。デコーダ未対応だが最後の手段。
        };
        if best.as_ref().map(|(s, _)| score > *s).unwrap_or(true) {
            best = Some((score, url.to_string()));
        }
    }
    best.map(|(_, url)| (info, url))
}

/// JSON を再帰的に走査し videoRenderer から TrackInfo を収集する（検索結果用）。
fn collect_video_renderers(value: &Value, out: &mut Vec<TrackInfo>) {
    match value {
        Value::Object(map) => {
            if let Some(vr) = map.get("videoRenderer") {
                if let Some(info) = parse_video_renderer(vr) {
                    out.push(info);
                }
            }
            for v in map.values() {
                collect_video_renderers(v, out);
            }
        }
        Value::Array(arr) => {
            for v in arr {
                collect_video_renderers(v, out);
            }
        }
        _ => {}
    }
}

fn runs_text(v: &Value) -> Option<String> {
    if let Some(s) = v.get("simpleText").and_then(Value::as_str) {
        return Some(s.to_string());
    }
    let runs = v.get("runs")?.as_array()?;
    let mut s = String::new();
    for r in runs {
        if let Some(t) = r.get("text").and_then(Value::as_str) {
            s.push_str(t);
        }
    }
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

fn parse_length_text(v: &Value) -> u64 {
    // "3:45" / "1:02:03" → ms
    let text = runs_text(v).unwrap_or_default();
    let mut secs: u64 = 0;
    for part in text.split(':') {
        if let Ok(n) = part.trim().parse::<u64>() {
            secs = secs * 60 + n;
        } else {
            return 0;
        }
    }
    secs * 1000
}

fn parse_video_renderer(vr: &Value) -> Option<TrackInfo> {
    let video_id = vr.get("videoId")?.as_str()?.to_string();
    let title = vr.get("title").and_then(runs_text).unwrap_or_default();
    if title.is_empty() {
        return None;
    }
    let author = vr
        .get("ownerText")
        .or_else(|| vr.get("longBylineText"))
        .and_then(runs_text)
        .unwrap_or_default();
    let length = vr.get("lengthText").map(parse_length_text).unwrap_or(0);
    let is_live = length == 0; // 簡易: 長さ無し=ライブ扱い
    Some(TrackInfo {
        identifier: video_id.clone(),
        is_seekable: !is_live,
        author,
        length,
        is_stream: is_live,
        position: 0,
        title,
        uri: Some(format!("https://www.youtube.com/watch?v={video_id}")),
        artwork_url: Some(format!("https://i.ytimg.com/vi/{video_id}/hqdefault.jpg")),
        isrc: None,
        source_name: "youtube".to_string(),
    })
}

/// URL / 動画ID 文字列から 11 文字の videoId を抽出する。
pub fn extract_video_id(input: &str) -> Option<String> {
    let s = input.trim();
    // 生の 11 文字 ID
    if s.len() == 11 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Some(s.to_string());
    }
    // youtu.be/<id>
    if let Some(rest) = s.split("youtu.be/").nth(1) {
        let id: String = rest.chars().take(11).collect();
        if id.len() == 11 {
            return Some(id);
        }
    }
    // ...watch?v=<id>... / ...&v=<id>
    if let Some(rest) = s.split("v=").nth(1) {
        let id: String = rest.chars().take(11).collect();
        if id.len() == 11 {
            return Some(id);
        }
    }
    // .../shorts/<id>, .../embed/<id>
    for marker in ["/shorts/", "/embed/"] {
        if let Some(rest) = s.split(marker).nth(1) {
            let id: String = rest.chars().take(11).collect();
            if id.len() == 11 {
                return Some(id);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_video_ids() {
        assert_eq!(extract_video_id("dQw4w9WgXcQ").as_deref(), Some("dQw4w9WgXcQ"));
        assert_eq!(
            extract_video_id("https://www.youtube.com/watch?v=dQw4w9WgXcQ&t=10s").as_deref(),
            Some("dQw4w9WgXcQ")
        );
        assert_eq!(extract_video_id("https://youtu.be/dQw4w9WgXcQ").as_deref(), Some("dQw4w9WgXcQ"));
        assert_eq!(extract_video_id("not a video"), None);
    }

    #[test]
    fn parses_search_renderer() {
        let v = json!({
            "videoRenderer": {
                "videoId": "abc12345678",
                "title": { "runs": [ { "text": "Some " }, { "text": "Song" } ] },
                "ownerText": { "runs": [ { "text": "Artist" } ] },
                "lengthText": { "simpleText": "3:30" }
            }
        });
        let mut out = Vec::new();
        collect_video_renderers(&v, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].title, "Some Song");
        assert_eq!(out[0].author, "Artist");
        assert_eq!(out[0].length, 210_000);
        assert_eq!(out[0].source_name, "youtube");
    }

    #[test]
    fn picks_itag_140_with_direct_url() {
        let resp = json!({
            "videoDetails": { "title": "T", "author": "A", "lengthSeconds": "100", "isLiveContent": false },
            "streamingData": { "adaptiveFormats": [
                { "itag": 251, "mimeType": "audio/webm; codecs=\"opus\"", "url": "https://webm" },
                { "itag": 140, "mimeType": "audio/mp4; codecs=\"mp4a.40.2\"", "url": "https://m4a" }
            ]}
        });
        let (info, url) = parse_player(&resp, "vid12345678").unwrap();
        assert_eq!(url, "https://m4a");
        assert_eq!(info.length, 100_000);
    }
}
