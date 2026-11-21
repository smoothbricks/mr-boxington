//! Validate build-storage placement before starting a cache session.

use crate::config::Config;
use eyre::{Context, Result};
use std::io;
use std::path::{Path, PathBuf};

pub(crate) fn check_cache(config: &Config) -> Result<()> {
    // Check existing submounts and symlinks as well as the configured root.
    for path in [
        config.cache_dir.clone(),
        config.store_dir(),
        config.cache_dir.join("incremental"),
        config.cache_dir.join("scheduler"),
        config.cache_dir.join("cargo-roots"),
        config.cache_dir.join("tools"),
    ] {
        require_local(&path, "mbx cache directory", "MBX_CACHE_DIR")?;
    }
    require_local(&config.shims_dir, "mbx shim directory", "MBX_SHIMS_DIR")
}

pub(crate) fn require_local(path: &Path, role: &str, setting: &str) -> Result<()> {
    require_local_with(path, role, setting, is_nfs)
}

fn require_local_with(
    path: &Path,
    role: &str,
    setting: &str,
    probe: impl FnOnce(&Path) -> io::Result<bool>,
) -> Result<()> {
    require_local_observed(path, role, setting, probe, &mut |_, _| {})
}

/// Which of the two looks [`existing_directory`] takes at a path comes next:
/// what kind of entry it is, then where that entry leads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Look {
    Kind,
    Destination,
}

/// [`require_local_with`], telling `before` each look at each path first.
fn require_local_observed(
    path: &Path,
    role: &str,
    setting: &str,
    probe: impl FnOnce(&Path) -> io::Result<bool>,
    before: &mut dyn FnMut(&Path, Look),
) -> Result<()> {
    let absolute = std::path::absolute(path)?;
    let existing = existing_directory(&absolute, 0, before)
        .wrap_err_with(|| format!("could not inspect {role} {}", path.display()))?;
    if probe(&existing).wrap_err_with(|| {
        format!(
            "could not identify the filesystem for {role} {}",
            path.display()
        )
    })? {
        eyre::bail!(
            "{role} {} is on NFS, which is unsupported for build storage. Set {setting} to a local filesystem. Remote caches remain supported.",
            path.display()
        );
    }
    Ok(())
}

