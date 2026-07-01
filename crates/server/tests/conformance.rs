//! 公式 Lavalink v4 とのレスポンス差分テスト（チケット 6-2 / 6-3）。
//!
//! - in-process で HTTP を叩き（`tower::ServiceExt::oneshot`）、レスポンス JSON を
//!   公式仕様由来の golden とフィールド単位で突き合わせる。
//! - WebSocket の op / event は serde のワイヤ出力を公式 docs の形と直接比較し、
//!   入れ子内部タグ（`op` → `type`）と各 `rename_all` の正しさを検証する。
//!
//! 値レベルで実 YouTube データと突き合わせる差分（loadtracks の実値等）は、
//! `scripts/capture_golden.sh` で実 Lavalink から golden を取得して行う（フェーズ5 以降）。
//! 既知の保留差分は `docs/conformance.md` 参照（例: `info.artworkUrl` は YouTube source 実装待ち）。

use axum::{
    body::Body,
    http::{Request, StatusCode},
    Router,
};
use serde_json::{json, Value};
use tower::ServiceExt;

use lavalink_protocol::{
    Cpu, Event, Memory, PlayerState, ServerMessage, Stats, Track, TrackEndReason, TrackInfo,
};
use lavalink_server::{build_app, config::AppConfig, AppState, SharedState};

const PW: &str = "youshallnotpass";
const SAMPLE: &str = "QAAAjQIAJVJpY2sgQXN0bGV5IC0gTmV2ZXIgR29ubmEgR2l2ZSBZb3UgVXAADlJpY2tBc3RsZXlWRVZPAAAAAAADPCAAC2RRdzR3OVdnWGNRAAEAK2h0dHBzOi8vd3d3LnlvdXR1YmUuY29tL3dhdGNoP3Y9ZFF3NHc5V2dYY1EAB3lvdXR1YmUAAAAAAAAAAA==";

// ----------------------------- harness -----------------------------

fn app_and_state() -> (Router, SharedState) {
    let state = AppState::new(AppConfig::default());
    (build_app(state.clone()), state)
}

fn app() -> Router {
    app_and_state().0
}

async fn send(app: &Router, method: &str, uri: &str, body: Option<&str>) -> (StatusCode, Value) {
    send_with_auth(app, method, uri, body, Some(PW)).await
}

async fn send_with_auth(
    app: &Router,
    method: &str,
    uri: &str,
    body: Option<&str>,
    auth: Option<&str>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(a) = auth {
        builder = builder.header("authorization", a);
    }
    let req = match body {
        Some(s) => builder
            .header("content-type", "application/json")
            .body(Body::from(s.to_owned()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    };
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
    let json = if bytes.is_empty() {
        Value::Null
    } else {
        // /version 等はプレーンテキストを返すため、JSON でなければ文字列として扱う。
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()))
    };
    (status, json)
}

/// `expected` と `actual` を再帰的に比較し、差分パスの一覧を返す。
/// `ignore` に含まれる JSON パス（例 `"info.artworkUrl"`, `"[0].info.artworkUrl"`）は無視する。
fn json_diff(expected: &Value, actual: &Value, ignore: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    walk(expected, actual, "", ignore, &mut out);
    out
}

fn walk(expected: &Value, actual: &Value, path: &str, ignore: &[&str], out: &mut Vec<String>) {
    if ignore.contains(&path) {
        return;
    }
    match (expected, actual) {
        (Value::Object(e), Value::Object(a)) => {
            for (k, ev) in e {
                let p = if path.is_empty() { k.clone() } else { format!("{path}.{k}") };
                match a.get(k) {
                    Some(av) => walk(ev, av, &p, ignore, out),
                    None if !ignore.contains(&p.as_str()) => out.push(format!("missing key: {p}")),
                    None => {}
                }
            }
            for k in a.keys() {
                if !e.contains_key(k) {
                    let p = if path.is_empty() { k.clone() } else { format!("{path}.{k}") };
                    if !ignore.contains(&p.as_str()) {
                        out.push(format!("unexpected key: {p}"));
                    }
                }
            }
        }
        (Value::Array(e), Value::Array(a)) => {
            if e.len() != a.len() {
                out.push(format!("array length at '{path}': {} vs {}", e.len(), a.len()));
            }
            for (i, (ev, av)) in e.iter().zip(a.iter()).enumerate() {
                walk(ev, av, &format!("{path}[{i}]"), ignore, out);
            }
        }
        (e, a) if e != a => out.push(format!("value at '{path}': {e} vs {a}")),
        _ => {}
    }
}

