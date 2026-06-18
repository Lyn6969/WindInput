//! `/local/*` 处理：仅 GUI 调用（同机非浏览器）。

use std::sync::Arc;

use axum::{Json, extract::State};
use serde_json::{Value, json};

use crate::session::WebState;

const DEFAULT_WEB_BASE: &str = "https://config.windinput.com";

/// 本机信息：版本/变体/连接态/端口（GUI「关于」用）。
pub async fn info(State(state): State<Arc<WebState>>) -> Json<Value> {
    Json(json!({
        "app": crate::APP_VERSION,
        "engine": crate::APP_VERSION,
        "variant": state.variant,
        "running": true,
        "activeSchema": state.status.active_schema_id(),
        "port": state.port(),
    }))
}

/// 开启网页配置：按需签发短时效 token，返回带 port/token 的 URL（GUI 据此开浏览器）。
pub async fn open(State(state): State<Arc<WebState>>) -> Json<Value> {
    let token = state.issue_token();
    let port = state.port();
    let base = std::env::var("WIND_WEB_BASE").unwrap_or_else(|_| DEFAULT_WEB_BASE.to_string());
    let url = format!("{}/?port={}&token={}", base, port, token);
    Json(json!({ "url": url, "port": port, "token": token }))
}

/// 关闭网页配置：撤销 token（即时收回 Web 访问）。
pub async fn close(State(state): State<Arc<WebState>>) -> Json<Value> {
    state.revoke_token();
    Json(json!({ "ok": true }))
}
