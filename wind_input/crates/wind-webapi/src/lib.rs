//! wind-webapi: core(wind_input) 内嵌的本地 HTTP 控制 / 配置 API。
//!
//! 端点分两层：
//! - `/local/*`：GUI 专用，拒绝浏览器（带 Origin 即拒绝）。
//! - `/api/rpc`：Web 数据端点，按需 token 授权 + Origin 白名单 + CORS/PNA。
//!
//! 仅监听 loopback（127.0.0.1:随机端口），端口写入 control{suffix}.json 供 GUI 发现。
//!
//! 不依赖 wind-coordinator/wind-ui：运行时状态经 [`CoreStatus`] trait 注入，
//! 使本 crate 可在任意平台独立编译/联调（见 `examples/dev_server.rs`）。

mod local;
mod manifest;
mod rpc;
mod security;
mod session;

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Query, State},
    middleware::{from_fn, from_fn_with_state},
    routing::{get, post},
};
use wind_ipc::rpc::Request;

pub use session::WebState;

pub(crate) const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// 由宿主（service）注入的运行时状态来源。解耦 wind-coordinator，使 web API 可独立编译。
pub trait CoreStatus: Send + Sync {
    fn is_chinese_mode(&self) -> bool;
    fn active_schema_id(&self) -> String;
    /// config.setItems 落盘后重新加载并即时应用用户配置；返回是否仍需重启才能完全生效。
    /// 默认实现保守返回 true（未接入热重载的宿主，如 dev server stub）。
    fn apply_config(&self) -> bool {
        true
    }
    /// 数据类 RPC（schema/dict/temp/freq/shadow/stats/theme/phrase）转发到宿主 core 实现。
    /// 默认未接入（dev server stub）：返回 unknown method 错误。
    fn data_rpc(&self, method: &str, _params: &serde_json::Value) -> anyhow::Result<serde_json::Value> {
        anyhow::bail!("unknown method: {}", method)
    }
}

/// 在调用方 tokio runtime 内启动 HTTP 服务（loopback）。一直 await 至服务结束。
pub async fn serve(status: Arc<dyn CoreStatus>, variant: &'static str) -> anyhow::Result<()> {
    serve_with_state(Arc::new(WebState::new(status, variant)?)).await
}

/// 用调用方预先构造的 [`WebState`] 启动服务，使宿主可共享同一句柄
/// （如「设置」菜单经 [`WebState::open_url`] 签发 token 开网页配置）。
pub async fn serve_with_state(state: Arc<WebState>) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
    let port = listener.local_addr()?.port();
    state.on_bound(port)?;
    tracing::info!("wind-webapi listening on http://127.0.0.1:{}", port);

    // 开发联调：WIND_DEV=1 时直接签发一个 token 并打印可用 URL（无需 GUI）。
    if std::env::var("WIND_DEV").is_ok() {
        let token = state.issue_token();
        let base =
            std::env::var("WIND_WEB_BASE").unwrap_or_else(|_| "http://localhost:5173".to_string());
        eprintln!("[wind-webapi dev] {}/?port={}&token={}", base, port, token);
    }

    let app = build_router(state);
    axum::serve(listener, app.into_make_service()).await?;
    Ok(())
}

fn build_router(state: Arc<WebState>) -> Router {
    let api = Router::new()
        .route("/api/rpc", post(api_rpc))
        .layer(from_fn_with_state(state.clone(), security::api_guard));
    // SSE 事件流：EventSource 不能设自定义 header，故 token 走 query；handler 内自管 Origin/token/CORS。
    let events = Router::new().route("/api/events", get(api_events));
    let local = Router::new()
        .route("/local/info", get(local::info))
        .route("/local/web-config/open", post(local::open))
        .route("/local/web-config/close", post(local::close))
        .layer(from_fn(security::local_guard));
    Router::new().merge(api).merge(events).merge(local).with_state(state)
}

