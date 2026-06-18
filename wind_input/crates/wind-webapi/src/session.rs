//! WebState：服务共享状态（清单缓存、端口、按需 Web token）。

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::{Duration, Instant};

use wind_config::Config;

use crate::CoreStatus;

/// Web token 有效期（短时效）。
const TOKEN_TTL: Duration = Duration::from_secs(3600);

struct TokenEntry {
    token: String,
    expires_at: Instant,
}

pub struct WebState {
    pub(crate) status: Arc<dyn CoreStatus>,
    pub(crate) variant: &'static str,
    pub(crate) manifest: serde_json::Value,
    port: AtomicU16,
    web_token: Mutex<Option<TokenEntry>>,
}

impl WebState {
    pub fn new(status: Arc<dyn CoreStatus>, variant: &'static str) -> anyhow::Result<Self> {
        let manifest = crate::manifest::load(variant)?;
        Ok(Self {
            status,
            variant,
            manifest,
            port: AtomicU16::new(0),
            web_token: Mutex::new(None),
        })
    }

    pub(crate) fn suffix(&self) -> &'static str {
        if self.variant == "debug" { "_debug" } else { "" }
    }

    pub(crate) fn port(&self) -> u16 {
        self.port.load(Ordering::Relaxed)
    }

    /// 端口确定后写 control{suffix}.json 供 GUI 发现。
    pub(crate) fn on_bound(&self, port: u16) -> anyhow::Result<()> {
        self.port.store(port, Ordering::Relaxed);
        if let Some(dir) = Config::local_dir() {
            std::fs::create_dir_all(&dir).ok();
            let file = dir.join(format!("control{}.json", self.suffix()));
            let body = serde_json::json!({
                "port": port,
                "pid": std::process::id(),
                "variant": self.variant,
            });
            std::fs::write(&file, serde_json::to_vec_pretty(&body)?)?;
        }
        Ok(())
    }

    /// 按需签发短时效 Web token（覆盖旧 token）。
    pub(crate) fn issue_token(&self) -> String {
        let token = uuid::Uuid::new_v4().to_string();
        *self.web_token.lock().unwrap() = Some(TokenEntry {
            token: token.clone(),
            expires_at: Instant::now() + TOKEN_TTL,
        });
        token
    }

    pub(crate) fn revoke_token(&self) {
        *self.web_token.lock().unwrap() = None;
    }

    pub(crate) fn check_token(&self, t: &str) -> bool {
        if t.is_empty() {
            return false;
        }
        match &*self.web_token.lock().unwrap() {
            Some(e) => e.token == t && Instant::now() < e.expires_at,
            None => false,
        }
    }
}
