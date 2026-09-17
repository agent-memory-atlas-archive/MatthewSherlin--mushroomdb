//! `BatchBuilder::insert_node_on_conflict` — the engine half of v0.6.10 Task 6.
//!
//! A mirror rebuild needs to write a frame onto a store that already has
//! content without wiping it first. `OnConflict` says what a taken key means:
//! refuse the frame (today's answer), leave the stored node alone, or make its
//! properties exactly the supplied ones.
use core_api::{BatchOp, Direction, GraphDb, GraphError, OnConflict, Value};

fn tmp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!(
        "graphdb-onconflict-{}-{}",
        name,
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn s(v: &str) -> Value {
    Value::Str(v.into())
}

// ── error: the 0.6.9 answer, unchanged ───────────────────────────────────────

#[test]
fn error_policy_rejects_the_whole_frame() {
    let dir = tmp("error");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("Doc", "a", vec![("title".into(), s("one"))])
        .unwrap();

    let err = {
        let mut b = db.batch();
        b.insert_node_on_conflict(
            "Doc",
            "a",
            vec![("title".into(), s("two"))],
            OnConflict::Error,
        );
        b.insert_node_on_conflict("Doc", "b", vec![], OnConflict::Error);
        b.commit_outcome().unwrap_err()
    };
    assert!(
        matches!(&err, GraphError::DuplicateKey { key } if key == "a"),
        "expected DuplicateKey for 'a', got {err:?}"
    );
    // Atomic: the good row in the same frame did not land either.
    assert!(!db.has_node("b"));
    assert_eq!(db.get_prop("a", "title"), Some(s("one")));
}

#[test]
fn an_error_policy_op_built_by_hand_still_refuses() {
    // `insert_node_on_conflict` queues a plain InsertNode for `Error`, so the
    // builder never reaches this arm — but `BatchOp` is public, so a caller
    // constructing the op directly does, and must get the same answer.
    let dir = tmp("errorop");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("Doc", "a", vec![]).unwrap();

    let (results, _sync) = db.commit_group(vec![vec![BatchOp::InsertNodeOnConflict {
        label: "Doc".into(),
        key: "a".into(),
        props: vec![],
        on_conflict: OnConflict::Error,
    }]]);
    assert!(
        matches!(&results[0], Err(GraphError::DuplicateKey { key }) if key == "a"),
        "expected DuplicateKey, got {:?}",
        results[0]
    );
}

// ── skip ─────────────────────────────────────────────────────────────────────

#[test]
fn skip_leaves_the_stored_node_untouched_and_counts_it() {
    let dir = tmp("skip");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("Doc", "a", vec![("title".into(), s("one"))])
        .unwrap();

    let out = {
        let mut b = db.batch();
        b.insert_node_on_conflict(
            "Doc",
            "a",
            vec![("title".into(), s("two")), ("extra".into(), Value::Int(1))],
            OnConflict::Skip,
        );
        b.insert_node_on_conflict("Doc", "b", vec![], OnConflict::Skip);
        b.commit_outcome().unwrap()
    };
    assert_eq!(out.skipped, 1, "the taken key is skipped");
    assert_eq!(out.replaced, 0);
    assert_eq!(out.nodes_inserted, 1, "only the fresh key is an insert");
    assert!(out.row_errors.is_empty());
    assert_eq!(db.get_prop("a", "title"), Some(s("one")), "props untouched");
    assert_eq!(db.get_prop("a", "extra"), None, "no new prop was added");
    assert!(db.has_node("b"));
}

#[test]
fn skip_of_a_whole_frame_writes_nothing_and_still_reports() {
    let dir = tmp("skipall");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("Doc", "a", vec![]).unwrap();
    let before = db.wal_total_commits().unwrap();

    let out = {
        let mut b = db.batch();
        b.insert_node_on_conflict("Doc", "a", vec![], OnConflict::Skip);
        b.commit_outcome().unwrap()
    };
    assert_eq!(out.skipped, 1);
    assert_eq!(out.nodes_inserted, 0);
    assert_eq!(
        db.wal_total_commits().unwrap(),
        before,
        "an all-skip frame writes no WAL"
    );
}

// ── replace ──────────────────────────────────────────────────────────────────

#[test]
fn replace_makes_props_exactly_the_supplied_props() {
    let dir = tmp("replace");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node(
        "Doc",
        "a",
        vec![("title".into(), s("a")), ("tag".into(), s("x"))],
    )
    .unwrap();

    let out = {
        let mut b = db.batch();
        b.insert_node_on_conflict(
            "Doc",
            "a",
            vec![("title".into(), s("b"))],
            OnConflict::Replace,
        );
        b.commit_outcome().unwrap()
    };
    assert_eq!(out.replaced, 1);
    assert_eq!(out.skipped, 0);
    assert_eq!(out.nodes_inserted, 0, "a replace is not an insert");
    assert_eq!(
        db.get_prop("a", "title"),
        Some(s("b")),
        "supplied field set"
    );
    assert_eq!(
        db.get_prop("a", "tag"),
        None,
        "a field absent from the supplied props is removed, not merged"
    );
}

#[test]
fn replace_of_a_free_key_is_an_ordinary_insert() {
    let dir = tmp("replacefresh");
    let mut db = GraphDb::open(&dir).unwrap();
    let out = {
        let mut b = db.batch();
        b.insert_node_on_conflict(
            "Doc",
            "a",
            vec![("title".into(), s("a"))],
            OnConflict::Replace,
        );
        b.commit_outcome().unwrap()
    };
    assert_eq!(out.nodes_inserted, 1);
    assert_eq!(out.replaced, 0, "no conflict, so nothing was replaced");
    assert_eq!(db.get_prop("a", "title"), Some(s("a")));
}

#[test]
fn replace_with_a_different_label_is_a_row_error() {
    let dir = tmp("relabel");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("Doc", "a", vec![("title".into(), s("a"))])
        .unwrap();

    let out = {
        let mut b = db.batch();
        b.insert_node_on_conflict(
            "Note",
            "a",
            vec![("title".into(), s("b"))],
            OnConflict::Replace,
        );
        b.insert_node_on_conflict("Doc", "z", vec![], OnConflict::Replace);
        b.commit_outcome().unwrap()
    };
    assert_eq!(out.replaced, 0, "a relabel is refused, not performed");
    assert_eq!(out.row_errors.len(), 1);
    assert_eq!(out.row_errors[0].0, 0, "the refused row is row 0");
    assert!(
        out.row_errors[0].1.contains("Doc") && out.row_errors[0].1.contains("Note"),
        "the row error names both labels: {}",
        out.row_errors[0].1
    );
    assert_eq!(db.node_info("a").unwrap().label, "Doc", "label unchanged");
    assert_eq!(db.get_prop("a", "title"), Some(s("a")), "props unchanged");
    assert!(
        db.has_node("z"),
        "the other rows of the frame still committed"
    );
}

#[test]
fn replace_cannot_move_a_node_between_namespaces() {
    let dir = tmp("nsmove");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node(
        "Doc",
        "a",
        vec![("ns".into(), s("t1")), ("title".into(), s("a"))],
    )
    .unwrap();

    let out = {
        let mut b = db.batch();
        b.insert_node_on_conflict(
            "Doc",
            "a",
            vec![("ns".into(), s("t2")), ("title".into(), s("b"))],
            OnConflict::Replace,
        );
        b.commit_outcome().unwrap()
    };
    assert_eq!(out.replaced, 0);
    assert_eq!(out.row_errors.len(), 1, "the ns refusal is a row error");
    assert!(
        out.row_errors[0].1.contains("t1") && out.row_errors[0].1.contains("t2"),
        "the row error names both namespaces: {}",
        out.row_errors[0].1
    );
    assert_eq!(db.get_prop("a", "ns"), Some(s("t1")));
    assert_eq!(db.get_prop("a", "title"), Some(s("a")));
}

#[test]
fn replace_omitting_ns_is_the_same_move_and_is_refused() {
    // Absent `ns` means `default`, so dropping it from the supplied props is a
    // move out of the namespace just as naming a different one is.
    let dir = tmp("nsdrop");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node(
        "Doc",
        "a",
        vec![("ns".into(), s("t1")), ("title".into(), s("a"))],
    )
    .unwrap();

    let out = {
        let mut b = db.batch();
        b.insert_node_on_conflict(
            "Doc",
            "a",
            vec![("title".into(), s("b"))],
            OnConflict::Replace,
        );
        b.commit_outcome().unwrap()
    };
    assert_eq!(out.replaced, 0);
    assert_eq!(out.row_errors.len(), 1);
    assert_eq!(db.get_prop("a", "ns"), Some(s("t1")));
    assert_eq!(db.get_prop("a", "title"), Some(s("a")));
}

#[test]
fn replace_keeps_the_node_inside_its_namespace() {
    let dir = tmp("nskeep");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node(
        "Doc",
        "a",
        vec![("ns".into(), s("t1")), ("title".into(), s("a"))],
    )
    .unwrap();

    let out = {
        let mut b = db.batch();
        b.insert_node_on_conflict(
            "Doc",
            "a",
            vec![("ns".into(), s("t1")), ("title".into(), s("b"))],
            OnConflict::Replace,
        );
        b.commit_outcome().unwrap()
    };
    assert_eq!(out.replaced, 1);
    assert!(out.row_errors.is_empty());
    assert_eq!(db.get_prop("a", "ns"), Some(s("t1")));
    assert_eq!(db.get_prop("a", "title"), Some(s("b")));
}

#[test]
fn replace_leaves_edges_alone() {
    let dir = tmp("edges");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("Doc", "a", vec![("title".into(), s("a"))])
        .unwrap();
    db.insert_node("Doc", "b", vec![]).unwrap();
    db.insert_edge("LINK", "a", "b").unwrap();

    let out = {
        let mut batch = db.batch();
        batch.insert_node_on_conflict(
            "Doc",
            "a",
            vec![("title".into(), s("b"))],
            OnConflict::Replace,
        );
        batch.commit_outcome().unwrap()
    };
    assert_eq!(out.replaced, 1);
    assert_eq!(
        db.neighbors("a", "LINK", Direction::Out).unwrap(),
        vec!["b".to_string()]
    );
}

// ── the frame stays atomic ───────────────────────────────────────────────────

#[test]
fn a_bad_edge_still_rejects_a_frame_of_skips() {
    let dir = tmp("atomic");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("Doc", "a", vec![("title".into(), s("a"))])
        .unwrap();

    let err = {
        let mut b = db.batch();
        b.insert_node_on_conflict("Doc", "a", vec![], OnConflict::Skip);
        b.insert_node_on_conflict("Doc", "b", vec![], OnConflict::Skip);
        b.insert_edge("LINK", "b", "ghost");
        b.commit_outcome().unwrap_err()
    };
    assert!(matches!(err, GraphError::KeyNotFound { .. }), "{err:?}");
    assert!(!db.has_node("b"), "nothing from the frame landed");
}
