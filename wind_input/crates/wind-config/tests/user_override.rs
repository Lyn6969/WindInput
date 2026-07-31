//! 用户覆盖层的端到端接线：`用户配置目录/<rel>` 整体替代 `data_dir/<rel>`。
//!
//! 为什么必须是集成测试（独立进程）：解析真调 `Config::user_config_dir()`，而它经
//! `variant::custom_userdata_dir()` 的 OnceLock 缓存——同一进程里只解析一次盘上状态。
//! 用 `WIND_DATADIR_CONF` 把用户目录重定向到临时目录（与 `datadir_conf.rs` 同一杠杆）。
//!
//! 纯函数单测在这里证明不了什么：这套机制历史上的缺陷全部是**接线**缺陷——解析函数
//! 本身没错，错在调用方绕过它直接拼 `data_dir`（`common_chars.txt` / `pinyin_map.txt` /
//! `unigram_path` 都栽在这上面）。故本测试只走公开 API。
//!
//! ⚠️ 全文件仅此一个 `#[test]`：多个测试在同一二进制里并行会争抢 `WIND_DATADIR_CONF`
//! 与 OnceLock，先跑的那个会把用户目录定死，后跑的静默测到错误的目标。

use std::path::Path;
use wind_config::Config;

/// 在 `dir/rel` 写一个带内容的文件（父目录按需创建）。
fn write_at(dir: &Path, rel: &str, body: &str) -> std::path::PathBuf {
    let p = dir.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, body).unwrap();
    p
}

#[test]
fn user_dir_overrides_installed_data_files() {
    let tmp = std::env::temp_dir().join("wind_user_override_e2e");
    let user = tmp.join("UserData");
    let data = tmp.join("install/data");
    let conf = tmp.join("datadir.conf");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    std::fs::write(&conf, user.to_string_lossy().as_bytes()).unwrap();

    // SAFETY: 本文件仅此一个测试，env 在任何 OnceLock 初始化之前设置，无并发读者。
    unsafe { std::env::set_var("WIND_DATADIR_CONF", &conf) };
    assert_eq!(
        Config::user_config_dir(),
        Some(user.clone()),
        "前置条件：用户目录须已重定向到临时目录，否则本测试会去读真实 %APPDATA%"
    );

    // ── 数据根文件（system.phrases.toml / pinyin_map.txt 这一类）───────────────

    // 1) 两侧同名 → 用户胜出（这就是「覆盖」）
    write_at(&data, "system.phrases.toml", "sys");
    let user_phrases = write_at(&user, "system.phrases.toml", "user");
    assert_eq!(
        Config::resolve_data_file(Some(&data), "system.phrases.toml"),
        Some(user_phrases),
        "用户目录同名文件必须整体替代安装目录那份"
    );

    // 2) 仅安装侧有 → 回落安装目录（绝大多数用户的日常情形）
    let sys_pinyin_map = write_at(&data, "pinyin_map.txt", "sys");
    assert_eq!(
        Config::resolve_data_file(Some(&data), "pinyin_map.txt"),
        Some(sys_pinyin_map.clone()),
        "用户未覆盖时必须回落安装目录"
    );

    // 3) 两侧皆无 → None（调用方据此告警，而不是拿着不存在的路径去 open）
    assert_eq!(
        Config::resolve_data_file(Some(&data), "nonexistent.toml"),
        None,
        "两处均不存在必须返回 None"
    );

    // 4) 空 rel 不得退化成「返回目录本身」
    assert_eq!(Config::resolve_data_file(Some(&data), ""), None);

    // 5) 无 data_dir（headless）时仍认用户覆盖——否则测试/便携场景下用户配置形同虚设
    assert_eq!(
        Config::resolve_data_file(None, "system.phrases.toml"),
        Some(user.join("system.phrases.toml")),
        "data_dir 缺席时用户层仍须生效"
    );

    // ── schemas/ 下的方案附属资源（common_chars.txt / 拆字库 / 字根字体）────────

    let sys_common = write_at(&data, "schemas/common_chars.txt", "sys");
    assert_eq!(
        Config::resolve_schema_resource(Some(&data), "common_chars.txt"),
        Some(sys_common),
        "未覆盖时回落安装目录 schemas/"
    );
    let user_common = write_at(&user, "schemas/common_chars.txt", "user");
    assert_eq!(
        Config::resolve_schema_resource(Some(&data), "common_chars.txt"),
        Some(user_common),
        "用户 schemas/ 同名文件必须覆盖安装目录"
    );

    // 子目录形式的资源（第三方方案的拆字库只在用户目录下，只拼 data_dir 会永远找不到）
    let user_chaizi = write_at(&user, "schemas/tigercode/huma_chaizi.txt", "user");
    assert_eq!(
        Config::resolve_schema_resource(Some(&data), "tigercode/huma_chaizi.txt"),
        Some(user_chaizi),
        "用户目录独有的方案资源必须能解析到"
    );

    // 两套根不得串味：数据根解析绝不能命中 schemas/ 下的同名文件，反之亦然。
    assert_eq!(
        Config::resolve_data_file(Some(&data), "common_chars.txt"),
        None,
        "数据根解析不得穿透到 schemas/ 子目录"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}
