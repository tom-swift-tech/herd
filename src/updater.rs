use anyhow::{Context, Result};
use sha2::Digest;
use std::path::Path;

const REPO_OWNER: &str = "swift-innovate";
const REPO_NAME: &str = "herd";
const BIN_NAME: &str = "herd";

/// Information about an available update.
#[derive(Debug, Clone, serde::Serialize)]
pub struct UpdateInfo {
    pub current: String,
    pub latest: String,
    pub update_available: bool,
}

/// Check GitHub Releases for a newer version without applying it.
pub fn check_for_update() -> Result<UpdateInfo> {
    let current = self_update::cargo_crate_version!().to_string();

    let releases = self_update::backends::github::ReleaseList::configure()
        .repo_owner(REPO_OWNER)
        .repo_name(REPO_NAME)
        .build()?
        .fetch()?;

    let latest = releases
        .first()
        .map(|r| r.version.clone())
        .unwrap_or_else(|| current.clone());

    let update_available = version_is_newer(&current, &latest);

    Ok(UpdateInfo {
        current,
        latest,
        update_available,
    })
}

/// Download and apply the latest release, replacing the current binary.
/// Returns the new version string on success.
pub fn perform_update(show_progress: bool) -> Result<String> {
    let status = self_update::backends::github::Update::configure()
        .repo_owner(REPO_OWNER)
        .repo_name(REPO_NAME)
        .bin_name(BIN_NAME)
        .show_download_progress(show_progress)
        .current_version(self_update::cargo_crate_version!())
        .build()?
        .update()?;

    Ok(status.version().to_string())
}

/// Download a binary from `url`, verify its sha256, and replace the current
/// executable in place. Synchronous (self_update uses blocking HTTP) — call
/// via `spawn_blocking`. The temp file is created in the same directory as
/// the current executable so the final swap never crosses filesystems.
///
/// On any failure (download, hash mismatch, swap) the temp file is removed
/// and the running binary is left untouched.
pub fn update_from_url(url: &str, expected_sha256: &str, token: Option<&str>) -> Result<()> {
    let current = std::env::current_exe().context("cannot resolve current executable path")?;
    let dir = current
        .parent()
        .context("current executable has no parent directory")?;
    let temp_path = dir.join(format!(".herd-update-{}.tmp", std::process::id()));

    if let Err(e) = download_to_file(url, token, &temp_path) {
        let _ = std::fs::remove_file(&temp_path);
        return Err(e).with_context(|| format!("failed to download update from {url}"));
    }

    apply_verified(&temp_path, expected_sha256, |staged| {
        self_update::self_replace::self_replace(staged)
            .context("failed to swap the running executable")
    })
}

fn download_to_file(url: &str, token: Option<&str>, dest: &Path) -> Result<()> {
    let mut file = std::fs::File::create(dest)
        .with_context(|| format!("failed to create temp file {}", dest.display()))?;
    let mut download = self_update::Download::from_url(url);
    download.set_header(
        reqwest::header::ACCEPT,
        reqwest::header::HeaderValue::from_static("application/octet-stream"),
    );
    if let Some(token) = token {
        let value = reqwest::header::HeaderValue::from_str(&format!("Bearer {token}"))
            .context("agent token is not a valid header value")?;
        download.set_header(reqwest::header::AUTHORIZATION, value);
    }
    download
        .download_to(&mut file)
        .map_err(|e| anyhow::anyhow!("download failed: {e}"))
}

/// Verify the staged binary's sha256, then hand it to `swap`. On hash
/// mismatch the staged file is deleted and no swap happens. The staged file
/// is best-effort removed afterwards either way (the swap may have already
/// consumed it via rename).
fn apply_verified(
    staged: &Path,
    expected_sha256: &str,
    swap: impl FnOnce(&Path) -> Result<()>,
) -> Result<()> {
    let actual = sha256_of_file(staged)?;
    let expected = expected_sha256.trim();
    if !actual.eq_ignore_ascii_case(expected) {
        let _ = std::fs::remove_file(staged);
        anyhow::bail!("sha256 mismatch: expected {expected}, downloaded binary hashed to {actual}");
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(staged, std::fs::Permissions::from_mode(0o755))
            .context("failed to mark staged binary executable")?;
    }

    let result = swap(staged);
    let _ = std::fs::remove_file(staged);
    result
}

