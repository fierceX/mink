use super::*;

#[test]
fn read_baseline_is_lru_capped_at_max_paths() {
    // read_latest 基线按 MAX_PATHS 淘汰，长会话不得无界增长。
    let mut store = FileSnapshotStore::default();
    for idx in 0..(MAX_PATHS + 10) {
        let path = PathBuf::from(format!("missing-cap-{idx}.rs"));
        store.record(&path, &format!("content {idx}\n"), [1]);
    }
    assert!(store.read_latest_len() <= MAX_PATHS);
    // 最老的条目已被淘汰：基线查询返回 None。
    let oldest = PathBuf::from("missing-cap-0.rs");
    assert!(store.latest_read_snapshot(&oldest).is_none());
    // 最新的条目仍在。
    let newest = PathBuf::from(format!("missing-cap-{}.rs", MAX_PATHS + 9));
    assert!(store.latest_read_snapshot(&newest).is_some());
}

#[test]
fn record_edit_does_not_update_read_baseline() {
    let mut store = FileSnapshotStore::default();
    let path = PathBuf::from("missing-baseline.rs");
    let read_snapshot = store.record(&path, "original\n", [1]);
    let baseline = store.latest_read_snapshot(&path).expect("baseline exists");
    assert_eq!(baseline.text, "original\n");
    assert_eq!(baseline.tag, read_snapshot.tag);
    store.record_edit(&path, "edited\n", [1]);
    let baseline_after_edit = store.latest_read_snapshot(&path).expect("baseline kept");
    assert_eq!(
        baseline_after_edit.text, "original\n",
        "edit must not clobber baseline"
    );
}

#[test]
fn identical_content_reuses_version_and_merges_seen_lines() {
    let mut store = FileSnapshotStore::default();
    let path = PathBuf::from("missing-a.rs");
    let first = store.record(&path, "a\nb\n", [1]);
    let second = store.record(&path, "a\nb\n", [2]);
    assert_eq!(first.tag, second.tag);
    let stored = store.versions(&path, &first.tag);
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].seen_lines, BTreeSet::from([1, 2]));
}

#[test]
fn snapshot_preserves_bom_and_crlf_shape() {
    let mut store = FileSnapshotStore::default();
    let path = PathBuf::from("shape.rs");
    store.record(&path, "\u{feff}a\r\nb\r\n", [1, 2]);
    let snapshot = store.latest_read_snapshot(&path).expect("baseline exists");
    assert_eq!(snapshot.text, "a\nb\n");
    assert!(snapshot.bom);
    assert!(snapshot.crlf);
    assert_eq!(
        crate::tools::snapshot::TextShape {
            bom: snapshot.bom,
            crlf: snapshot.crlf,
        }
        .restore(&snapshot.text),
        "\u{feff}a\r\nb\r\n"
    );
}

#[test]
fn text_shape_matrix_round_trips_bom_crlf_and_plain_inputs() {
    // The single format matrix for TextShape: normal edits, snapshots and
    // signal rollback all trust these rules.
    let cases = [
        (
            "\u{feff}a\r\nb\r\n",
            true,
            true,
            "a\nb\n",
            "\u{feff}a\r\nb\r\n",
        ),
        ("a\r\nb\r\n", false, true, "a\nb\n", "a\r\nb\r\n"),
        ("a\r\nb\nc", false, false, "a\nb\nc", "a\nb\nc"),
        ("a\nb\n", false, false, "a\nb\n", "a\nb\n"),
        ("", false, false, "", ""),
        ("hello", false, false, "hello", "hello"),
        (
            "\u{feff}no-newline",
            true,
            false,
            "no-newline",
            "\u{feff}no-newline",
        ),
        // Bare CR (old-Mac) is normalized to LF and never treated as CRLF.
        ("a\rb\rc", false, false, "a\nb\nc", "a\nb\nc"),
    ];
    for (raw, bom, crlf, normalized_expected, restored_expected) in cases {
        let (shape, normalized) = crate::tools::snapshot::TextShape::decode(raw);
        assert_eq!(shape.bom, bom, "bom for {raw:?}");
        assert_eq!(shape.crlf, crlf, "crlf for {raw:?}");
        assert_eq!(normalized, normalized_expected, "normalized for {raw:?}");
        assert_eq!(
            shape.restore(&normalized),
            restored_expected,
            "restored for {raw:?}"
        );
    }
}

#[test]
fn tag_ignores_crlf_and_trailing_horizontal_space() {
    assert_eq!(compute_file_tag("a  \r\nb\r\n"), compute_file_tag("a\nb\n"));
}

#[test]
fn named_clipboard_survives_independent_snapshot_records() {
    let mut store = FileSnapshotStore::default();
    store.set_named_clipboard(BTreeMap::from([("saved".into(), vec!["x".into()])]));
    store.record(Path::new("a"), "a", [1]);
    assert_eq!(
        store.named_clipboard().get("saved"),
        Some(&vec!["x".to_string()])
    );
}
