//! 整机备份：config + 逐表用户数据（文本）+ 用户方案/主题目录 + 可选 state，
//! 组合 bundle/merge/store 导出原语打成 kind=backup 的自描述 zip。
use crate::bundle::{BundleKind, BundleWriter, Manifest};
use std::path::{Path, PathBuf};
use wind_store::store::Store;

pub struct BackupOptions {
    pub include_stats: bool,
    pub include_state: bool,
}

pub struct BackupSources<'a> {
    pub user_config_file: Option<&'a Path>,
    pub user_schemas_dir: Option<&'a Path>,
    pub user_themes_dir: Option<&'a Path>,
    pub state_file: Option<&'a Path>,
}

pub struct BackupResult {
    pub path: PathBuf,
    pub entries: Vec<String>,
}

/// 递归收集目录下全部文件的 (zip条目名, 绝对路径)；条目名 = prefix + 目录相对路径（`/`分隔）。
fn walk_dir(dir: &Path, prefix: &str) -> anyhow::Result<Vec<(String, PathBuf)>> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d)? {
            let entry = entry?;
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.is_file() {
                let rel = p
                    .strip_prefix(dir)
                    .map_err(|e| anyhow::anyhow!("{e}"))?
                    .to_string_lossy()
                    .replace('\\', "/");
                out.push((format!("{prefix}{rel}"), p));
            }
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

/// 创建整机备份。schema 清单取 `store.list_data_schemas()`（覆盖有数据但未启用的方案）。
pub fn create_backup(
    store: &Store,
    src: &BackupSources,
    out_path: &Path,
    app_version: &str,
    platform: &str,
    created_at: &str,
    opts: &BackupOptions,
) -> anyhow::Result<BackupResult> {
    let manifest = Manifest::new(BundleKind::Backup, app_version, platform, created_at);
    let mut w = BundleWriter::new(out_path, manifest)?;
    let mut entries = Vec::new();
    let mut add = |w: &mut BundleWriter,
                   name: String,
                   data: &[u8],
                   ty: &str,
                   meta: serde_json::Value|
     -> anyhow::Result<()> {
        w.add_bytes_with(&name, data, ty, meta)?;
        entries.push(name);
        Ok(())
    };

    // 文件域：config / state
    if let Some(cfg) = src.user_config_file {
        if cfg.is_file() {
            add(
                &mut w,
                "config/config.toml".into(),
                &std::fs::read(cfg)?,
                "config",
                serde_json::Value::Null,
            )?;
        }
    }
    if opts.include_state {
        if let Some(st) = src.state_file {
            if st.is_file() {
                add(
                    &mut w,
                    "state/state.toml".into(),
                    &std::fs::read(st)?,
                    "state",
                    serde_json::Value::Null,
                )?;
            }
        }
    }

    // 数据域：逐 schema 四表 + 全局 phrases
    let schemas = store.list_data_schemas()?;
    for sc in &schemas {
        let meta = serde_json::json!({ "schema": sc });
        let words = store.export_user_words_wdict(sc, created_at)?;
        add(
            &mut w,
            format!("userdata/user_words/{sc}.wdict"),
            words.as_bytes(),
            "dict",
            meta.clone(),
        )?;
        let temp = store.export_temp_words_wdict(sc, created_at)?;
        add(
            &mut w,
            format!("userdata/temp_words/{sc}.wdict"),
            temp.as_bytes(),
            "temp",
            meta.clone(),
        )?;
        let freq = store.export_freq_jsonl(sc)?;
        add(
            &mut w,
            format!("userdata/freq/{sc}.jsonl"),
            freq.as_bytes(),
            "freq",
            meta.clone(),
        )?;
        let shadow = store.export_shadow_jsonl(sc)?;
        add(
            &mut w,
            format!("userdata/shadow/{sc}.jsonl"),
            shadow.as_bytes(),
            "shadow",
            meta,
        )?;
    }
    let phrases = store.export_user_phrases_wdict(created_at)?;
    add(
        &mut w,
        "userdata/phrases.wdict".into(),
        phrases.as_bytes(),
        "phrase",
        serde_json::Value::Null,
    )?;

    if opts.include_stats {
        let stats = store.export_stats_jsonl()?;
        add(
            &mut w,
            "userdata/stats.jsonl".into(),
            stats.as_bytes(),
            "stats",
            serde_json::Value::Null,
        )?;
        let meta = store.get_stats_meta()?;
        add(
            &mut w,
            "userdata/stats_meta.json".into(),
            serde_json::to_vec(&meta)?.as_slice(),
            "stats_meta",
            serde_json::Value::Null,
        )?;
    }

    // 文件域：用户方案 / 主题整目录
    if let Some(dir) = src.user_schemas_dir {
        if dir.is_dir() {
            for (name, path) in walk_dir(dir, "schemas/")? {
                let data = std::fs::read(&path)?;
                add(&mut w, name, &data, "schema_file", serde_json::Value::Null)?;
            }
        }
    }
    if let Some(dir) = src.user_themes_dir {
        if dir.is_dir() {
            for (name, path) in walk_dir(dir, "themes/")? {
                let data = std::fs::read(&path)?;
                add(&mut w, name, &data, "theme_file", serde_json::Value::Null)?;
            }
        }
    }

    w.finish()?;
    Ok(BackupResult {
        path: out_path.to_path_buf(),
        entries,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn seed_store(dir: &std::path::Path) -> wind_store::store::Store {
        let s = wind_store::store::Store::open(dir.join("t.redb")).unwrap();
        s.add_user_word("wb", "a", "工", 100).unwrap();
        s.learn_temp_word("wb", "ab", "临", 5).unwrap();
        s.record_freq("wb", "a", "工").unwrap();
        s.pin_shadow("wb", "aa", "恭", None, 0).unwrap();
        s.add_phrase("bj", "北京", 0, 10).unwrap();
        s
    }

    #[test]
    fn create_backup_covers_all_sections() {
        let t = tempfile::tempdir().unwrap();
        let s = seed_store(t.path());
        // 文件域 fixtures
        let cfg = t.path().join("config.toml");
        fs::write(&cfg, "[ui]\n").unwrap();
        let schemas = t.path().join("schemas");
        fs::create_dir_all(schemas.join("my")).unwrap();
        fs::write(schemas.join("my.schema.toml"), "[schema]\nid=\"my\"\n").unwrap();
        fs::write(schemas.join("my/d.yaml"), "d").unwrap();
        let themes = t.path().join("themes");
        fs::create_dir_all(themes.join("dark")).unwrap();
        fs::write(themes.join("dark/theme.toml"), "[meta]\nname=\"dark\"\n").unwrap();
        let state = t.path().join("state.toml");
        fs::write(&state, "[toolbar]\n").unwrap();

        let out = t.path().join("backup.zip");
        let src = BackupSources {
            user_config_file: Some(&cfg),
            user_schemas_dir: Some(&schemas),
            user_themes_dir: Some(&themes),
            state_file: Some(&state),
        };
        let r = create_backup(
            &s,
            &src,
            &out,
            "1.0.0",
            "windows",
            "t",
            &BackupOptions {
                include_stats: true,
                include_state: true,
            },
        )
        .unwrap();

        let m = crate::bundle::read_manifest(&out).unwrap();
        assert_eq!(m.kind, crate::bundle::BundleKind::Backup);
        let types: Vec<&str> = m.contents.iter().map(|e| e.r#type.as_str()).collect();
        for ty in [
            "config",
            "dict",
            "temp",
            "phrase",
            "freq",
            "shadow",
            "stats",
            "stats_meta",
            "schema_file",
            "theme_file",
            "state",
        ] {
            assert!(types.contains(&ty), "缺 {ty} 条目; got {types:?}");
        }
        // dict 条目路径与 meta.schema
        let dict = m.contents.iter().find(|e| e.r#type == "dict").unwrap();
        assert_eq!(dict.path, "userdata/user_words/wb.wdict");
        assert_eq!(dict.meta.get("schema").and_then(|v| v.as_str()), Some("wb"));
        // schema_file 递归含子目录文件
        assert!(m.contents.iter().any(|e| e.path == "schemas/my/d.yaml"));
        // 载荷可取
        let bytes = crate::bundle::extract_entry(&out, "config/config.toml").unwrap();
        assert_eq!(bytes, b"[ui]\n");
        assert!(!r.entries.is_empty());
    }

    #[test]
    fn create_backup_options_exclude() {
        let t = tempfile::tempdir().unwrap();
        let s = seed_store(t.path());
        let out = t.path().join("b2.zip");
        let src = BackupSources {
            user_config_file: None,
            user_schemas_dir: None,
            user_themes_dir: None,
            state_file: None,
        };
        create_backup(
            &s,
            &src,
            &out,
            "1.0.0",
            "windows",
            "t",
            &BackupOptions {
                include_stats: false,
                include_state: false,
            },
        )
        .unwrap();
        let m = crate::bundle::read_manifest(&out).unwrap();
        let types: Vec<&str> = m.contents.iter().map(|e| e.r#type.as_str()).collect();
        assert!(!types.contains(&"stats"), "include_stats=false 不含 stats");
        assert!(!types.contains(&"state"));
        assert!(!types.contains(&"config"), "无 config 源则无 config 条目");
        assert!(types.contains(&"dict"), "store 数据域始终导出");
    }
}
