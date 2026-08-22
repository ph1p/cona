use super::*;

fn marker(name: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("cona-test-{name}-{}.indexing", std::process::id()));
    let _ = std::fs::remove_file(&p);
    p
}

#[test]
fn index_lock_excludes_a_second_holder() {
    let p = marker("lock-excl");
    let first = IndexLock::at(&p).expect("first acquire wins");
    assert!(
        IndexLock::at(&p).is_none(),
        "second acquire must be refused"
    );
    drop(first);
    // Released on drop, so the next session can index again.
    assert!(IndexLock::at(&p).is_some(), "lock must free on drop");
    let _ = std::fs::remove_file(&p);
}

#[test]
fn index_lock_reclaims_a_stale_marker() {
    // A marker from a killed process must not wedge indexing forever, and a
    // fresh one must still exclude. Age is judged by `marker_is_stale`, so
    // the policy is checked without having to backdate a real file.
    assert!(!IndexLock::marker_is_stale(Some(
        std::time::Duration::from_secs(1)
    )));
    assert!(IndexLock::marker_is_stale(Some(
        std::time::Duration::from_secs(IndexLock::STALE_SECS + 1)
    )));
    // An unreadable/absurd mtime (clock skew makes `elapsed` fail) counts as
    // stale: better one duplicate walk than indexing wedged for good.
    assert!(IndexLock::marker_is_stale(None));
}

#[test]
fn baseline_windows_not_whole_file() {
    // 500 lines of 80 chars: whole file ≈ (500*81)/4 ≈ 10125 tok.
    let lens = vec![80usize; 500];
    let whole = est_tokens(500 * 80 + 500);
    // one hit in the middle → only a ±40 window (81 lines) counts.
    let one = baseline_tokens(&lens, &[250]);
    assert!(one < whole / 4, "one hit {one} vs whole {whole}");
    let expect_win = est_tokens(81 * 80 + 81);
    assert_eq!(one, expect_win);
    // empty hits ⇒ whole file (agent scanned everything, found no anchor).
    assert_eq!(baseline_tokens(&lens, &[]), whole);
    // adjacent hits merge into one window, not double-counted.
    let merged = baseline_tokens(&lens, &[250, 251, 252]);
    assert!(merged < one * 2, "merged {merged} vs 2×one {}", one * 2);
    // out-of-range hits are ignored (fail-safe, never panics).
    assert_eq!(baseline_tokens(&lens, &[9999]), whole);
    assert_eq!(baseline_tokens(&[], &[1]), 0);
}

#[test]
fn maintenance_cmds_classified() {
    for cmd in ["index", "edit", "hook:read-block"] {
        assert!(is_maintenance_cmd(cmd), "{cmd}");
    }
    for cmd in ["tree", "outline", "find", "show", "refs"] {
        assert!(!is_maintenance_cmd(cmd), "{cmd}");
    }
}

#[test]
fn ephemeral_paths_classified() {
    // The system temp dir is ephemeral on every platform.
    let tmp = std::env::temp_dir().join("cona-it-1");
    assert!(is_ephemeral_path(&tmp), "{}", tmp.display());

    // Hard-coded unix roots only classify as ephemeral on unix.
    #[cfg(unix)]
    for p in [
        "/tmp/ltest",
        "/private/tmp/it2",
        "/private/var/folders/6q/x/T/cona-git-123",
    ] {
        assert!(is_ephemeral_path(Path::new(p)), "{p}");
    }
    for p in ["/Users/u/dev/proj", "/Volumes/ext/repo", "/home/u/tmp/x"] {
        assert!(!is_ephemeral_path(Path::new(p)), "{p}");
    }
}

#[test]
fn usage_detail_migration_is_idempotent() {
    // simulate an old global.db whose usage table predates the `detail` column
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE usage(id INTEGER PRIMARY KEY, ts INTEGER, tokens_saved INTEGER)",
    )
    .unwrap();
    assert!(!column_exists(&conn, "usage", "detail").unwrap());
    // running the guarded migration twice must be safe
    for _ in 0..2 {
        if !column_exists(&conn, "usage", "detail").unwrap() {
            conn.execute_batch("ALTER TABLE usage ADD COLUMN detail TEXT NOT NULL DEFAULT ''")
                .unwrap();
        }
    }
    assert!(column_exists(&conn, "usage", "detail").unwrap());
    conn.execute(
        "INSERT INTO usage(ts, tokens_saved, detail) VALUES(1, 2, 'sym')",
        [],
    )
    .unwrap();
    let d: String = conn
        .query_row("SELECT detail FROM usage", [], |r| r.get(0))
        .unwrap();
    assert_eq!(d, "sym");
}

#[test]
fn totals_baseline_and_pct() {
    let t = Totals {
        calls: 3,
        tokens_out: 100,
        tokens_saved: 900,
        reads_blocked: 1,
        total_ms: 5,
    };
    assert_eq!(t.baseline(), 1000);
    assert!((t.pct_saved() - 90.0).abs() < 1e-9);
    assert_eq!(Totals::default().pct_saved(), 0.0);
}

#[test]
fn human_bytes_scales() {
    assert_eq!(human_bytes(512), "512 B");
    assert_eq!(human_bytes(2048), "2.0 KB");
    assert!(human_bytes(5 * 1024 * 1024).ends_with("MB"));
}

#[test]
fn usage_row_cap_deletes_oldest() {
    // the tidy row-cap query must drop the OLDEST rows down to the cap
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("CREATE TABLE usage(id INTEGER PRIMARY KEY, ts INTEGER)")
        .unwrap();
    for i in 0..10 {
        conn.execute("INSERT INTO usage(ts) VALUES(?1)", [i])
            .unwrap();
    }
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM usage", [], |r| r.get(0))
        .unwrap();
    let max = 4;
    let deleted = conn
        .execute(
            "DELETE FROM usage WHERE id IN (SELECT id FROM usage ORDER BY id ASC LIMIT ?1)",
            [count - max],
        )
        .unwrap();
    assert_eq!(deleted, 6);
    let remaining: i64 = conn
        .query_row("SELECT COUNT(*) FROM usage", [], |r| r.get(0))
        .unwrap();
    assert_eq!(remaining, 4);
    let min_id: i64 = conn
        .query_row("SELECT MIN(id) FROM usage", [], |r| r.get(0))
        .unwrap();
    assert_eq!(min_id, 7); // oldest ids 1..=6 gone, newest 7..=10 kept
}
