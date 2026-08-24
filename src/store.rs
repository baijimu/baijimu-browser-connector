use rand::{rngs::OsRng, RngCore};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ProfileMetadata {
    pub schema_version: u32,
    pub profile_id: String,
    pub display_name: String,
    pub workspace_id: u64,
    pub allowed_hosts: Vec<String>,
    pub created_at: u64,
    pub updated_at: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorkspacePolicy {
    pub schema_version: u32,
    pub workspace_id: u64,
    pub upload_roots: Vec<PathBuf>,
    pub updated_at: u64,
}

#[derive(Clone, Debug)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn open(root: PathBuf) -> Result<Self, String> {
        create_private_dir(&root)?;
        create_private_dir(&root.join("profiles"))?;
        create_private_dir(&root.join("policies"))?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn profile_dir(&self, workspace_id: u64, profile_id: &str) -> PathBuf {
        self.root
            .join("profiles")
            .join(workspace_id.to_string())
            .join(profile_id)
    }

    pub fn browser_data_dir(&self, workspace_id: u64, profile_id: &str) -> PathBuf {
        self.profile_dir(workspace_id, profile_id)
            .join("browser-data")
    }

    pub fn create_profile(
        &self,
        workspace_id: u64,
        profile_id: &str,
        display_name: &str,
        allowed_hosts: Vec<String>,
        now: u64,
    ) -> Result<ProfileMetadata, String> {
        validate_profile_id(profile_id)?;
        validate_display_name(display_name)?;
        let allowed_hosts = normalize_allowed_hosts(allowed_hosts)?;
        let directory = self.profile_dir(workspace_id, profile_id);
        if directory.exists() {
            return Err(format!("profile already exists: {profile_id}"));
        }
        create_private_dir(&directory)?;
        create_private_dir(&directory.join("browser-data"))?;
        let profile = ProfileMetadata {
            schema_version: 1,
            profile_id: profile_id.to_string(),
            display_name: display_name.trim().to_string(),
            workspace_id,
            allowed_hosts,
            created_at: now,
            updated_at: now,
        };
        write_json_atomic(&directory.join("profile.json"), &profile)?;
        Ok(profile)
    }

    pub fn get_profile(
        &self,
        workspace_id: u64,
        profile_id: &str,
    ) -> Result<ProfileMetadata, String> {
        validate_profile_id(profile_id)?;
        let path = self
            .profile_dir(workspace_id, profile_id)
            .join("profile.json");
        let bytes =
            fs::read(&path).map_err(|error| format!("profile not found or unreadable: {error}"))?;
        let profile: ProfileMetadata = serde_json::from_slice(&bytes)
            .map_err(|error| format!("invalid profile metadata: {error}"))?;
        if profile.schema_version != 1
            || profile.workspace_id != workspace_id
            || profile.profile_id != profile_id
        {
            return Err("profile identity does not match its storage location".to_string());
        }
        Ok(profile)
    }

    pub fn list_profiles(&self, workspace_id: u64) -> Result<Vec<ProfileMetadata>, String> {
        let root = self.root.join("profiles").join(workspace_id.to_string());
        if !root.exists() {
            return Ok(Vec::new());
        }
        let mut profiles = Vec::new();
        for entry in fs::read_dir(root).map_err(|error| error.to_string())? {
            let entry = entry.map_err(|error| error.to_string())?;
            if !entry
                .file_type()
                .map_err(|error| error.to_string())?
                .is_dir()
            {
                continue;
            }
            let Some(profile_id) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            profiles.push(self.get_profile(workspace_id, &profile_id)?);
        }
        profiles.sort_by(|left, right| left.profile_id.cmp(&right.profile_id));
        Ok(profiles)
    }

    pub fn update_profile(
        &self,
        workspace_id: u64,
        profile_id: &str,
        display_name: &str,
        allowed_hosts: Vec<String>,
        now: u64,
    ) -> Result<ProfileMetadata, String> {
        let mut profile = self.get_profile(workspace_id, profile_id)?;
        validate_display_name(display_name)?;
        profile.display_name = display_name.trim().to_string();
        profile.allowed_hosts = normalize_allowed_hosts(allowed_hosts)?;
        profile.updated_at = now;
        write_json_atomic(
            &self
                .profile_dir(workspace_id, profile_id)
                .join("profile.json"),
            &profile,
        )?;
        Ok(profile)
    }

    pub fn delete_profile(&self, workspace_id: u64, profile_id: &str) -> Result<(), String> {
        self.get_profile(workspace_id, profile_id)?;
        let path = self.profile_dir(workspace_id, profile_id);
        fs::remove_dir_all(path).map_err(|error| error.to_string())
    }

    pub fn get_policy(&self, workspace_id: u64) -> Result<WorkspacePolicy, String> {
        let path = self
            .root
            .join("policies")
            .join(format!("{workspace_id}.json"));
        if !path.exists() {
            return Ok(WorkspacePolicy {
                schema_version: 1,
                workspace_id,
                upload_roots: Vec::new(),
                updated_at: 0,
            });
        }
        let policy: WorkspacePolicy =
            serde_json::from_slice(&fs::read(path).map_err(|error| error.to_string())?)
                .map_err(|error| format!("invalid workspace policy: {error}"))?;
        if policy.schema_version != 1 || policy.workspace_id != workspace_id {
            return Err("workspace policy identity mismatch".to_string());
        }
        Ok(policy)
    }

    pub fn set_policy(
        &self,
        workspace_id: u64,
        upload_roots: Vec<PathBuf>,
        now: u64,
    ) -> Result<WorkspacePolicy, String> {
        let mut normalized = Vec::new();
        for root in upload_roots {
            if !root.is_absolute() {
                return Err(format!("upload root must be absolute: {}", root.display()));
            }
            let canonical = root
                .canonicalize()
                .map_err(|error| format!("upload root is unavailable: {error}"))?;
            if !canonical.is_dir() {
                return Err(format!(
                    "upload root is not a directory: {}",
                    canonical.display()
                ));
            }
            if !normalized.contains(&canonical) {
                normalized.push(canonical);
            }
        }
        normalized.sort();
        let policy = WorkspacePolicy {
            schema_version: 1,
            workspace_id,
            upload_roots: normalized,
            updated_at: now,
        };
        write_json_atomic(
            &self
                .root
                .join("policies")
                .join(format!("{workspace_id}.json")),
            &policy,
        )?;
        Ok(policy)
    }

    pub fn authorize_upload_paths(
        &self,
        workspace_id: u64,
        paths: &[PathBuf],
    ) -> Result<Vec<PathBuf>, String> {
        if paths.is_empty() || paths.len() > 10 {
            return Err("upload requires between 1 and 10 files".to_string());
        }
        let policy = self.get_policy(workspace_id)?;
        if policy.upload_roots.is_empty() {
            return Err("no upload roots are authorized for this workspace".to_string());
        }
        let mut result = Vec::new();
        for path in paths {
            if !path.is_absolute() {
                return Err(format!("upload path must be absolute: {}", path.display()));
            }
            let canonical = path
                .canonicalize()
                .map_err(|error| format!("upload file is unavailable: {error}"))?;
            let metadata = canonical
                .metadata()
                .map_err(|error| format!("upload file is unreadable: {error}"))?;
            if !metadata.is_file() {
                return Err(format!(
                    "upload path is not a regular file: {}",
                    canonical.display()
                ));
            }
            if metadata.len() > 10 * 1024 * 1024 * 1024 {
                return Err(format!(
                    "upload file exceeds 10 GiB: {}",
                    canonical.display()
                ));
            }
            if !policy
                .upload_roots
                .iter()
                .any(|root| canonical.starts_with(root))
            {
                return Err(format!(
                    "upload file is outside authorized roots: {}",
                    canonical.display()
                ));
            }
            result.push(canonical);
        }
        Ok(result)
    }
}

