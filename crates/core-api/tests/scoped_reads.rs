//! Scoped reads — the RBAC read contract, in the engine (v0.6.10 §5.3, §5.4).
//!
//! A scoped read checks the **subject** first: a key outside the scope is
//! indistinguishable from a key that does not exist. Only then is every other
//! node the answer would mention filtered to the scope.
//!
//! This is the contract `crates/server/src/http.rs` wrote by hand on its
//! role-token branches. It lives here now, so every caller gets it — not only
//! the two HTTP routes that happened to implement it.
//!
//! The `*_masked` methods deliberately do **not** make the subject check: they
//! serve full-token client masks, where the caller already knows the key
//! exists. That difference is the whole reason these methods exist.

use core_api::{Dir, GraphDb, GraphError, NodeMask, RealFs};

fn tmp(name: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "graphdb-scoped-{}-{}-{}",
        name,
        std::process::id(),
        nanos
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// `a → b → c`, the chain every test here reasons about.
fn chain(name: &str) -> (std::path::PathBuf, GraphDb<RealFs>) {
    let dir = tmp(name);
    let mut db = GraphDb::open(&dir).unwrap();
    for key in ["a", "b", "c"] {
        db.insert_node("Doc", key, vec![]).unwrap();
    }
    db.insert_edge("LINKS", "a", "b").unwrap();
    db.insert_edge("LINKS", "b", "c").unwrap();
    (dir, db)
}

/// The `key` column of a neighborhood result, in row order.
fn keys_of(rs: &core_api::ResultSet) -> Vec<String> {
    (0..rs.len())
        .map(|i| match rs.row(i)[0].as_ref() {
            Some(core_api::Value::Str(s)) => s.clone(),
            other => panic!("the key column must be a string, got {other:?}"),
        })
        .collect()
}

/// A hidden subject answers exactly as an absent one does. Anything else is an
/// existence oracle: the caller learns `c` exists by being told it has edges.
#[test]
fn scoped_node_edges_on_a_hidden_subject_is_key_not_found() {
    let (_dir, db) = chain("edges-hidden-subject");
    let mask = NodeMask::from_keys(&db, ["a", "b"]);

    match db.node_edges_scoped("c", &mask) {
        Err(GraphError::KeyNotFound { key }) => assert_eq!(key, "c"),
        Ok(edges) => panic!("a hidden subject must not yield an edge list: {edges:?}"),
        Err(other) => panic!("expected KeyNotFound, got {other:?}"),
    }

    // The same answer an absent key gives — that is the point.
    match db.node_edges_scoped("never-existed", &mask) {
        Err(GraphError::KeyNotFound { key }) => assert_eq!(key, "never-existed"),
        other => panic!("expected KeyNotFound, got {other:?}"),
    }
}

/// The subject is visible, so the call succeeds — but `b → c` names a node the
/// scope hides, and a visible subject must not become a window onto its hidden
/// neighbours.
#[test]
fn scoped_node_edges_drops_hidden_endpoints() {
    let (_dir, db) = chain("edges-hidden-endpoint");
    let mask = NodeMask::from_keys(&db, ["a", "b"]);

    let edges = db.node_edges_scoped("b", &mask).unwrap();
    assert_eq!(edges.len(), 1, "only `a → b` survives the scope: {edges:?}");
    assert_eq!(edges[0].edge_type, "LINKS");
    assert_eq!(edges[0].src_key, "a");
    assert_eq!(edges[0].dst_key, "b");

    // Unscoped, `b` has both edges — the filter is doing the work, not the
    // fixture.
    assert_eq!(db.node_edges("b").unwrap().len(), 2);
}

/// `c` is *in* the scope, and still must not appear: the only path to it runs
/// through hidden `b`. A hidden node is not a stepping stone.
///
/// The start key is not a row of its own — that is `neighborhood_masked`'s
/// long-standing shape, which this wrapper must not change — so the correct
/// answer here is the empty set.
#[test]
fn scoped_neighborhood_never_crosses_a_hidden_node() {
    let (_dir, db) = chain("neighborhood-no-crossing");
    let mask = NodeMask::from_keys(&db, ["a", "c"]);

    let rs = db
        .neighborhood_scoped("a", 2, None, Dir::Both, &mask)
        .unwrap();
    assert_eq!(
        keys_of(&rs),
        Vec::<String>::new(),
        "`c` is reachable only through hidden `b`"
    );

    // Widen the scope to include `b` and `c` reappears — the fixture does reach
    // it, so the empty result above is the filter's doing.
    let all = NodeMask::from_keys(&db, ["a", "b", "c"]);
    let rs = db
        .neighborhood_scoped("a", 2, None, Dir::Both, &all)
        .unwrap();
    assert_eq!(keys_of(&rs), vec!["b".to_string(), "c".to_string()]);
}

/// The subject check applies to `neighborhood` on the same terms as to
/// `node_edges`: hidden and absent are one answer.
#[test]
fn scoped_neighborhood_on_a_hidden_subject_is_key_not_found() {
    let (_dir, db) = chain("neighborhood-hidden-subject");
    let mask = NodeMask::from_keys(&db, ["a", "b"]);

    match db.neighborhood_scoped("c", 2, None, Dir::Both, &mask) {
        Err(GraphError::KeyNotFound { key }) => assert_eq!(key, "c"),
        other => panic!("expected KeyNotFound, got {other:?}"),
    }
    match db.neighborhood_scoped("never-existed", 2, None, Dir::Both, &mask) {
        Err(GraphError::KeyNotFound { key }) => assert_eq!(key, "never-existed"),
        other => panic!("expected KeyNotFound, got {other:?}"),
    }
}

/// The reader-snapshot twins carry the same contract. HTTP's role branches read
/// from a [`core_api::ReaderSnapshot`], not a `GraphDb`, so a contract that
/// lived only on `GraphDb` would leave the RBAC path still hand-rolled.
#[test]
fn scoped_reads_on_a_reader_snapshot_honour_the_same_contract() {
    let (_dir, db) = chain("reader-snapshot");
    let mask = NodeMask::from_keys(&db, ["a", "b"]);
    let snap = db.reader();

    match snap.node_edges_scoped("c", &mask) {
        Err(GraphError::KeyNotFound { key }) => assert_eq!(key, "c"),
        other => panic!("expected KeyNotFound, got {other:?}"),
    }

    let edges = snap.node_edges_scoped("b", &mask).unwrap();
    assert_eq!(edges.len(), 1, "only `a → b` survives: {edges:?}");
    assert_eq!(edges[0].src_key, "a");
    assert_eq!(edges[0].dst_key, "b");

    let via_hidden = NodeMask::from_keys(&db, ["a", "c"]);
    let rs = snap
        .neighborhood_scoped("a", 2, None, Dir::Both, &via_hidden)
        .unwrap();
    assert_eq!(rs.len(), 0, "`c` is reachable only through hidden `b`");

    match snap.neighborhood_scoped("c", 2, None, Dir::Both, &mask) {
        Err(GraphError::KeyNotFound { key }) => assert_eq!(key, "c"),
        other => panic!("expected KeyNotFound, got {other:?}"),
    }
}
