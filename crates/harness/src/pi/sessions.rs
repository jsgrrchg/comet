//! Host-local UUID -> native file lookup. Never rewrite Pi's conversation files.
use crate::HarnessError;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
};

#[derive(Clone)]
pub(super) struct Store {
    root: PathBuf,
    agent: PathBuf,
    legacy: PathBuf,
}
impl Store {
    pub fn new(root: Option<PathBuf>) -> Self {
        let home = crate::executable::home_or_current_dir();
        let agent = crate::model_context::root("PI_CODING_AGENT_DIR", home.join(".pi/agent"));
        Self {
            root: root.unwrap_or_else(|| agent.join("zeron-sessions")),
            agent,
            legacy: home.join(".pi/pi-acp/session-map.json"),
        }
    }
    fn key(&self, id: &str) -> PathBuf {
        self.root
            .join(format!("{:x}.json", Sha256::digest(id.as_bytes())))
    }
    pub fn remember(&self, id: &str, file: &Path) -> Result<(), HarnessError> {
        if !file.is_absolute() {
            return Err(HarnessError::Protocol(
                "Pi returned a non-absolute session file".into(),
            ));
        }
        std::fs::create_dir_all(&self.root)?;
        let path = self.key(id);
        // Unique temp names avoid concurrent sessions truncating each other's map.
        let temp = self.root.join(format!("{}.tmp", uuid::Uuid::new_v4()));
        std::fs::write(
            &temp,
            serde_json::to_vec(&json!({"sessionId":id,"sessionFile":file})).unwrap(),
        )?;
        if let Err(error) = std::fs::rename(&temp, &path) {
            // Windows rename cannot replace a file. Existing identical mappings need no change.
            if json_file(&path)["sessionFile"] != file.to_string_lossy().as_ref() {
                let _ = std::fs::remove_file(&temp);
                return Err(error.into());
            }
            let _ = std::fs::remove_file(&temp);
        }
        Ok(())
    }
    pub fn resolve(&self, id: &str, cwd: &Path) -> Result<PathBuf, HarnessError> {
        let mut candidates = vec![];
        if let Some(file) = json_file(&self.key(id))["sessionFile"].as_str() {
            candidates.push(PathBuf::from(file));
        }
        if let Some(file) = json_file(&self.legacy)["sessions"][id]["sessionFile"].as_str() {
            candidates.push(PathBuf::from(file));
        }
        let mut roots = vec![self.agent.join("sessions")];
        for settings in [
            self.agent.join("settings.json"),
            cwd.join(".pi/settings.json"),
        ] {
            if let Some(root) = json_file(&settings)["sessionDir"].as_str() {
                let path = if let Some(rest) = root.strip_prefix("~/") {
                    crate::executable::home_or_current_dir().join(rest)
                } else {
                    cwd.join(root)
                };
                roots.push(path);
            }
        }
        for file in candidates {
            if matches_id(&file, id) {
                return Ok(file.canonicalize()?);
            }
        }
        for root in roots {
            // Pi stores session files under per-project directories. No symlink traversal.
            let mut dirs = vec![(root, 0)];
            let mut seen = 0;
            while let Some((dir, depth)) = dirs.pop() {
                let Ok(entries) = std::fs::read_dir(dir) else {
                    continue;
                };
                for entry in entries.flatten() {
                    seen += 1;
                    if seen > 20000 {
                        return Err(HarnessError::Protocol(
                            "Pi session search exceeded 20000 entries".into(),
                        ));
                    }
                    let Ok(kind) = entry.file_type() else {
                        continue;
                    };
                    let path = entry.path();
                    if kind.is_dir() && depth < 2 {
                        dirs.push((path, depth + 1));
                    } else if kind.is_file()
                        && path.extension().is_some_and(|e| e == "jsonl")
                        && matches_id(&path, id)
                    {
                        return Ok(path.canonicalize()?);
                    }
                }
            }
        }
        Err(HarnessError::Protocol(format!(
            "Cannot restore Pi session {id}: native session file was not found; previous context has not been replaced"
        )))
    }
}
fn json_file(path: &Path) -> Value {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or(Value::Null)
}
fn matches_id(path: &Path, id: &str) -> bool {
    use std::io::Read;
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    let mut line = String::new();
    if BufReader::new(file.take(65536))
        .read_line(&mut line)
        .is_err()
    {
        return false;
    }
    serde_json::from_str::<Value>(&line).is_ok_and(|v| v["type"] == "session" && v["id"] == id)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn resolves_legacy_and_native_files_and_rejects_wrong_identity() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session with spaces.jsonl");
        std::fs::write(&path, "{\"type\":\"session\",\"id\":\"old-id\"}\n").unwrap();
        let mut store = Store::new(Some(dir.path().join("index")));
        store.agent = dir.path().join("agent");
        store.legacy = dir.path().join("legacy.json");
        std::fs::write(
            &store.legacy,
            json!({"version":1,"sessions":{"old-id":{"sessionFile":path}}}).to_string(),
        )
        .unwrap();
        assert_eq!(store.resolve("old-id", dir.path()).unwrap(), path);
        store.remember("old-id", &path).unwrap();
        std::fs::remove_file(&store.legacy).unwrap();
        assert_eq!(store.resolve("old-id", dir.path()).unwrap(), path);
        store.remember("wrong-id", &path).unwrap();
        assert!(store.resolve("wrong-id", dir.path()).is_err());
    }
}