/// Follow even dangling directory links before choosing an existing ancestor.
/// `Path::exists` alone would mistake a dangling link to NFS for local storage.
///
/// Each path is looked at twice: for what kind of entry it is, then for
/// where that entry leads. Commands of one checkout share its view lease, so
/// one may move a real `target/` into the view, link it, or undo its link
/// between another's two looks. A path that is gone at the second look has
/// changed kind, and is looked at again as what it is now: followed if it
/// became a link, an ancestor if it is absent. That takes a step of the same
/// bound as following a link, so a path that never holds still is refused.
/// Any other failure of the second look is an error.
fn existing_directory(
    path: &Path,
    depth: usize,
    before: &mut dyn FnMut(&Path, Look),
) -> io::Result<PathBuf> {
    if depth > 256 {
        return Err(io::Error::other(
            "too many directory or symbolic-link components",
        ));
    }
    before(path, Look::Kind);
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_symlink() => {
            before(path, Look::Destination);
            let destination = match std::fs::read_link(path) {
                Ok(destination) => destination,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    return existing_directory(path, depth + 1, before);
                }
                Err(error) => return Err(error),
            };
            let destination = if destination.is_absolute() {
                destination
            } else {
                path.parent().unwrap_or(Path::new(".")).join(destination)
            };
            existing_directory(&destination, depth + 1, before)
        }
        Ok(metadata) if metadata.is_dir() => {
            before(path, Look::Destination);
            match std::fs::canonicalize(path) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    existing_directory(path, depth + 1, before)
                }
                canonical => canonical,
            }
        }
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            "not a directory",
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let parent = path
                .parent()
                .filter(|parent| *parent != path)
                .ok_or(error)?;
            existing_directory(parent, depth + 1, before)
        }
        Err(error) => Err(error),
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn is_nfs(path: &Path) -> io::Result<bool> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes())?;
    let mut stats = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: path is NUL-terminated and stats has the ABI's size and alignment.
    if unsafe { libc::statfs(path.as_ptr(), stats.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: statfs initialized stats on success.
    let stats = unsafe { stats.assume_init() };
    #[cfg(target_os = "linux")]
    {
        Ok(stats.f_type == 0x6969)
    }
    #[cfg(target_os = "macos")]
    {
        Ok(stats.f_fstypename[..4]
            == [
                b'n' as libc::c_char,
                b'f' as libc::c_char,
                b's' as libc::c_char,
                0,
            ])
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn is_nfs(_path: &Path) -> io::Result<bool> {
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_nfs_with_the_path_and_remedy_without_creating_storage() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("not/created");
        let error = require_local_with(&path, "mbx cache directory", "MBX_CACHE_DIR", |existing| {
            assert_eq!(existing, temp.path().canonicalize().unwrap());
            Ok(true)
        })
        .unwrap_err()
        .to_string();
        assert!(error.contains(&path.display().to_string()));
        assert!(error.contains("NFS"));
        assert!(error.contains("MBX_CACHE_DIR"));
        assert!(!path.exists());
    }

    #[test]
    fn accepts_local_storage_and_reports_inspection_errors() {
        let temp = tempfile::tempdir().unwrap();
        require_local_with(temp.path(), "cache", "MBX_CACHE_DIR", |_| Ok(false)).unwrap();
        let error = require_local_with(temp.path(), "cache", "MBX_CACHE_DIR", |_| {
            Err(io::Error::from(io::ErrorKind::PermissionDenied))
        })
        .unwrap_err();
        assert!(format!("{error:#}").contains("permission denied"));
        let file = temp.path().join("file");
        std::fs::write(&file, "data").unwrap();
        assert!(
            require_local_with(&file.join("child"), "cache", "setting", |_| Ok(false)).is_err()
        );
    }

    #[test]
    #[cfg(unix)]
    fn follows_links_in_both_directions_including_dangling_destinations() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let local = temp.path().join("local");
        let nfs = temp.path().join("nfs");
        std::fs::create_dir(&local).unwrap();
        std::fs::create_dir(&nfs).unwrap();
        let nfs = nfs.canonicalize().unwrap();
        let probe = |path: &Path| Ok(path.starts_with(&nfs));
        symlink("../nfs/missing", local.join("out")).unwrap();
        assert!(require_local_with(&local.join("out/deeper"), "target", "setting", probe).is_err());
        symlink(&local, nfs.join("target")).unwrap();
        require_local_with(&nfs.join("target/missing"), "target", "setting", probe).unwrap();
        symlink("loop", local.join("loop")).unwrap();
        assert!(require_local_with(&local.join("loop"), "target", "setting", probe).is_err());
    }

    /// A checkout with a target path, a place for its managed view, and a
    /// directory the probe calls NFS.
    #[cfg(unix)]
    struct Placement {
        _temp: tempfile::TempDir,
        root: PathBuf,
        target: PathBuf,
        view: PathBuf,
        nfs: PathBuf,
    }

    #[cfg(unix)]
    impl Placement {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().canonicalize().unwrap();
            for directory in ["checkout", "views", "nfs"] {
                std::fs::create_dir(root.join(directory)).unwrap();
            }
            Self {
                target: root.join("checkout/target"),
                view: root.join("views/view"),
                nfs: root.join("nfs"),
                root,
                _temp: temp,
            }
        }

        /// Check the target while `change` runs once, between the two looks
        /// at it. Returns the directory the probe was asked about.
        fn checked_while(&self, mut change: impl FnMut()) -> Result<PathBuf> {
            let mut probed = None;
            let mut pending = true;
            require_local_observed(
                &self.target,
                "Cargo target directory",
                "CARGO_TARGET_DIR or build.target-dir",
                |existing| {
                    probed = Some(existing.to_path_buf());
                    Ok(existing.starts_with(&self.nfs))
                },
                &mut |path, look| {
                    if pending && path == self.target && look == Look::Destination {
                        pending = false;
                        change();
                    }
                },
            )?;
            Ok(probed.unwrap())
        }
    }

    /// Another command of the same checkout moves its real `target/` into the
    /// managed view and links it while this one is checking it.
    #[test]
    #[cfg(unix)]
    fn a_target_adopted_while_it_is_checked_is_followed_into_its_view() {
        use std::os::unix::fs::symlink;
        let placed = Placement::new();
        std::fs::create_dir(&placed.target).unwrap();
        let probed = placed
            .checked_while(|| {
                std::fs::rename(&placed.target, &placed.view).unwrap();
                symlink(&placed.view, &placed.target).unwrap();
            })
            .unwrap();
        assert_eq!(probed, placed.view);

        // Renamed away, and the link not placed yet.
        let placed = Placement::new();
        std::fs::create_dir(&placed.target).unwrap();
        let probed = placed
            .checked_while(|| std::fs::rename(&placed.target, &placed.view).unwrap())
            .unwrap();
        assert_eq!(probed, placed.root.join("checkout"));

        // Linked to a view placement has not created yet.
        let placed = Placement::new();
        std::fs::create_dir(&placed.target).unwrap();
        let probed = placed
            .checked_while(|| {
                std::fs::remove_dir(&placed.target).unwrap();
                symlink(&placed.view, &placed.target).unwrap();
            })
            .unwrap();
        assert_eq!(probed, placed.root.join("views"));
    }

    /// A placement that undoes its own link, or replaces an outdated one,
    /// while another command is checking the target.
    #[test]
    #[cfg(unix)]
    fn a_target_link_removed_while_it_is_checked_reads_as_not_placed() {
        use std::os::unix::fs::symlink;
        let placed = Placement::new();
        std::fs::create_dir(&placed.view).unwrap();
        symlink(&placed.view, &placed.target).unwrap();
        let probed = placed
            .checked_while(|| std::fs::remove_file(&placed.target).unwrap())
            .unwrap();
        assert_eq!(probed, placed.root.join("checkout"));
    }

    #[test]
    #[cfg(unix)]
    fn a_target_moved_onto_nfs_while_it_is_checked_is_still_refused() {
        use std::os::unix::fs::symlink;
        for destination in ["target", "not/created"] {
            let placed = Placement::new();
            std::fs::create_dir(&placed.target).unwrap();
            let remote = placed.nfs.join(destination);
            let error = placed
                .checked_while(|| {
                    std::fs::remove_dir(&placed.target).unwrap();
                    if destination == "target" {
                        std::fs::create_dir(&remote).unwrap();
                    }
                    symlink(&remote, &placed.target).unwrap();
                })
                .unwrap_err();
            assert!(error.to_string().contains("is on NFS"), "{error:#}");
        }
    }

    /// Only a path that is gone at the second look is looked at again.
    #[test]
    #[cfg(unix)]
    fn a_target_that_stops_being_a_link_while_it_is_checked_is_an_error() {
        use std::os::unix::fs::symlink;
        let placed = Placement::new();
        std::fs::create_dir(&placed.view).unwrap();
        symlink(&placed.view, &placed.target).unwrap();
        let error = placed
            .checked_while(|| {
                std::fs::remove_file(&placed.target).unwrap();
                std::fs::create_dir(&placed.target).unwrap();
            })
            .unwrap_err();
        let cause = error.downcast_ref::<io::Error>().unwrap();
        assert_eq!(cause.kind(), io::ErrorKind::InvalidInput, "{error:#}");
        assert!(error.to_string().contains("could not inspect"));
    }

    /// A path that is there at every first look and gone at every second
    /// uses up the walk's bound instead of holding the command forever.
    #[test]
    fn a_target_that_never_holds_still_is_refused_within_the_bound() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().canonicalize().unwrap().join("target");
        let mut looks = 0;
        let error = require_local_observed(
            &target,
            "Cargo target directory",
            "CARGO_TARGET_DIR or build.target-dir",
            |_| Ok(false),
            &mut |path, look| {
                if path != target {
                    return;
                }
                looks += 1;
                match look {
                    Look::Kind => std::fs::create_dir(&target).unwrap(),
                    Look::Destination => std::fs::remove_dir(&target).unwrap(),
                }
            },
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("too many directory or symbolic-link components"),
            "{error:#}"
        );
        assert_eq!(looks, 2 * 257);
    }

    #[test]
    fn private_shims_are_checked_instead_of_the_unused_default_directory() {
        let directory = tempfile::tempdir().unwrap();
        let cache = directory.path().join("cache");
        std::fs::create_dir(&cache).unwrap();
        std::fs::write(cache.join("shims"), "not a directory").unwrap();
        let mut config = Config::for_test(&cache);
        config.shims_dir = directory.path().join("private shims");
        check_cache(&config).unwrap();
        std::fs::write(&config.shims_dir, "not a directory").unwrap();
        let error = check_cache(&config).unwrap_err();
        assert!(format!("{error:#}").contains("mbx shim directory"));
    }

    #[test]
    fn probes_the_local_test_filesystem() {
        let temp = tempfile::tempdir().unwrap();
        assert!(!is_nfs(temp.path()).unwrap());
    }
}
