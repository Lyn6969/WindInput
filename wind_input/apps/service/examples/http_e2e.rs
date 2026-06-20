//! Linux E2E 验证：真实 headless Coordinator + wind-webapi HTTP 服务，供 curl 跑数据域 RPC。
//!
//! 仅用于本机联调（非生产）：用临时 redb store + build_debug/data，跳过 TSF/UI。
//! 用法：
//!   WIND_DEV=1 cargo run -p wind_service --example http_e2e -- [data_dir]
//! 启动后 stderr 打印 `[wind-webapi dev] http://.../?port=PORT&token=TOKEN`，
//! 用该 PORT/TOKEN curl `http://127.0.0.1:PORT/api/rpc`（带 token 头 + Origin）。

use std::path::PathBuf;
use std::sync::Arc;

struct WebStatus(Arc<wind_coordinator::Coordinator>);

impl wind_webapi::CoreStatus for WebStatus {
    fn is_chinese_mode(&self) -> bool {
        self.0.is_chinese_mode()
    }
    fn active_schema_id(&self) -> String {
        self.0.active_schema_id()
    }
    fn data_rpc(
        &self,
        method: &str,
        params: &serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        self.0.web_data_rpc(method, params)
    }
    fn fonts(&self) -> Vec<String> {
        self.0.list_font_families()
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .init();

    let data_dir = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("wind_input/build_debug/data"));
    eprintln!("[e2e] data_dir = {}", data_dir.display());

    let store_path = std::env::temp_dir().join("wind_http_e2e.redb");
    let _ = std::fs::remove_file(&store_path);
    let store = Arc::new(wind_store::Store::open(&store_path)?);

    // 从 data_dir 载入 config（含 active schema）；失败回退默认。
    let cfg = wind_config::Config::load(Some(&data_dir)).unwrap_or_default();
    let coord = wind_coordinator::Coordinator::new_headless_with_store(cfg, Some(&data_dir), store);

    let status: Arc<dyn wind_webapi::CoreStatus> = Arc::new(WebStatus(coord));
    // serve 在 WIND_DEV=1 时打印可用 URL（port+token）。一直 await。
    wind_webapi::serve(status, "debug").await
}
