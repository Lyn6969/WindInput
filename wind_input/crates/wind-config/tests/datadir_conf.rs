//! `datadir.conf` 的端到端接线：配置文件 → `Config::user_config_dir()`。
//!
//! 为什么必须是集成测试（独立进程）：`variant::custom_userdata_dir()` 用 OnceLock
//! 缓存，进程内只解析一次盘上状态，同一进程里测不了第二种取值。
//!
//! 这条链此前**整段缺失**——安装器按向导选择写下 `datadir.conf`，主程序从不读它，
//! 用户选的目录形同虚设、卸载器却按它删数据。单测只能证明解析函数对，证明不了
//! `user_config_dir()` 真的走了这条分支，故必须有这一个真调公开 API 的测试。

use wind_config::Config;
use wind_config::config::UserConfigProbe;

#[test]
fn datadir_conf_redirects_user_config_dir() {
    let tmp = std::env::temp_dir().join("wind_datadir_conf_e2e");
    let target = tmp.join("MyUserData");
    let conf = tmp.join("datadir.conf");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    // 安装器写的就是一行裸路径（`write_datadir_conf` 直接写 to_string_lossy）。
    std::fs::write(&conf, target.to_string_lossy().as_bytes()).unwrap();

    // SAFETY: 本文件仅此一个测试，env 在任何 OnceLock 初始化之前设置，无并发读者。
    unsafe { std::env::set_var("WIND_DATADIR_CONF", &conf) };

    assert_eq!(
        Config::user_config_dir(),
        Some(target.clone()),
        "user_config_dir 必须落到 datadir.conf 指定的目录"
    );

    // 安装器只写配置不建目录，首次启动时目标目录通常还不存在——必须由读端建出来，
    // 否则配置与词库全部读写失败。
    assert!(target.is_dir(), "自定义目录应被创建");

    // local_dir 系（cache / logs / state.toml）刻意不跟随：与卸载器的语义对齐
    // （cleanup.rs 的 local_cache_dir() 恒为 %LOCALAPPDATA%\{id}\cache，从不读 conf），
    // 且让 C++ FileLogger 的硬编码日志路径无需改动、两份日志仍能按时间对齐。
    let local = Config::local_dir().expect("local_dir");
    assert!(
        !local.starts_with(&target),
        "local_dir 不得跟随自定义数据目录，实际: {}",
        local.display()
    );

    // 自定义目录不经漫游 known folder，必须判为恒就绪；否则每次启动都白等一个
    // 完整超时后退回系统预置方案。
    let probe = Config::probe_user_config();
    assert!(
        matches!(&probe, UserConfigProbe::CustomDir(d) if d == &target),
        "probe 应报 CustomDir，实际: {probe:?}"
    );
    assert!(probe.is_settled(), "自定义目录必须恒就绪");

    let _ = std::fs::remove_dir_all(&tmp);
}