fn sample_track() -> Track {
    Track::new(
        SAMPLE.to_string(),
        TrackInfo {
            identifier: "dQw4w9WgXcQ".into(),
            is_seekable: true,
            author: "RickAstleyVEVO".into(),
            length: 212_000,
            is_stream: false,
            position: 0,
            title: "Rick Astley - Never Gonna Give You Up".into(),
            uri: Some("https://www.youtube.com/watch?v=dQw4w9WgXcQ".into()),
            artwork_url: None,
            isrc: None,
            source_name: "youtube".into(),
        },
    )
}

// ----------------------------- REST tests -----------------------------

#[tokio::test]
async fn version_endpoint_is_open_and_returns_semver() {
    let (status, _) = send_with_auth(&app(), "GET", "/version", None, None).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn decodetracks_matches_official_golden() {
    let body = json!([SAMPLE]).to_string();
    let (status, actual) = send(&app(), "POST", "/v4/decodetracks", Some(&body)).await;
    assert_eq!(status, StatusCode::OK);

    let expected: Value =
        serde_json::from_str(include_str!("golden/decodetracks_sample.json")).unwrap();
    // info.artworkUrl は公式が YouTube source で識別子から再構築するフィールドで、
    // 本実装の汎用コーデックは null を返す（フェーズ5 で対応）。既知差分として無視。
    let diffs = json_diff(&expected, &actual, &["[0].info.artworkUrl"]);
    assert!(diffs.is_empty(), "decodetracks diffs: {diffs:#?}");
}

#[tokio::test]
async fn decodetrack_get_works_with_url_encoded_param() {
    let enc = SAMPLE.replace('+', "%2B").replace('/', "%2F").replace('=', "%3D");
    let (status, actual) =
        send(&app(), "GET", &format!("/v4/decodetrack?encodedTrack={enc}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(actual["info"]["identifier"], json!("dQw4w9WgXcQ"));
    assert_eq!(actual["info"]["sourceName"], json!("youtube"));
}

#[tokio::test]
async fn loadtracks_http_url_returns_track() {
    // http ソースはネットワーク無しで track を返す（hermetic）。
    let (status, v) =
        send(&app(), "GET", "/v4/loadtracks?identifier=https://example.com/song.m4a", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["loadType"], json!("track"));
    assert_track_schema(&v["data"]);
    assert_eq!(v["data"]["info"]["sourceName"], json!("http"));
}

#[tokio::test]
async fn loadtracks_unknown_identifier_returns_empty() {
    // 検索プレフィックス無し・URL でも 11 文字 ID でもない → empty（ネットワーク不要）。
    let (status, v) =
        send(&app(), "GET", "/v4/loadtracks?identifier=just%20some%20words", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["loadType"], json!("empty"));
}

/// Track オブジェクトが Lavalink v4 のキー構成を持つか（値ではなくスキーマ検証）。
fn assert_track_schema(t: &Value) {
    for k in ["encoded", "info", "pluginInfo", "userData"] {
        assert!(t.get(k).is_some(), "track missing key: {k}");
    }
    let info = &t["info"];
    for k in [
        "identifier", "isSeekable", "author", "length", "isStream", "position", "title", "uri",
        "artworkUrl", "isrc", "sourceName",
    ] {
        assert!(info.get(k).is_some(), "info missing key: {k}");
    }
    assert!(info["length"].is_number());
    assert!(info["isSeekable"].is_boolean());
}

#[tokio::test]
async fn unknown_session_error_matches_official_shape() {
    let (status, actual) = send(
        &app(),
        "GET",
        "/v4/sessions/does-not-exist/players/123",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let expected: Value =
        serde_json::from_str(include_str!("golden/error_session_not_found.json")).unwrap();
    // timestamp は実行時刻なので無視。
    let diffs = json_diff(&expected, &actual, &["timestamp"]);
    assert!(diffs.is_empty(), "error shape diffs: {diffs:#?}");
}

#[tokio::test]
async fn missing_auth_is_rejected_with_lavalink_error() {
    let (status, v) = send_with_auth(&app(), "GET", "/v4/info", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    for k in ["timestamp", "status", "error", "message", "path"] {
        assert!(v.get(k).is_some(), "error missing key: {k}");
    }
    assert_eq!(v["status"], json!(401));
}

#[tokio::test]
async fn update_player_returns_v4_player_shape() {
    let (app, state) = app_and_state();
    let sess = state.create_session("42".into()).await;
    let sid = sess.id.clone();
    let guild = "1425444129260703777";

    let body = json!({ "track": { "encoded": SAMPLE } }).to_string();
    let (status, player) = send(
        &app,
        "PATCH",
        &format!("/v4/sessions/{sid}/players/{guild}"),
        Some(&body),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let expected = json!({
        "guildId": guild,
        "track": {
            "encoded": SAMPLE,
            "info": {
                "identifier": "dQw4w9WgXcQ",
                "isSeekable": true,
                "author": "RickAstleyVEVO",
                "length": 212000,
                "isStream": false,
                "position": 0,
                "title": "Rick Astley - Never Gonna Give You Up",
                "uri": "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
                "artworkUrl": null,
                "isrc": null,
                "sourceName": "youtube"
            },
            "pluginInfo": {},
            "userData": {}
        },
        "volume": 100,
        "paused": false,
        "state": { "time": 0, "position": 0, "connected": false, "ping": -1 },
        "voice": { "token": "", "endpoint": "", "sessionId": "" },
        "filters": {}
    });
    // state.time / state.position は実行時依存なので無視。
    let diffs = json_diff(&expected, &player, &["state.time", "state.position"]);
    assert!(diffs.is_empty(), "player shape diffs: {diffs:#?}");
}

// ----------------------------- WebSocket wire-format tests -----------------------------
// 公式 docs の op / event JSON と serde 出力を直接突き合わせ、入れ子内部タグと rename を検証する。

fn ser(msg: &ServerMessage) -> Value {
    serde_json::to_value(msg).unwrap()
}

#[tokio::test]
async fn ws_ready_op_matches_docs() {
    let actual = ser(&ServerMessage::Ready {
        resumed: false,
        session_id: "abc".into(),
    });
    let expected = json!({ "op": "ready", "resumed": false, "sessionId": "abc" });
    assert!(json_diff(&expected, &actual, &[]).is_empty(), "{actual}");
}

#[tokio::test]
async fn ws_player_update_op_matches_docs() {
    let actual = ser(&ServerMessage::PlayerUpdate {
        guild_id: "123".into(),
        state: PlayerState { time: 1500467109, position: 60000, connected: true, ping: 50 },
    });
    let expected = json!({
        "op": "playerUpdate",
        "guildId": "123",
        "state": { "time": 1500467109, "position": 60000, "connected": true, "ping": 50 }
    });
    assert!(json_diff(&expected, &actual, &[]).is_empty(), "{actual}");
}

#[tokio::test]
async fn ws_stats_op_matches_docs() {
    let actual = ser(&ServerMessage::Stats(Stats {
        players: 1,
        playing_players: 1,
        uptime: 123456789,
        memory: Memory { free: 1, used: 2, allocated: 3, reservable: 4 },
        cpu: Cpu { cores: 4, system_load: 0.5, lavalink_load: 0.5 },
        frame_stats: None,
    }));
    let expected = json!({
        "op": "stats",
        "players": 1,
        "playingPlayers": 1,
        "uptime": 123456789,
        "memory": { "free": 1, "used": 2, "allocated": 3, "reservable": 4 },
        "cpu": { "cores": 4, "systemLoad": 0.5, "lavalinkLoad": 0.5 }
    });
    assert!(json_diff(&expected, &actual, &[]).is_empty(), "{actual}");
}

#[tokio::test]
async fn ws_track_start_event_matches_docs() {
    let actual = ser(&ServerMessage::Event(Event::TrackStart {
        guild_id: "123".into(),
        track: sample_track(),
    }));
    let expected = json!({
        "op": "event",
        "type": "TrackStartEvent",
        "guildId": "123",
        "track": serde_json::to_value(sample_track()).unwrap()
    });
    assert!(json_diff(&expected, &actual, &[]).is_empty(), "{actual}");
}

#[tokio::test]
async fn ws_track_end_event_matches_docs() {
    let actual = ser(&ServerMessage::Event(Event::TrackEnd {
        guild_id: "123".into(),
        track: sample_track(),
        reason: TrackEndReason::Finished,
    }));
    assert_eq!(actual["op"], json!("event"));
    assert_eq!(actual["type"], json!("TrackEndEvent"));
    assert_eq!(actual["reason"], json!("finished"));
    assert_eq!(actual["guildId"], json!("123"));
}

#[tokio::test]
async fn ws_websocket_closed_event_matches_docs() {
    let actual = ser(&ServerMessage::Event(Event::WebSocketClosed {
        guild_id: "123".into(),
        code: 4006,
        reason: "Your session is no longer valid.".into(),
        by_remote: true,
    }));
    let expected = json!({
        "op": "event",
        "type": "WebSocketClosedEvent",
        "guildId": "123",
        "code": 4006,
        "reason": "Your session is no longer valid.",
        "byRemote": true
    });
    assert!(json_diff(&expected, &actual, &[]).is_empty(), "{actual}");
}
