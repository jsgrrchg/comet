//! OS keyring integration. An identity hash is the account, so stale results
//! cannot supply credentials to a profile whose endpoint or account has changed.
use super::profiles::Profile;
use crate::settings::{self, SavePolicy};
use gpui::{App, Task};
use sha2::{Digest, Sha256};
use std::path::Path;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CredentialKey {
    pub service: String,
    pub account: String,
}
impl CredentialKey {
    pub fn new(data_dir: &Path, profile: &Profile) -> Self {
        let directory = std::fs::canonicalize(data_dir).unwrap_or_else(|_| data_dir.to_path_buf());
        let scope = format!(
            "{:x}",
            Sha256::digest(directory.as_os_str().as_encoded_bytes())
        );
        let identity = serde_json::to_vec(&(
            profile.host.to_ascii_lowercase(),
            profile.port,
            &profile.domain,
            &profile.username,
        ))
        .expect("string tuple serializes");
        Self {
            service: format!("zeron.remote-desktop.v1/{scope}/{}", profile.id),
            account: format!("{:x}", Sha256::digest(identity)),
        }
    }
    pub fn read(&self, cx: &App) -> Task<anyhow::Result<Option<(String, Vec<u8>)>>> {
        cx.read_credentials(&self.service)
    }
    pub fn write(&self, password: &[u8], cx: &App) -> Task<anyhow::Result<()>> {
        cx.write_credentials(&self.service, &self.account, password)
    }
    pub fn matches(&self, account: &str) -> bool {
        self.account == account
    }
}

/// Persist deletion intent *before* asking the keyring. A failed delete remains
/// retryable after the profile is removed or the application is restarted.
pub fn queue_delete(service: String, cx: &mut App) {
    settings::update(SavePolicy::Immediate, cx, |s| {
        if !s.remote_desktop_credential_cleanup.contains(&service) {
            s.remote_desktop_credential_cleanup.push(service);
        }
    });
}
pub fn delete(service: &str, cx: &App) -> Task<anyhow::Result<()>> {
    cx.delete_credentials(service)
}
pub fn finish_delete(service: &str, cx: &mut App) {
    settings::update(SavePolicy::Immediate, cx, |s| {
        s.remote_desktop_credential_cleanup
            .retain(|key| key != service)
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn remote_desktop_keyring_scope_tracks_identity_not_label() {
        let mut profile = Profile::default();
        profile.host = "server".into();
        let a = CredentialKey::new(Path::new("/tmp/rdp-a"), &profile);
        profile.name = "Renamed".into();
        assert_eq!(a, CredentialKey::new(Path::new("/tmp/rdp-a"), &profile));
        profile.username = "another".into();
        let b = CredentialKey::new(Path::new("/tmp/rdp-a"), &profile);
        assert_eq!(a.service, b.service);
        assert!(!a.matches(&b.account));
        assert_ne!(
            a.service,
            CredentialKey::new(Path::new("/tmp/rdp-b"), &profile).service
        );
        profile.id = uuid::Uuid::new_v4();
        assert_ne!(
            a.service,
            CredentialKey::new(Path::new("/tmp/rdp-a"), &profile).service
        );
    }
}
