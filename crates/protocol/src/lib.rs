//! Lavalink v4 互換の REST / WebSocket DTO 定義（チケット 2-1, 2-6）。
//!
//! 全フィールドは Lavalink の camelCase と一致（`#[serde(rename_all = "camelCase")]`）。
//! `PATCH player` の三状態（未指定 / null / 値）は `Option<Option<T>>` + [`double_option`] で表現する。

#![forbid(unsafe_code)]

pub mod types;
pub mod ws;

pub use types::*;
pub use ws::*;

use serde::{Deserialize, Deserializer};

/// 空の JSON オブジェクト `{}`（pluginInfo / userData の既定値）。
pub fn empty_object() -> serde_json::Value {
    serde_json::Value::Object(serde_json::Map::new())
}

/// 三状態フィールド用デシリアライザ。
/// 未指定 → `None`（`#[serde(default)]` 経由）、`null` → `Some(None)`、値 → `Some(Some(v))`。
pub fn double_option<'de, T, D>(de: D) -> Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: Deserializer<'de>,
{
    Deserialize::deserialize(de).map(Some)
}
