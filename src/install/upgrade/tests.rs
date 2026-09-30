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
    // the shape `cona hooks install` actually writes: quoted absolute exe
    std::fs::write(
        dir.join("post-commit"),
        "#!/bin/sh\nmake lint\n'/opt/bin/cona' index --quiet 2>/dev/null &\n",
    )
    .unwrap();
    assert!(git_hooks_have(&dir, CONA_HOOK_NEEDLES));
    assert!(strip_git_hook_lines(&dir, CONA_HOOK_NEEDLES));
    assert_eq!(
        std::fs::read_to_string(dir.join("post-commit")).unwrap(),
        "#!/bin/sh\nmake lint\n"
    );
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

#[test]
fn latest_tag_comes_from_the_redirect_location() {
    let h = "HTTP/2 302\r\nserver: github.com\r\nLocation: https://github.com/ph1p/cona/releases/tag/v0.0.27\r\n\r\n";
    assert_eq!(tag_from_redirect(h).as_deref(), Some("0.0.27"));
    // No release yet → GitHub redirects to /releases, no tag.
    assert_eq!(
        tag_from_redirect("location: https://github.com/ph1p/cona/releases\r\n"),
        None
    );
    assert_eq!(
        tag_from_redirect("location: https://x/releases/tag/nightly\r\n"),
        None
    );
    assert_eq!(tag_from_redirect(""), None);
}

#[test]
fn index_newest_is_highest_version_not_last_line() {
    let idx = [
        r#"{"vers":"0.0.9","yanked":false}"#,
        r#"{"vers":"0.1.0","yanked":false}"#,
        r#"{"vers":"0.2.0","yanked":true}"#,
        r#"{"vers":"0.3.0-rc1","yanked":false}"#,
        r#"{"vers":"0.0.10","yanked":false}"#,
        "not json",
    ]
    .join("\n");
    assert_eq!(newest_in_index(&idx).as_deref(), Some("0.1.0"));
    assert_eq!(newest_in_index(""), None);
}

#[test]
fn remote_check_is_daily_after_success_hourly_after_failure() {
    let day = 86_400;
    assert!(check_due(day, 0, 0), "never checked");
    assert!(!check_due(day + 10, day, day), "answered just now");
    assert!(check_due(2 * day, day, day), "a day after success");
    // Last answer is old, last attempt failed 10 min ago → wait …
    assert!(!check_due(5 * day, day, 5 * day - 600));
    // … but an hour later try again, not a day later.
    assert!(check_due(5 * day, day, 5 * day - 3_600));
}