pub fn validate_profile_id(profile_id: &str) -> Result<(), String> {
    let valid = (1..=48).contains(&profile_id.len())
        && profile_id
            .bytes()
            .all(|value| value.is_ascii_lowercase() || value.is_ascii_digit() || value == b'-')
        && !profile_id.starts_with('-')
        && !profile_id.ends_with('-');
    if !valid {
        return Err(
            "profileId must be 1-48 lowercase letters, digits, or internal hyphens".to_string(),
        );
    }
    Ok(())
}

fn validate_display_name(display_name: &str) -> Result<(), String> {
    let length = display_name.trim().chars().count();
    if !(1..=80).contains(&length) {
        return Err("displayName must contain 1-80 characters".to_string());
    }
    Ok(())
}

pub fn normalize_allowed_hosts(hosts: Vec<String>) -> Result<Vec<String>, String> {
    if hosts.is_empty() || hosts.len() > 100 {
        return Err("allowedHosts must contain between 1 and 100 hosts".to_string());
    }
    let mut normalized = Vec::new();
    for raw in hosts {
        let host = raw.trim().to_ascii_lowercase();
        let bare = host.strip_prefix("*.").unwrap_or(&host);
        let valid = !bare.is_empty()
            && bare.len() <= 253
            && !bare.starts_with('.')
            && !bare.ends_with('.')
            && bare.split('.').all(|label| {
                !label.is_empty()
                    && label.len() <= 63
                    && !label.starts_with('-')
                    && !label.ends_with('-')
                    && label.bytes().all(|value| {
                        value.is_ascii_lowercase() || value.is_ascii_digit() || value == b'-'
                    })
            });
        if !valid || bare == "localhost" || bare.ends_with(".localhost") || bare.ends_with(".local")
        {
            return Err(format!("invalid or local allowed host: {raw}"));
        }
        if !normalized.contains(&host) {
            normalized.push(host);
        }
    }
    normalized.sort();
    Ok(normalized)
}

