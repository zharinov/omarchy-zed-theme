//! Publishes a theme only from a stable source palette.
//!
//! A theme update holds one lock, verifies source identity after validation,
//! rejects a symlink at the destination, and atomically replaces changed output.

use crate::palette::resolve_palette;
use crate::theme::{Audit, build_theme};
use crate::{Error, Result};
use fs2::FileExt;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static TEMPORARY_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SourceIdentity {
    device: u64,
    inode: u64,
    size: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
}

fn source_identity(path: &Path) -> Result<SourceIdentity> {
    let metadata = fs::metadata(path)
        .map_err(|error| Error(format!("cannot inspect {}: {error}", path.display())))?;
    if !metadata.is_file() {
        return Err(Error(format!(
            "colors.toml is not a regular file: {}",
            path.display()
        )));
    }

    Ok(SourceIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
        size: metadata.len(),
        modified_seconds: metadata.mtime(),
        modified_nanoseconds: metadata.mtime_nsec(),
        changed_seconds: metadata.ctime(),
        changed_nanoseconds: metadata.ctime_nsec(),
    })
}

fn replaceable_target(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(Error(format!(
            "refusing to replace symlink target: {}",
            path.display()
        ))),
        Ok(metadata) if !metadata.is_file() => Err(Error(format!(
            "refusing non-regular target: {}",
            path.display()
        ))),
        Ok(_) => Ok(()),
        Err(ref error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

pub(crate) fn read_regular_nofollow(path: &Path) -> Result<Option<Vec<u8>>> {
    let mut file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(Error(format!(
                "cannot read existing target {}: {error}",
                path.display()
            )));
        }
    };

    if !file.metadata()?.is_file() {
        return Err(Error(format!(
            "refusing non-regular target: {}",
            path.display()
        )));
    }

    let mut content = Vec::new();
    file.read_to_end(&mut content)?;

    Ok(Some(content))
}

#[derive(Clone, Copy)]
enum ExpectedContent<'a> {
    Any,
    Exact(Option<&'a [u8]>),
}

fn atomic_write_file_inner(
    target: &Path,
    content: &[u8],
    expected: ExpectedContent<'_>,
) -> Result<Option<bool>> {
    let parent = target
        .parent()
        .ok_or_else(|| Error("target has no parent".into()))?;
    fs::create_dir_all(parent)?;
    replaceable_target(target)?;

    let current = read_regular_nofollow(target)?;
    if let ExpectedContent::Exact(expected) = expected
        && current.as_deref() != expected
    {
        return Ok(None);
    }
    if current.as_deref() == Some(content) {
        return Ok(Some(false));
    }

    let sequence = TEMPORARY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temporary = parent.join(format!(
        ".omarchy-zed-theme-{}-{sequence}.tmp",
        std::process::id()
    ));
    let result = (|| -> Result<bool> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(content)?;
        file.sync_all()?;
        replaceable_target(target)?;
        if let ExpectedContent::Exact(expected) = expected
            && read_regular_nofollow(target)?.as_deref() != expected
        {
            return Ok(false);
        }
        fs::rename(&temporary, target)?;
        File::open(parent)?.sync_all()?;
        Ok(true)
    })();

    if !result.as_ref().is_ok_and(|written| *written) {
        let _ = fs::remove_file(&temporary);
    }

    result.map(|written| written.then_some(true))
}

pub fn atomic_write_file(target: &Path, content: &[u8]) -> Result<bool> {
    atomic_write_file_inner(target, content, ExpectedContent::Any)?
        .ok_or_else(|| Error("unconditional atomic write was not attempted".into()))
}

pub fn atomic_write_file_if_unchanged(
    target: &Path,
    expected: Option<&[u8]>,
    content: &[u8],
) -> Result<Option<bool>> {
    atomic_write_file_inner(target, content, ExpectedContent::Exact(expected))
}

pub struct ThemeUpdate {
    pub target: PathBuf,
    pub audit: Audit,
    pub changed: bool,
}

