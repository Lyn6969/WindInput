//! [`WebDataHost`]：设置页数据 RPC（wind-webdata crate）消费宿主能力的窄面。
//!
//! RPC 本体在 `wind-webdata`（trait `WebDataRpc: WebDataHost` 的默认方法）；
//! 依赖方向是 wind-webdata → wind-coordinator，本 crate 不依赖 wind-transfer/fontdb，
//! Android 闭包（不含 wind-webdata）因此无任何 C 依赖。

use std::path::Path;
use std::sync::{Arc, RwLock};

use wind_engine::EngineManager;
use wind_reverse::ReverseLookup;
use wind_store::Store;
use wind_store::stat_collector::StatCollector;

use crate::coordinator::Coordinator;

/// webdata 消费宿主能力的**窄面**：设置页数据 RPC 对 Coordinator 的全部依赖收敛于此。
///
/// ★ webdata 不碰输入态（`State` 与 Coordinator 的 80 余个字段），只消费引擎/存储/
/// 统计/主题句柄与少数重建入口——新增 RPC 若需新依赖，**必须加在本 trait 上**，
/// 勿在默认方法里绕道取宿主其它状态（那会无声地把窄面重新擑宽，也阻断后续
/// 独立成 crate 的路）。
///
/// 方法与 Coordinator 固有方法同名时固有优先；转发 impl 内一律用完全限定路径
/// `Coordinator::xxx(self)` 消歧，否则就是自调递归。
pub trait WebDataHost {
    fn engine_mgr(&self) -> &EngineManager;
    fn user_store(&self) -> Option<&Arc<Store>>;
    fn stat_collector(&self) -> Option<&StatCollector>;
    fn reverse_lookup(&self) -> &RwLock<ReverseLookup>;
    fn themes_dir(&self) -> Option<&Path>;
    fn rebuild_phrases(&self);
    fn restore_missing_system_phrases(&self, reason: &str);
    fn restore_system_phrases(&self) -> usize;
    fn sync_comment_dicts(&self);
    fn sync_chaizi_assets(&self);
    fn reload_user_config(&self) -> bool;
    fn push_theme(&self, name: &str, is_dark: bool);
    fn theme_search_dirs(&self) -> Vec<std::path::PathBuf>;
    fn list_themes_full(&self) -> Vec<(String, String, bool)>;
    /// 当前生效主题 id（快照）。
    fn current_theme_name(&self) -> String;
    /// 当前明暗（system 档按系统实时判定）。语义方法而非暴露 `Mutex<ThemeStyle>`：
    /// 窄面签名不携带宿主内部类型与锁形态。
    fn current_theme_is_dark(&self) -> bool;
}

impl WebDataHost for Coordinator {
    fn engine_mgr(&self) -> &EngineManager {
        &self.engine_mgr
    }
    fn user_store(&self) -> Option<&Arc<Store>> {
        self.store.as_ref()
    }
    fn stat_collector(&self) -> Option<&StatCollector> {
        self.stat_collector.as_ref()
    }
    fn reverse_lookup(&self) -> &RwLock<ReverseLookup> {
        &self.reverse
    }
    fn themes_dir(&self) -> Option<&Path> {
        self.themes_dir.as_deref()
    }
    fn rebuild_phrases(&self) {
        Coordinator::rebuild_phrases(self);
    }
    fn restore_missing_system_phrases(&self, reason: &str) {
        Coordinator::restore_missing_system_phrases(self, reason);
    }
    fn restore_system_phrases(&self) -> usize {
        Coordinator::restore_system_phrases(self)
    }
    fn sync_comment_dicts(&self) {
        Coordinator::sync_comment_dicts(self);
    }
    fn sync_chaizi_assets(&self) {
        Coordinator::sync_chaizi_assets(self);
    }
    fn reload_user_config(&self) -> bool {
        Coordinator::reload_user_config(self)
    }
    fn push_theme(&self, name: &str, is_dark: bool) {
        Coordinator::push_theme(self, name, is_dark);
    }
    fn theme_search_dirs(&self) -> Vec<std::path::PathBuf> {
        Coordinator::theme_search_dirs(self)
    }
    fn list_themes_full(&self) -> Vec<(String, String, bool)> {
        Coordinator::list_themes_full(self)
    }
    fn current_theme_name(&self) -> String {
        self.theme_name
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    fn current_theme_is_dark(&self) -> bool {
        self.theme_style
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .resolve_dark()
    }
}
