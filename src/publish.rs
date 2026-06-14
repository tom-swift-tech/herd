//! `herd publish` — the operator-facing write side of the fleet BinaryStore (v1.2 PR #6c).
//! Copies a binary into {publish_dir}/{version}/{os}-{arch}/herd[.exe] and prints the sha256
//! the gateway will advertise. Thin promote only: no config validate(), no HTTP, no async.

use crate::config::{Config, FleetConfig};
use crate::nodes::binary_store::{self, BinaryStore};
use anyhow::{bail, Context};
use std::path::{Path, PathBuf};

/// Result of a publish: the bytes were written, or an identical copy already existed.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Written(String),   // sha256
    Unchanged(String), // sha256 (idempotent: dest already held identical bytes)
}

impl Outcome {
    pub fn sha256(&self) -> &str {
        match self {
            Outcome::Written(s) | Outcome::Unchanged(s) => s,
        }
    }
}

/// Testable core: validate, resolve dest, hash, apply overwrite policy, copy.
fn publish_inner(
    source: &Path,
    publish_dir: &Path,
    version: &str,
    os: &str,
    arch: &str,
    force: bool,
) -> anyhow::Result<Outcome> {
    // 1. Validate components.
    if !binary_store::version_shaped(version) {
        bail!(
            "invalid version '{}': must match [A-Za-z0-9.-_+], no empty dot-segments",
            version
        );
    }
    if !binary_store::platform_shaped(os) {
        bail!(
            "invalid os '{}': must be lowercase alphanumeric/underscore only (e.g. linux, windows, macos)",
            os
        );
    }
    if !binary_store::platform_shaped(arch) {
        bail!(
            "invalid arch '{}': must be lowercase alphanumeric/underscore only (e.g. x86_64, aarch64)",
            arch
        );
    }

    // 2. Source must exist and be a file.
    if !source.exists() || !source.is_file() {
        bail!("source binary not found: {}", source.display());
    }

    // 3. Resolve destination path.
    let dest = binary_store::binary_path(publish_dir, version, os, arch)
        .context("internal: binary_path rejected validated components")?;

    // 4. Hash the source.
    let store = BinaryStore::new();
    let new_sha = store
        .sha256_of(source)
        .with_context(|| format!("hashing source {}", source.display()))?;

    // 5. Overwrite policy.
    if dest.exists() {
        let old_sha = store
            .sha256_of(&dest)
            .with_context(|| format!("hashing existing dest {}", dest.display()))?;
        if old_sha == new_sha {
            return Ok(Outcome::Unchanged(new_sha));
        }
        if !force {
            bail!(
                "refusing to overwrite {} (published sha {} != new {}); pass --force to replace",
                dest.display(),
                old_sha,
                new_sha
            );
        }
        tracing::warn!(
            "overwriting {} (sha {} → {}); gateway advertised sha is changing",
            dest.display(),
            old_sha,
            new_sha
        );
    }

    // 6. Ensure parent directory exists.
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating directory {}", parent.display()))?;
    }

    // 7. Copy. std::fs::copy preserves Unix +x bits on Unix.
    std::fs::copy(source, &dest).with_context(|| format!("copying to {}", dest.display()))?;

    Ok(Outcome::Written(new_sha))
}

/// Read-only fleet.publish_dir from a config file (no validate(), explicit-command error path).
fn publish_dir_from_config(path: &Path) -> anyhow::Result<Option<String>> {
    Config::from_file(path)
        .map(|c| c.fleet.publish_dir)
        .with_context(|| format!("could not read fleet.publish_dir from {}", path.display()))
}

