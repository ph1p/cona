use super::*;

#[test]
fn semver_parse_and_compare() {
    assert_eq!(parse_semver("0.1.1"), Some((0, 1, 1)));
    assert_eq!(parse_semver("v1.2.3"), Some((1, 2, 3)));
    assert_eq!(parse_semver("1.2.3-rc1"), Some((1, 2, 3)));
    assert_eq!(parse_semver("nope"), None);
    assert!(remote_is_newer("0.1.2", "0.1.1"));
    assert!(remote_is_newer("0.2.0", "0.1.9"));
    assert!(!remote_is_newer("0.1.1", "0.1.1"));
    assert!(!remote_is_newer("0.1.0", "0.1.1"));
    assert!(!remote_is_newer("garbage", "0.1.1"));
}

#[test]
fn git_hooks_have_detects_cona_lines() {
    let dir = std::env::temp_dir().join("cona-hookhave-test");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    assert!(!git_hooks_have(&dir, CONA_HOOK_NEEDLES));
    std::fs::write(
        dir.join("post-commit"),
        "#!/bin/sh\nexec cona index --quiet\n",
    )
    .unwrap();
    assert!(git_hooks_have(&dir, CONA_HOOK_NEEDLES));
    // a foreign hook must not match
    std::fs::write(dir.join("post-commit"), "#!/bin/sh\nmake lint\n").unwrap();
    assert!(!git_hooks_have(&dir, CONA_HOOK_NEEDLES));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn tree_dirty_tracks_working_changes() {
    let repo = std::env::temp_dir().join("cona-dirtytest-repo");
    let _ = std::fs::remove_dir_all(&repo);
    std::fs::create_dir_all(&repo).unwrap();
    if !git_ok(&repo, &["init", "--quiet"]) {
        return; // no usable git in this environment — skip
    }
    git_ok(&repo, &["config", "user.email", "t@t"]);
    git_ok(&repo, &["config", "user.name", "t"]);
    std::fs::write(repo.join("f.txt"), "one\n").unwrap();
    git_ok(&repo, &["add", "f.txt"]);
    git_ok(&repo, &["commit", "--quiet", "-m", "init"]);

    // Clean tree after commit.
    assert!(!tree_dirty(&repo));
    // Modify a tracked file → dirty.
    std::fs::write(repo.join("f.txt"), "two\n").unwrap();
    assert!(tree_dirty(&repo));

    let _ = std::fs::remove_dir_all(&repo);
}
