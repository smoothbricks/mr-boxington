use super::*;

/// 1,500 unlinks at 1,000 a second, after a second's burst, take at least
/// half a second.
#[test]
fn a_paced_process_keeps_to_the_pace_after_a_seconds_burst() {
    let _paced = pace();
    let started = Instant::now();
    for _ in 0..UNLINKS_PER_SECOND + UNLINKS_PER_SECOND / 2 {
        wait();
    }
    let elapsed = started.elapsed();
    assert!(elapsed >= Duration::from_millis(500), "took {elapsed:?}");
}

#[cfg(unix)]
#[test]
fn removing_a_tree_never_follows_a_link_inside_it() {
    let directory = tempfile::tempdir().unwrap();
    let outside = directory.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("kept"), b"kept").unwrap();
    let tree = directory.path().join("tree");
    std::fs::create_dir_all(tree.join("nested/deeper")).unwrap();
    std::fs::write(tree.join("nested/deeper/file"), b"gone").unwrap();
    std::fs::write(tree.join("top"), b"gone").unwrap();
    std::os::unix::fs::symlink(&outside, tree.join("nested/link")).unwrap();

    remove_dir_all(&tree).unwrap();

    assert!(!tree.exists());
    assert_eq!(std::fs::read(outside.join("kept")).unwrap(), b"kept");
}

#[cfg(unix)]
#[test]
fn a_link_at_the_root_goes_as_a_link() {
    let directory = tempfile::tempdir().unwrap();
    let outside = directory.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("kept"), b"kept").unwrap();
    let link = directory.path().join("link");
    std::os::unix::fs::symlink(&outside, &link).unwrap();

    remove_dir_all(&link).unwrap();

    assert!(std::fs::symlink_metadata(&link).is_err());
    assert_eq!(std::fs::read(outside.join("kept")).unwrap(), b"kept");
}

#[cfg(unix)]
#[test]
fn a_file_at_the_root_is_not_a_tree() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("file");
    std::fs::write(&file, b"kept").unwrap();

    assert!(remove_dir_all(&file).is_err());
    assert_eq!(std::fs::read(&file).unwrap(), b"kept");
}