/// CLI entry: resolve args, call publish_inner, print sha to stdout + narration to stderr.
pub fn run(args: crate::cli::PublishArgs) -> anyhow::Result<()> {
    // 1. Resolve OS and arch.
    let os = args.os.unwrap_or_else(|| std::env::consts::OS.to_string());
    let arch = args
        .arch
        .unwrap_or_else(|| std::env::consts::ARCH.to_string());

    // 2. Resolve source binary.
    let source = match args.binary {
        Some(p) => p,
        None => std::env::current_exe().context("could not determine current executable")?,
    };

    // 3. Resolve publish_dir: --publish-dir > HERD_AGENT_PUBLISH_DIR > config > default.
    let publish_dir: PathBuf = if let Some(dir) = args.publish_dir {
        dir
    } else {
        let cfg_val = match &args.config {
            Some(p) => publish_dir_from_config(p)?,
            None => None,
        };
        // The CLI has no running server context so we read HERD_DATA_DIR from
        // env only (the config-file data_dir field is not consulted here — env
        // is the container lever). This ensures `herd publish` writes to
        // {HERD_DATA_DIR}/binaries, matching where the gateway serves from.
        let data_root = crate::config::Config::data_dir_from(
            std::env::var("HERD_DATA_DIR").ok().as_deref(),
            None,
        );
        FleetConfig::publish_dir_from(
            std::env::var("HERD_AGENT_PUBLISH_DIR").ok().as_deref(),
            cfg_val.as_deref(),
            &data_root,
        )
    };

    // 4. Publish.
    let outcome = publish_inner(&source, &publish_dir, &args.version, &os, &arch, args.force)?;

    // 5. For the narration message, reconstruct the dest path (already validated above).
    let dest =
        binary_store::binary_path(&publish_dir, &args.version, &os, &arch).unwrap_or_default();

    // Scriptable sha on stdout.
    println!("{}", outcome.sha256());

    // Human-readable narration on stderr.
    match &outcome {
        Outcome::Written(_) => {
            eprintln!("published {}", dest.display());
        }
        Outcome::Unchanged(_) => {
            eprintln!(
                "already published (identical), no change: {}",
                dest.display()
            );
        }
    }
    eprintln!("  sha256: {}", outcome.sha256());
    eprintln!(
        "  -> now set fleet.target_agent_version: {} (config hot-reload picks it up; no restart)",
        args.version
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn tmp_dir(label: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("herd-pub-test-{}-{}", label, std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Write `content` to a temp file inside `dir` named `name`, return its path.
    fn write_file(dir: &Path, name: &str, content: &[u8]) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, content).unwrap();
        p
    }

    #[test]
    fn publishes_to_expected_layout() {
        let dir = tmp_dir("layout");
        let src = write_file(&dir, "herd", b"binary");

        let outcome = publish_inner(&src, &dir, "1.2.0", "linux", "x86_64", false).unwrap();
        let dest = dir.join("1.2.0").join("linux-x86_64").join("herd");
        assert!(dest.exists(), "dest should exist");
        assert!(matches!(outcome, Outcome::Written(_)));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn windows_dest_gets_exe_suffix() {
        let dir = tmp_dir("winsuffix");
        let src = write_file(&dir, "herd.exe", b"windows-binary");

        let outcome = publish_inner(&src, &dir, "1.2.0", "windows", "x86_64", false).unwrap();
        let dest = dir.join("1.2.0").join("windows-x86_64").join("herd.exe");
        assert!(dest.exists(), "herd.exe should exist");
        assert!(matches!(outcome, Outcome::Written(_)));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn returned_sha_matches_binarystore() {
        let dir = tmp_dir("sha");
        let src = write_file(&dir, "herd", b"abc");

        let outcome = publish_inner(&src, &dir, "1.2.0", "linux", "x86_64", false).unwrap();
        let dest = dir.join("1.2.0").join("linux-x86_64").join("herd");

        let expected = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        assert_eq!(outcome.sha256(), expected);
        assert_eq!(BinaryStore::new().sha256_of(&dest).unwrap(), expected);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn idempotent_rehash_same_bytes() {
        let dir = tmp_dir("idempotent");
        let src = write_file(&dir, "herd", b"same");

        let first = publish_inner(&src, &dir, "1.2.0", "linux", "x86_64", false).unwrap();
        assert!(matches!(first, Outcome::Written(_)));

        let second = publish_inner(&src, &dir, "1.2.0", "linux", "x86_64", false).unwrap();
        assert!(matches!(second, Outcome::Unchanged(_)));
        assert_eq!(first.sha256(), second.sha256());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn refuses_overwrite_differing_bytes_without_force() {
        let dir = tmp_dir("noforce");
        let src1 = write_file(&dir, "herd-v1", b"v1");
        publish_inner(&src1, &dir, "1.2.0", "linux", "x86_64", false).unwrap();

        let src2 = write_file(&dir, "herd-v2", b"v2");
        let result = publish_inner(&src2, &dir, "1.2.0", "linux", "x86_64", false);
        assert!(result.is_err(), "should refuse without --force");

        // Dest still holds v1 bytes.
        let dest = dir.join("1.2.0").join("linux-x86_64").join("herd");
        assert_eq!(std::fs::read(&dest).unwrap(), b"v1");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn force_overwrites_differing_bytes() {
        let dir = tmp_dir("force");
        let src1 = write_file(&dir, "herd-v1", b"v1");
        publish_inner(&src1, &dir, "1.2.0", "linux", "x86_64", false).unwrap();

        let src2 = write_file(&dir, "herd-v2", b"v2");
        let result = publish_inner(&src2, &dir, "1.2.0", "linux", "x86_64", true).unwrap();
        assert!(matches!(result, Outcome::Written(_)));

        let dest = dir.join("1.2.0").join("linux-x86_64").join("herd");
        assert_eq!(std::fs::read(&dest).unwrap(), b"v2");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_malformed_version() {
        let dir = tmp_dir("badver");
        // Need a real source file since validation happens before source check.
        let src = write_file(&dir, "herd", b"bin");

        let result = publish_inner(&src, &dir, "../etc", "linux", "x86_64", false);
        assert!(result.is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_malformed_os() {
        let dir = tmp_dir("bados");
        let src = write_file(&dir, "herd", b"bin");

        // Uppercase not allowed by platform_shaped
        let result = publish_inner(&src, &dir, "1.2.0", "LINUX", "x86_64", false);
        assert!(result.is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_malformed_arch() {
        let dir = tmp_dir("badarch");
        let src = write_file(&dir, "herd", b"bin");

        let result = publish_inner(&src, &dir, "1.2.0", "linux", "x86_64/..", false);
        assert!(result.is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_source_errors() {
        let dir = tmp_dir("nosrc");

        let absent = dir.join("does-not-exist");
        let result = publish_inner(&absent, &dir, "1.2.0", "linux", "x86_64", false);
        assert!(result.is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn creates_missing_parent_dirs() {
        let dir = tmp_dir("mkdirs");
        let src = write_file(&dir, "herd", b"nested");

        // The version/platform subdirs don't exist yet — publish_inner should create them.
        let outcome = publish_inner(&src, &dir, "2.0.0", "linux", "aarch64", false).unwrap();
        assert!(matches!(outcome, Outcome::Written(_)));
        let dest = dir.join("2.0.0").join("linux-aarch64").join("herd");
        assert!(dest.exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn config_publish_dir_resolution() {
        let dir = tmp_dir("cfgdir");
        let cfg_path = dir.join("herd.yaml");
        let publish_target = dir.join("binaries");
        std::fs::write(
            &cfg_path,
            format!("fleet:\n  publish_dir: {}\n", publish_target.display()),
        )
        .unwrap();

        let result = publish_dir_from_config(&cfg_path).unwrap();
        assert_eq!(result.as_deref(), Some(publish_target.to_str().unwrap()));

        // FleetConfig::publish_dir_from selects config value when env is None.
        let data_root = crate::config::Config::data_dir_from(None, None);
        let resolved =
            FleetConfig::publish_dir_from(None, Some(publish_target.to_str().unwrap()), &data_root);
        assert_eq!(resolved, publish_target);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
