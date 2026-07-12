use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::path::Path;

pub const SPEC_VERSION: u32 = 1;
pub const FORMAT_TAG: &str = "windinput-bundle";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BundleKind {
    Scheme,
    Backup,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContentEntry {
    pub r#type: String,
    pub path: String,
    #[serde(default)]
    pub meta: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub format: String,
    pub kind: BundleKind,
    pub spec_version: u32,
    pub app_version: String,
    pub platform: String,
    pub created_at: String,
    #[serde(default)]
    pub contents: Vec<ContentEntry>,
}

impl Manifest {
    pub fn new(kind: BundleKind, app_version: &str, platform: &str, created_at: &str) -> Self {
        Self {
            format: FORMAT_TAG.to_string(),
            kind,
            spec_version: SPEC_VERSION,
            app_version: app_version.to_string(),
            platform: platform.to_string(),
            created_at: created_at.to_string(),
            contents: Vec::new(),
        }
    }

    /// 校验:format 与 FORMAT_TAG 一致,且 spec_version 不高于当前支持版本。
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.format != FORMAT_TAG {
            anyhow::bail!("非 WindInput 归档(format={})", self.format);
        }
        if self.spec_version > SPEC_VERSION {
            anyhow::bail!(
                "归档版本过高(spec_version={},当前支持 {}),请升级 WindInput",
                self.spec_version,
                SPEC_VERSION
            );
        }
        Ok(())
    }
}

const MANIFEST_NAME: &str = "manifest.json";

pub struct BundleWriter {
    writer: zip::ZipWriter<std::fs::File>,
    manifest: Manifest,
}

impl BundleWriter {
    pub fn new(path: &Path, manifest: Manifest) -> anyhow::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::File::create(path)?;
        Ok(Self {
            writer: zip::ZipWriter::new(file),
            manifest,
        })
    }

    /// 写入一个条目,并在 manifest.contents 里登记(type 由调用方后续细化;P1 记 path)。
    pub fn add_bytes(&mut self, name: &str, data: &[u8]) -> anyhow::Result<()> {
        self.writer
            .start_file(name, zip::write::SimpleFileOptions::default())?;
        self.writer.write_all(data)?;
        self.manifest.contents.push(ContentEntry {
            r#type: String::new(),
            path: name.to_string(),
            meta: serde_json::Value::Null,
        });
        Ok(())
    }

    /// 收尾:写入 manifest.json 并关闭。
    pub fn finish(mut self) -> anyhow::Result<()> {
        let json = serde_json::to_vec_pretty(&self.manifest)?;
        self.writer
            .start_file(MANIFEST_NAME, zip::write::SimpleFileOptions::default())?;
        self.writer.write_all(&json)?;
        self.writer.finish()?;
        Ok(())
    }
}

/// 免全解压读取并校验 manifest.json。
pub fn read_manifest(path: &Path) -> anyhow::Result<Manifest> {
    let file = std::fs::File::open(path)?;
    let mut archive = zip::ZipArchive::new(file)?;
    let mut entry = archive
        .by_name(MANIFEST_NAME)
        .map_err(|_| anyhow::anyhow!("归档缺少 {}", MANIFEST_NAME))?;
    let mut buf = String::new();
    entry.read_to_string(&mut buf)?;
    let manifest: Manifest = serde_json::from_str(&buf)?;
    manifest.validate()?;
    Ok(manifest)
}

/// 读取单个条目字节。
pub fn extract_entry(path: &Path, name: &str) -> anyhow::Result<Vec<u8>> {
    let file = std::fs::File::open(path)?;
    let mut archive = zip::ZipArchive::new(file)?;
    let mut entry = archive
        .by_name(name)
        .map_err(|_| anyhow::anyhow!("归档缺少条目 {}", name))?;
    let mut buf = Vec::new();
    entry.read_to_end(&mut buf)?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile;

    #[test]
    fn manifest_roundtrip_and_validate() {
        let m = Manifest::new(
            BundleKind::Backup,
            "1.2.3",
            "windows",
            "2026-07-11T00:00:00+08:00",
        );
        assert_eq!(m.format, FORMAT_TAG);
        assert_eq!(m.spec_version, SPEC_VERSION);
        m.validate().unwrap();

        let json = serde_json::to_string(&m).unwrap();
        assert!(
            json.contains("\"kind\":\"backup\""),
            "kind 序列化为小写字符串"
        );
        let back: Manifest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.app_version, "1.2.3");
    }

    #[test]
    fn validate_rejects_future_spec_and_bad_format() {
        let mut m = Manifest::new(BundleKind::Scheme, "1.0.0", "darwin", "t");
        m.spec_version = SPEC_VERSION + 1;
        assert!(m.validate().is_err(), "更高 spec_version 应拒绝");

        let mut m2 = Manifest::new(BundleKind::Scheme, "1.0.0", "darwin", "t");
        m2.format = "wrong".into();
        assert!(m2.validate().is_err(), "format 不匹配应拒绝");
    }

    #[test]
    fn bundle_write_read_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let zip_path = dir.path().join("t.zip");

        let manifest = Manifest::new(BundleKind::Backup, "1.0.0", "windows", "t");
        let mut w = BundleWriter::new(&zip_path, manifest).unwrap();
        w.add_bytes("userdata/user_words.wdict", b"hello-words")
            .unwrap();
        w.finish().unwrap();

        // 免解压读 manifest
        let m = read_manifest(&zip_path).unwrap();
        assert_eq!(m.kind, BundleKind::Backup);
        // 取单个条目
        let data = extract_entry(&zip_path, "userdata/user_words.wdict").unwrap();
        assert_eq!(data, b"hello-words");
    }

    #[test]
    fn read_manifest_rejects_bad_bundle() {
        let dir = tempfile::tempdir().unwrap();
        let bad = dir.path().join("bad.zip");
        // 手写一个不含 manifest.json 的 zip
        let mut w = zip::ZipWriter::new(std::fs::File::create(&bad).unwrap());
        w.start_file("foo.txt", zip::write::SimpleFileOptions::default())
            .unwrap();
        use std::io::Write;
        w.write_all(b"x").unwrap();
        w.finish().unwrap();
        assert!(read_manifest(&bad).is_err(), "缺 manifest.json 应报错");
    }
}
