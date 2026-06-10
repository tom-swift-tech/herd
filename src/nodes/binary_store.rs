//! Published agent-binary store for the gateway's fleet version authority
//! (v1.2 PR #6).
//!
//! Layout: `{publish_dir}/{version}/{os}-{arch}/herd[.exe]`. The gateway
//! advertises `fleet.target_agent_version` on every heartbeat but attaches a
//! download URL + sha256 only when a binary actually exists here for the
//! agent's reported platform — dropping a binary into the store (or running
//! `herd publish`, PR #6c) is the deliberate promote step.
//!
//! The sha256 of a served binary is computed lazily and cached keyed on the
//! file's (len, mtime), so steady 2s heartbeats never re-hash a ~30MB file.

use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

/// A cached digest is valid only while the file's (len, mtime) are unchanged.
#[derive(Debug, Clone)]
struct CachedSha {
    len: u64,
    mtime: Option<SystemTime>,
    sha256: String,
}

/// True if `s` is shaped like a version string (the charset
/// `config::Config::validate` allows for `fleet.target_agent_version`).
/// Used for path components, so path separators and `..` are unrepresentable.
pub fn version_shaped(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '+'))
        && !s.split('.').any(|part| part.is_empty())
}

/// True if `s` is shaped like a `std::env::consts::{OS, ARCH}` value
/// ("windows", "x86_64", "aarch64", ...).
pub fn platform_shaped(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 32
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// Resolve the on-disk path of the published binary for (version, os, arch)
/// under `publish_dir`. Returns `None` when any component is malformed —
/// components are validated to a charset that cannot traverse, never joined
/// raw from request input.
pub fn binary_path(publish_dir: &Path, version: &str, os: &str, arch: &str) -> Option<PathBuf> {
    if !version_shaped(version) || !platform_shaped(os) || !platform_shaped(arch) {
        return None;
    }
    let file = if os == "windows" { "herd.exe" } else { "herd" };
    Some(
        publish_dir
            .join(version)
            .join(format!("{os}-{arch}"))
            .join(file),
    )
}

/// Lazy, invalidating sha256 cache over published binaries. Shared on
/// `AppState`; cheap to clone via `Arc`.
#[derive(Debug, Default)]
pub struct BinaryStore {
    cache: Mutex<HashMap<PathBuf, CachedSha>>,
}

impl BinaryStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock_cache(&self) -> std::sync::MutexGuard<'_, HashMap<PathBuf, CachedSha>> {
        // A poisoned cache mutex only means another thread panicked mid-insert;
        // the map is still structurally valid, so recover rather than unwrap.
        match self.cache.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Hex-encoded sha256 of the file at `path`, served from cache while the
    /// file's (len, mtime) are unchanged. Synchronous file IO — call through
    /// [`sha256_async`](Self::sha256_async) from request handlers.
    pub fn sha256_of(&self, path: &Path) -> std::io::Result<String> {
        let meta = std::fs::metadata(path)?;
        if !meta.is_file() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("{} is not a regular file", path.display()),
            ));
        }
        let len = meta.len();
        let mtime = meta.modified().ok();

        if let Some(hit) = self.lock_cache().get(path) {
            if hit.len == len && hit.mtime == mtime {
                return Ok(hit.sha256.clone());
            }
        }

        let mut file = std::fs::File::open(path)?;
        let mut hasher = Sha256::new();
        std::io::copy(&mut file, &mut hasher)?;
        let sha256: String = hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();

        self.lock_cache().insert(
            path.to_path_buf(),
            CachedSha {
                len,
                mtime,
                sha256: sha256.clone(),
            },
        );
        Ok(sha256)
    }

    /// Async wrapper over [`sha256_of`](Self::sha256_of) for handlers. Returns
    /// `None` when the binary is missing or unreadable — callers treat that as
    /// "nothing published", never as a request failure.
    pub async fn sha256_async(self: &Arc<Self>, path: PathBuf) -> Option<String> {
        let store = Arc::clone(self);
        match tokio::task::spawn_blocking(move || {
            let result = store.sha256_of(&path);
            (path, result)
        })
        .await
        {
            Ok((_, Ok(sha))) => Some(sha),
            Ok((path, Err(e))) => {
                tracing::debug!("No published binary at {}: {}", path.display(), e);
                None
            }
            Err(e) => {
                tracing::error!("sha256 hashing task failed: {}", e);
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_path_uses_exe_suffix_only_on_windows() {
        let dir = Path::new("/srv/binaries");
        let win = binary_path(dir, "1.2.0", "windows", "x86_64").unwrap();
        assert!(win.ends_with(Path::new("1.2.0/windows-x86_64/herd.exe")));

        let linux = binary_path(dir, "1.2.0", "linux", "x86_64").unwrap();
        assert!(linux.ends_with(Path::new("1.2.0/linux-x86_64/herd")));

        let mac = binary_path(dir, "1.2.0", "macos", "aarch64").unwrap();
        assert!(mac.ends_with(Path::new("1.2.0/macos-aarch64/herd")));
    }

    #[test]
    fn binary_path_rejects_traversal_and_malformed_components() {
        let dir = Path::new("/srv/binaries");
        assert!(binary_path(dir, "../etc", "linux", "x86_64").is_none());
        assert!(binary_path(dir, "..", "linux", "x86_64").is_none());
        assert!(binary_path(dir, "1.2.0", "linux/..", "x86_64").is_none());
        assert!(binary_path(dir, "1.2.0", "linux", "x86_64/../..").is_none());
        assert!(binary_path(dir, "1.2.0", "LINUX", "x86_64").is_none());
        assert!(binary_path(dir, "", "linux", "x86_64").is_none());
        assert!(binary_path(dir, "1.2.0", "", "x86_64").is_none());
        assert!(binary_path(dir, "1.2.0", "linux", "").is_none());
        assert!(binary_path(dir, "1..2", "linux", "x86_64").is_none());
        assert!(binary_path(dir, "1.2.0 ", "linux", "x86_64").is_none());
    }

    #[test]
    fn version_shaped_accepts_release_shapes() {
        assert!(version_shaped("1.2.0"));
        assert!(version_shaped("v1.2.0"));
        assert!(version_shaped("1.2.0-rc.1"));
        assert!(version_shaped("1.2.0+build5"));
        assert!(!version_shaped("1.2.0/.."));
        assert!(!version_shaped(".."));
    }

    #[test]
    fn sha256_of_known_content_matches() {
        let dir = std::env::temp_dir().join(format!("herd-binstore-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("known.bin");
        std::fs::write(&path, b"abc").unwrap();

        let store = BinaryStore::new();
        assert_eq!(
            store.sha256_of(&path).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        // Second call is served from cache and must agree.
        assert_eq!(
            store.sha256_of(&path).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn sha256_cache_invalidates_when_file_changes() {
        let dir = std::env::temp_dir().join(format!("herd-binstore-inv-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("changing.bin");
        std::fs::write(&path, b"first contents").unwrap();

        let store = BinaryStore::new();
        let first = store.sha256_of(&path).unwrap();

        // Different length guarantees invalidation even on coarse mtime
        // granularity filesystems.
        std::fs::write(&path, b"second, longer contents").unwrap();
        let second = store.sha256_of(&path).unwrap();
        assert_ne!(first, second);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn sha256_of_missing_file_errors() {
        let store = BinaryStore::new();
        assert!(store
            .sha256_of(Path::new("/definitely/not/here/herd"))
            .is_err());
    }

    #[tokio::test]
    async fn sha256_async_returns_none_for_missing_file() {
        let store = Arc::new(BinaryStore::new());
        assert!(store
            .sha256_async(PathBuf::from("/definitely/not/here/herd"))
            .await
            .is_none());
    }
}
