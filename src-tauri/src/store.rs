use std::path::PathBuf;

use crate::error::Result;
use crate::shield::Shield;

pub struct Store {
    path: PathBuf,
}

impl Store {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn load(&self) -> Result<Vec<Shield>> {
        match std::fs::read(&self.path) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn save(&self, shields: &[Shield]) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(shields)?)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shield::{Mount, NetworkMode, NetworkPolicy};

    #[test]
    fn round_trip() {
        let dir = std::env::temp_dir().join(format!("zwinger-store-{}", uuid::Uuid::new_v4()));
        let store = Store::new(dir.join("nested").join("shields.json"));
        assert!(store.load().unwrap().is_empty());

        let shields = vec![Shield {
            id: "a".into(),
            name: "proj".into(),
            folder: "/tmp/proj".into(),
            mounts: vec![Mount {
                host: "/tmp/data".into(),
                guest: "/mnt/data".into(),
                readonly: true,
            }],
            network: NetworkPolicy {
                mode: NetworkMode::Public,
                domains: vec!["registry.npmjs.org".into()],
            },
            harness: Some("/workspace/agent".into()),
        }];
        store.save(&shields).unwrap();
        assert_eq!(store.load().unwrap(), shields);
        assert!(!dir.join("nested").join("shields.json.tmp").exists());
    }

    #[test]
    fn loads_config_without_harness() {
        let dir = std::env::temp_dir().join(format!("zwinger-store-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("shields.json");
        std::fs::write(
            &path,
            r#"[{"id":"a","name":"proj","folder":"/tmp/proj","mounts":[],
                "network":{"allowAll":false,"domains":[]}}]"#,
        )
        .unwrap();
        assert_eq!(Store::new(path).load().unwrap()[0].harness, None);
    }
}