/// `/api/events`：SSE 事件流（query token 鉴权 + Origin 白名单 + CORS）。
/// 当前无事件源，仅维持长连接 + 周期 keepalive，避免前端 EventSource 重连风暴。
async fn api_events(
    State(state): State<Arc<WebState>>,
    headers: axum::http::HeaderMap,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> axum::response::Response {
    use axum::http::{
        HeaderValue, StatusCode,
        header::{ACCESS_CONTROL_ALLOW_ORIGIN, ORIGIN},
    };
    use axum::response::{
        IntoResponse,
        sse::{Event, KeepAlive, Sse},
    };

    let origin = headers
        .get(ORIGIN)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    if !origin.as_deref().map(security::is_allowed_origin).unwrap_or(false) {
        return (StatusCode::FORBIDDEN, "forbidden: origin").into_response();
    }
    if !state.check_token(q.get("token").map(|s| s.as_str()).unwrap_or("")) {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }

    let stream = futures::stream::pending::<Result<Event, std::convert::Infallible>>();
    let mut resp = Sse::new(stream).keep_alive(KeepAlive::default()).into_response();
    if let Some(o) = origin {
        if let Ok(v) = HeaderValue::from_str(&o) {
            resp.headers_mut().insert(ACCESS_CONTROL_ALLOW_ORIGIN, v);
        }
    }
    resp
}

async fn api_rpc(
    State(state): State<Arc<WebState>>,
    Json(req): Json<Request>,
) -> Json<wind_ipc::rpc::Response> {
    tracing::info!("/api/rpc method={}", req.method);
    if std::env::var("WIND_DEV").is_ok() {
        eprintln!("[api/rpc] <- {}", req.method);
    }
    Json(rpc::dispatch(&state, req).await)
}

#[cfg(test)]
mod tests {
    //! 契约测试：用真实 router + stub CoreStatus 断言 core 输出与 web 期望的形状/安全行为一致。
    //! 不绑端口、不依赖浏览器，可在任意平台 CI 运行。
    use super::*;
    use axum::body::{Body, to_bytes};
    use axum::http::{StatusCode, header};
    use tower::ServiceExt;

    struct StubStatus;
    impl CoreStatus for StubStatus {
        fn is_chinese_mode(&self) -> bool {
            true
        }
        fn active_schema_id(&self) -> String {
            "wubi86".to_string()
        }
    }

    fn state() -> Arc<WebState> {
        Arc::new(WebState::new(Arc::new(StubStatus), "debug").expect("manifest 应能加载"))
    }

    const DEV_ORIGIN: &str = "http://localhost:5173";

    /// 经 /api/rpc 调一个方法，返回 (HTTP 状态码, 响应体 JSON)。
    async fn rpc(
        st: Arc<WebState>,
        token: Option<&str>,
        origin: Option<&str>,
        method: &str,
    ) -> (StatusCode, serde_json::Value) {
        let mut rb = axum::http::Request::builder()
            .method("POST")
            .uri("/api/rpc")
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(o) = origin {
            rb = rb.header(header::ORIGIN, o);
        }
        if let Some(t) = token {
            rb = rb.header("x-windinput-token", t);
        }
        let body = format!(
            r#"{{"version":1,"id":1,"method":"{}","params":{{}}}}"#,
            method
        );
        let resp = build_router(st)
            .oneshot(rb.body(Body::from(body)).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, json)
    }

    #[tokio::test]
    async fn manifest_shape_matches_web() {
        let st = state();
        let tok = st.issue_token();
        let (code, body) = rpc(st, Some(&tok), Some(DEV_ORIGIN), "system.manifest").await;
        assert_eq!(code, StatusCode::OK);
        let r = &body["result"];
        for k in ["manifest", "app", "engine", "variant", "groups", "items", "features"] {
            assert!(r.get(k).is_some(), "manifest 缺字段 {k}");
        }
        assert!(r["items"].is_array() && r["groups"].is_array());
    }

    #[tokio::test]
    async fn system_info_matches_web_shape() {
        // 回归：曾经 core 返回 app/engine/variant，与 web 的 {version,platform,dataDir,running} 全不匹配。
        let st = state();
        let tok = st.issue_token();
        let (code, body) = rpc(st, Some(&tok), Some(DEV_ORIGIN), "system.info").await;
        assert_eq!(code, StatusCode::OK);
        let r = &body["result"];
        for k in ["version", "platform", "dataDir", "running"] {
            assert!(r.get(k).is_some(), "system.info 缺 web 字段 {k}");
        }
    }

    #[tokio::test]
    async fn system_status_shape() {
        let st = state();
        let tok = st.issue_token();
        let (_c, body) = rpc(st, Some(&tok), Some(DEV_ORIGIN), "system.status").await;
        assert!(body["result"]["running"].is_boolean());
        assert!(body["result"].get("mode").is_some());
    }

    #[tokio::test]
    async fn config_get_and_defaults_are_objects() {
        let st = state();
        let tok = st.issue_token();
        let (c1, b1) = rpc(st.clone(), Some(&tok), Some(DEV_ORIGIN), "config.get").await;
        assert_eq!(c1, StatusCode::OK);
        assert!(b1["result"].is_object());
        let (c2, b2) = rpc(st, Some(&tok), Some(DEV_ORIGIN), "config.getDefaults").await;
        assert_eq!(c2, StatusCode::OK);
        assert!(
            b2["result"]["input"].is_object(),
            "默认配置应含 input 段"
        );
    }

    #[tokio::test]
    async fn rejects_missing_token() {
        let st = state();
        let (code, _) = rpc(st, None, Some(DEV_ORIGIN), "system.status").await;
        assert_eq!(code, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn rejects_bad_origin() {
        let st = state();
        let tok = st.issue_token();
        let (code, _) = rpc(st, Some(&tok), Some("http://evil.example"), "system.status").await;
        assert_eq!(code, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn unknown_method_returns_rpc_error() {
        let st = state();
        let tok = st.issue_token();
        let (code, body) = rpc(st, Some(&tok), Some(DEV_ORIGIN), "bogus.method").await;
        assert_eq!(code, StatusCode::OK); // RPC 层错误以 200 + error 字段返回
        assert!(body["error"].is_string());
    }

    #[tokio::test]
    async fn preflight_has_cors_and_pna() {
        let st = state();
        let req = axum::http::Request::builder()
            .method("OPTIONS")
            .uri("/api/rpc")
            .header(header::ORIGIN, DEV_ORIGIN)
            .body(Body::empty())
            .unwrap();
        let resp = build_router(st).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            resp.headers()
                .get("access-control-allow-private-network")
                .and_then(|v| v.to_str().ok()),
            Some("true")
        );
    }

    #[tokio::test]
    async fn local_endpoint_blocks_browser() {
        let st = state();
        // 浏览器（带 Origin）→ 403
        let req = axum::http::Request::builder()
            .method("GET")
            .uri("/local/info")
            .header(header::ORIGIN, DEV_ORIGIN)
            .body(Body::empty())
            .unwrap();
        let resp = build_router(st.clone()).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        // GUI（无 Origin）→ 200
        let req2 = axum::http::Request::builder()
            .method("GET")
            .uri("/local/info")
            .body(Body::empty())
            .unwrap();
        let resp2 = build_router(st).oneshot(req2).await.unwrap();
        assert_eq!(resp2.status(), StatusCode::OK);
    }
}