fn sha256_of_file(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path)
        .with_context(|| format!("failed to open {} for hashing", path.display()))?;
    let mut hasher = sha2::Sha256::new();
    std::io::copy(&mut file, &mut hasher).context("failed to hash staged binary")?;
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// Simple semver comparison: returns true if `latest` is newer than `current`.
pub(crate) fn version_is_newer(current: &str, latest: &str) -> bool {
    let parse = |v: &str| -> Vec<u64> {
        v.trim_start_matches('v')
            .split('.')
            .filter_map(|s| s.parse().ok())
            .collect()
    };
    let c = parse(current);
    let l = parse(latest);
    l > c
}

/// Log an update notification at startup (non-blocking, best-effort).
pub async fn startup_update_check() {
    // Run in a blocking task since self_update uses synchronous HTTP
    let result = tokio::task::spawn_blocking(check_for_update).await;

    match result {
        Ok(Ok(info)) if info.update_available => {
            tracing::info!(
                "Update available: v{} → v{} (run `herd --update` to install)",
                info.current,
                info.latest
            );
        }
        Ok(Ok(_)) => {
            tracing::debug!("Herd is up to date");
        }
        Ok(Err(e)) => {
            tracing::debug!("Update check failed: {}", e);
        }
        Err(e) => {
            tracing::debug!("Update check task failed: {}", e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_comparison_newer() {
        assert!(version_is_newer("0.3.0", "0.4.0"));
        assert!(version_is_newer("0.4.0", "1.0.0"));
        assert!(version_is_newer("0.4.0", "0.4.1"));
    }

    #[test]
    fn version_comparison_same_or_older() {
        assert!(!version_is_newer("0.4.0", "0.4.0"));
        assert!(!version_is_newer("1.0.0", "0.4.0"));
        assert!(!version_is_newer("0.4.1", "0.4.0"));
    }

    #[test]
    fn version_comparison_handles_v_prefix() {
        assert!(version_is_newer("v0.3.0", "v0.4.0"));
        assert!(!version_is_newer("v0.4.0", "0.4.0"));
    }

    /// sha256("abc")
    const SHA_ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    fn staged_file(tag: &str, contents: &[u8]) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "herd-updater-test-{tag}-{}.tmp",
            std::process::id()
        ));
        std::fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn sha256_of_file_matches_known_digest() {
        let path = staged_file("digest", b"abc");
        assert_eq!(sha256_of_file(&path).unwrap(), SHA_ABC);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn apply_verified_swaps_on_matching_sha() {
        let path = staged_file("match", b"abc");
        let mut swapped = false;
        apply_verified(&path, SHA_ABC, |staged| {
            assert_eq!(staged, path.as_path());
            swapped = true;
            Ok(())
        })
        .unwrap();
        assert!(swapped);
        assert!(!path.exists(), "staged file must be cleaned up after swap");
    }

    #[test]
    fn apply_verified_accepts_uppercase_and_padded_sha() {
        let path = staged_file("case", b"abc");
        let mut swapped = false;
        apply_verified(&path, &format!("  {}  ", SHA_ABC.to_uppercase()), |_| {
            swapped = true;
            Ok(())
        })
        .unwrap();
        assert!(swapped);
    }

    #[test]
    fn apply_verified_on_mismatch_deletes_temp_and_never_swaps() {
        let path = staged_file("mismatch", b"not the published bytes");
        let mut swapped = false;
        let err = apply_verified(&path, SHA_ABC, |_| {
            swapped = true;
            Ok(())
        })
        .unwrap_err();
        assert!(err.to_string().contains("sha256 mismatch"), "{err}");
        assert!(!swapped, "swap must not run on hash mismatch");
        assert!(!path.exists(), "temp file must be removed on mismatch");
    }

    #[test]
    fn apply_verified_propagates_swap_failure_and_cleans_up() {
        let path = staged_file("swapfail", b"abc");
        let err = apply_verified(&path, SHA_ABC, |_| anyhow::bail!("rename denied")).unwrap_err();
        assert!(err.to_string().contains("rename denied"));
        assert!(!path.exists());
    }
}
