use serde::{Deserialize, Serialize};

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