pub fn load_or_create_token(root: &Path) -> Result<String, String> {
    create_private_dir(root)?;
    let path = root.join("management-token");
    if let Ok(value) = fs::read_to_string(&path) {
        let value = value.trim();
        if value.len() >= 32 {
            return Ok(value.to_string());
        }
    }
    let mut bytes = [0_u8; 32];
    OsRng.fill_bytes(&mut bytes);
    let token = bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    write_private_atomic(&path, format!("{token}\n").as_bytes())?;
    Ok(token)
}

fn create_private_dir(path: &Path) -> Result<(), String> {
    fs::create_dir_all(path).map_err(|error| error.to_string())?;
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|error| error.to_string())?;
    Ok(())
}

fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(value).map_err(|error| error.to_string())?;
    write_private_atomic(path, &bytes)
}

fn write_private_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "state path has no parent".to_string())?;
    create_private_dir(parent)?;
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    fs::write(&temporary, bytes).map_err(|error| error.to_string())?;
    #[cfg(unix)]
    fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))
        .map_err(|error| error.to_string())?;
    #[cfg(windows)]
    if path.exists() {
        fs::remove_file(path).map_err(|error| error.to_string())?;
    }
    fs::rename(&temporary, path).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temporary_store() -> (Store, PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "baijimu-browser-store-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        (Store::open(root.clone()).unwrap(), root)
    }

    #[test]
    fn profiles_are_workspace_scoped_and_persistent() {
        let (store, root) = temporary_store();
        let created = store
            .create_profile(
                1211,
                "video-operations",
                "视频运营",
                vec!["*.example.com".to_string(), "example.com".to_string()],
                10,
            )
            .unwrap();
        assert_eq!(created.workspace_id, 1211);
        assert!(store.get_profile(642, "video-operations").is_err());
        let reopened = Store::open(root.clone()).unwrap();
        assert_eq!(
            reopened.get_profile(1211, "video-operations").unwrap(),
            created
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn profile_and_host_validation_rejects_unsafe_values() {
        assert!(validate_profile_id("../escape").is_err());
        assert!(validate_profile_id("Valid").is_err());
        assert!(normalize_allowed_hosts(vec!["localhost".to_string()]).is_err());
        assert!(normalize_allowed_hosts(vec!["*.example.com".to_string()]).is_ok());
    }

    #[test]
    fn upload_authorization_uses_canonical_roots() {
        let (store, root) = temporary_store();
        let allowed = root.join("uploads");
        fs::create_dir_all(&allowed).unwrap();
        let file = allowed.join("video.mp4");
        fs::write(&file, b"video").unwrap();
        store.set_policy(1211, vec![allowed], 20).unwrap();
        let canonical_file = file.canonicalize().unwrap();
        assert_eq!(
            store
                .authorize_upload_paths(1211, std::slice::from_ref(&file))
                .unwrap(),
            vec![canonical_file]
        );
        assert!(store
            .authorize_upload_paths(642, &[root.join("uploads/video.mp4")])
            .is_err());
        fs::remove_dir_all(root).unwrap();
    }
}