fn publish_if_source_unchanged(
    colors_file: &Path,
    expected: SourceIdentity,
    target: &Path,
    content: &[u8],
) -> Result<Option<bool>> {
    if source_identity(colors_file)? != expected {
        return Ok(None);
    }

    let changed = atomic_write_file(target, content)?;
    Ok(Some(changed))
}

pub fn generate_and_publish(
    colors_file: &Path,
    output_directory: Option<&Path>,
    resolver: Option<&Path>,
    appearance_assertion: Option<&str>,
) -> Result<ThemeUpdate> {
    let destination =
        output_directory.unwrap_or_else(|| colors_file.parent().unwrap_or_else(|| Path::new(".")));
    let target = destination.join("omarchy.json");
    let lock_parent = destination.parent().unwrap_or(destination);

    fs::create_dir_all(lock_parent)?;

    let lock_path = lock_parent.join(".omarchy-zed-theme.lock");
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(&lock_path)
        .map_err(|error| {
            Error(format!(
                "cannot open generation lock {}: {error}",
                lock_path.display()
            ))
        })?;

    if !lock.metadata()?.is_file() {
        return Err(Error(format!(
            "generation lock is not a regular file: {}",
            lock_path.display()
        )));
    }

    lock.lock_exclusive()
        .map_err(|error| Error(format!("cannot lock generation: {error}")))?;

    for _ in 0..5 {
        let before = source_identity(colors_file)?;
        let palette = resolve_palette(colors_file, resolver)?;
        let after_resolve = source_identity(colors_file)?;

        if before != after_resolve {
            continue;
        }

        if appearance_assertion.is_some_and(|appearance| appearance != palette.mode) {
            return Err(Error(format!(
                "appearance assertion {:?} disagrees with resolved mode {:?}",
                appearance_assertion.unwrap(),
                palette.mode
            )));
        }

        let (document, audit) = build_theme(&palette)?;
        let mut content = serde_json::to_vec_pretty(&document)?;
        content.push(b'\n');

        let Some(changed) =
            publish_if_source_unchanged(colors_file, after_resolve, &target, &content)?
        else {
            continue;
        };

        return Ok(ThemeUpdate {
            target,
            audit,
            changed,
        });
    }

    Err(Error(format!(
        "colors.toml did not remain stable after 5 attempts: {}",
        colors_file.display()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temporary(name: &str) -> PathBuf {
        let sequence = TEMPORARY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "omarchy-zed-theme-publish-test-{}-{sequence}-{name}",
            std::process::id()
        ))
    }

    #[test]
    fn changed_source_prevents_theme_publication() {
        let root = temporary("changed-source");
        fs::create_dir_all(&root).unwrap();
        let colors = root.join("colors.toml");
        let target = root.join("themes/omarchy.json");
        fs::write(&colors, b"before").unwrap();
        let expected = source_identity(&colors).unwrap();
        fs::write(&colors, b"after with a different size").unwrap();

        let changed =
            publish_if_source_unchanged(&colors, expected, &target, b"stale theme").unwrap();

        assert_eq!(changed, None);
        assert!(!target.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn identical_atomic_output_is_not_rewritten() {
        let root = temporary("identical-output");
        let target = root.join("themes/omarchy.json");
        assert!(atomic_write_file(&target, b"same").unwrap());
        assert!(!atomic_write_file(&target, b"same").unwrap());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn conditional_atomic_output_rejects_a_stale_snapshot() {
        let root = temporary("stale-output");
        let target = root.join("settings.json");
        fs::create_dir_all(&root).unwrap();
        fs::write(&target, b"newer").unwrap();

        let result = atomic_write_file_if_unchanged(&target, Some(b"older"), b"managed").unwrap();

        assert_eq!(result, None);
        assert_eq!(fs::read(&target).unwrap(), b"newer");
        fs::remove_dir_all(root).unwrap();
    }
}
