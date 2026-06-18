//! 开发联调：单独起 core 的 web API（stub 运行时状态），供 web 前端连接调试，
//! 无需完整输入法服务（绕开 wind-ui 等 Windows-only 依赖）。
//!
//! 运行：
//!   WIND_DEV=1 cargo run -p wind-webapi --example dev_server
//! 启动后会打印一个含 port+token 的 URL（默认指向 vite dev http://localhost:5173）。
//! 浏览器打开该 URL 即可联调；或自定义：WIND_WEB_BASE=http://localhost:5173

use std::sync::Arc;

use wind_webapi::CoreStatus;

struct StubStatus;

impl CoreStatus for StubStatus {
    fn is_chinese_mode(&self) -> bool {
        true
    }
    fn active_schema_id(&self) -> String {
        "wubi86".to_string()
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    eprintln!("[dev] wind-webapi dev server starting (stub core status)...");
    wind_webapi::serve(Arc::new(StubStatus), "debug").await
}
