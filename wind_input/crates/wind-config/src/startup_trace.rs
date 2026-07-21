//! 启动阶段轨迹：绕开 tracing，每次调用都同步落盘。
//!
//! 存在的理由是主日志本身可能失效。`tracing_appender::non_blocking` 把写入交给一个
//! worker 线程，该线程一旦出事，channel 断开后 lossy 模式会**静默丢弃**其后的每一条
//! 日志——`wind_input.log` 就永久停在某一行，而进程仍在正常运行。这种日志外观极易被
//! 误读成「进程卡在那一行」，实际进度可能远在其后。
//!
//! 本模块每次调用 open→write→flush→close，不经 tracing、不经缓冲、不常驻句柄，
//! 因此在主日志已死的场景下依然留痕。它只在启动路径与故障分支上被调用寥寥数次，
//! **不得进入按键热路径**。
//!
//! 每行都带 pid，因为「究竟起了几个进程」是这类故障的关键判据，而单看主日志答不了
//! ——被顶掉序号的日志文件会让多进程看起来像一次运行。
//!
//! 放在 wind-config 而非服务 crate，是为了让 wind-ui 等下层也能打点：UI 线程
//! 自己挂掉时，主线程与主日志都可能毫无察觉。

use std::io::Write;

/// 日志时间戳格式。与 `wind_tsf` 的 `FileLogger`(`_FormatTimestamp`) 逐字符一致，
/// 三份日志可直接归并排序。主日志的 timer 也应复用它，避免两处各写一份而漂移。
pub const LOG_TIME_FORMAT: &str = "%Y-%m-%d %H:%M:%S%.3f";

/// 轨迹文件大小上限，超过则清空重来。
///
/// 上限存在的目的**不是**控制体积——一次启动约 450 字节，一年 365 次开机也才 150KB。
/// 它防的是崩溃重启循环：服务若每秒重启数次，无限增长会失控。
///
/// 取值要足够大：故障是客户侧偶发的，日志往往隔几天才收集回来，期间的正常开机
/// 不能把那次复现的记录冲掉。1MB ≈ 2300 次启动，正常使用几年都摸不到。
const MAX_BYTES: u64 = 1024 * 1024;

fn trace_path() -> Option<std::path::PathBuf> {
    crate::config::Config::log_dir().map(|d| d.join("startup_stage.log"))
}

/// 记录一个启动/故障阶段。失败一律静默——诊断设施绝不能反过来影响启动。
pub fn stage(name: &str) {
    let Some(path) = trace_path() else { return };

    if std::fs::metadata(&path)
        .map(|m| m.len() > MAX_BYTES)
        .unwrap_or(false)
    {
        let _ = std::fs::remove_file(&path);
    }

    let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    else {
        return;
    };

    let _ = writeln!(
        f,
        "{} pid={} {}",
        chrono::Local::now().format(LOG_TIME_FORMAT),
        std::process::id(),
        name
    );
    let _ = f.flush();
}
