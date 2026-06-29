//! 共享内存管理（Host Render）
//!
//! 与 Go 版本 `wind_input/internal/bridge/shared_memory.go` 对齐。

/// 共享内存管理器
pub struct SharedMemoryManager {
    suffix: String,
    // TODO: mmap handle
}

impl SharedMemoryManager {
    pub fn new(suffix: &str) -> Self {
        Self {
            suffix: suffix.to_string(),
        }
    }

    /// 获取共享内存段名称
    pub fn shm_name(&self) -> String {
        format!("Local\\WindInput_SHM{}", self.suffix)
    }

    /// 获取 per-PID 事件名称
    pub fn event_name(&self, pid: u32) -> String {
        format!("Local\\WindInput_EVT_{}", pid)
    }
}
