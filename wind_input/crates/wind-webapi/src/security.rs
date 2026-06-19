//! 安全中间件：/api/* 走 Origin 白名单 + token + CORS/PNA；/local/* 拒绝浏览器。

use std::sync::Arc;

use axum::{
    extract::{Request, State},
    http::{
        HeaderMap, HeaderName, HeaderValue, Method, StatusCode,
        header::{
            ACCESS_CONTROL_ALLOW_HEADERS, ACCESS_CONTROL_ALLOW_METHODS,
            ACCESS_CONTROL_ALLOW_ORIGIN, ACCESS_CONTROL_MAX_AGE, ORIGIN,
        },
    },
    middleware::Next,
    response::{IntoResponse, Response},
};

use crate::session::WebState;

const ALLOWED_ORIGINS: &[&str] = &["https://setting.windinput.com"];

pub(crate) fn is_allowed_origin(origin: &str) -> bool {
    if ALLOWED_ORIGINS.contains(&origin) {
        return true;
    }
    // 开发期放行本地 dev server
    origin.starts_with("http://localhost:") || origin.starts_with("http://127.0.0.1:")
}

fn add_cors(h: &mut HeaderMap, origin: Option<&str>) {
    if let Some(o) = origin {
        if is_allowed_origin(o) {
            if let Ok(v) = HeaderValue::from_str(o) {
                h.insert(ACCESS_CONTROL_ALLOW_ORIGIN, v);
            }
        }
    }
    h.insert(
        ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("POST, OPTIONS"),
    );
    h.insert(
        ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("content-type, x-windinput-token"),
    );
    h.insert(
        HeaderName::from_static("access-control-allow-private-network"),
        HeaderValue::from_static("true"),
    );
    h.insert(ACCESS_CONTROL_MAX_AGE, HeaderValue::from_static("600"));
}

/// `/api/*`：CORS/PNA 预检 + Origin 白名单 + 逐请求 token 校验。
pub async fn api_guard(State(state): State<Arc<WebState>>, req: Request, next: Next) -> Response {
    let origin = req
        .headers()
        .get(ORIGIN)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    if req.method() == Method::OPTIONS {
        let mut resp = StatusCode::NO_CONTENT.into_response();
        add_cors(resp.headers_mut(), origin.as_deref());
        return resp;
    }

    if !origin.as_deref().map(is_allowed_origin).unwrap_or(false) {
        return (StatusCode::FORBIDDEN, "forbidden: origin").into_response();
    }

    let token = req
        .headers()
        .get("x-windinput-token")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !state.check_token(token) {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }

    let mut resp = next.run(req).await;
    add_cors(resp.headers_mut(), origin.as_deref());
    resp
}

/// `/local/*`：带 Origin（即浏览器跨源）一律拒绝，仅放行本机非浏览器客户端（GUI）。
pub async fn local_guard(req: Request, next: Next) -> Response {
    if req.headers().contains_key(ORIGIN) {
        return (StatusCode::FORBIDDEN, "forbidden: local only").into_response();
    }
    next.run(req).await
}
