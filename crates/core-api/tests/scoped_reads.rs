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

use core_api::{AlgoDir, Dir, GraphDb, GraphError, NodeMask, Predicate, RealFs, RuleDef, Value};

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

/// `a → b` and `a → c`: one subject, two neighbours, so a count can tell the
/// difference between filtering and not.
fn star(name: &str) -> (std::path::PathBuf, GraphDb<RealFs>) {
    let dir = tmp(name);
    let mut db = GraphDb::open(&dir).unwrap();
    for key in ["a", "b", "c"] {
        db.insert_node("Doc", key, vec![]).unwrap();
    }
    db.insert_edge("LINKS", "a", "b").unwrap();
    db.insert_edge("LINKS", "a", "c").unwrap();
    (dir, db)
}

/// `alice -[WORKS_AT]→ techcorp`, and a via-hop rule that derives
/// `alice -[FIT]→ proj_a` **because of `techcorp`**.
///
/// The via node is the explanation's evidence and the only node in it that the
/// two subjects do not already name — which is exactly why an `Explanation`
/// cannot be redacted: it carries `via_edge`, the hop's *type*, and never the
/// hop's key.
fn via(name: &str) -> (std::path::PathBuf, GraphDb<RealFs>) {
    let dir = tmp(name);
    let mut db = GraphDb::open(&dir).unwrap();
    let tech = || vec![("industry".to_string(), Value::Str("tech".into()))];
    db.insert_node("Org", "techcorp", tech()).unwrap();
    db.insert_node("Person", "alice", tech()).unwrap();
    db.insert_node("Project", "proj_a", tech()).unwrap();
    db.insert_edge("WORKS_AT", "alice", "techcorp").unwrap();
    db.create_rule(RuleDef {
        name: "fit".into(),
        src_label: "Person".into(),
        dst_label: "Project".into(),
        predicate: Predicate::FieldEqual {
            field: "industry".into(),
        },
        edge_type: "FIT".into(),
        weight_prop: None,
        max_edges: None,
        approximate: false,
        via_label: Some("Org".into()),
        via_edge: Some("WORKS_AT".into()),
        via_dir: None,
        namespace: None,
    })
    .unwrap();
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

// ── degree, degrees, explain (§5.3, §5.4) ────────────────────────────────────

/// A count is a disclosure. Telling a caller that `a` has two neighbours when it
/// may see only one of them says a second node exists — the same leak
/// `scoped_node_edges_drops_hidden_endpoints` prevents, arrived at by
/// arithmetic instead of by name.
#[test]
fn scoped_degree_counts_only_visible_neighbours() {
    let (_dir, db) = star("degree-visible-only");
    let mask = NodeMask::from_keys(&db, ["a", "b"]);

    assert_eq!(
        db.degree_scoped("a", None, AlgoDir::Out, &mask).unwrap(),
        1,
        "`c` is hidden, so it is not counted"
    );
    // Unscoped, `a` has two — the filter is doing the work, not the fixture.
    assert_eq!(db.degree("a", None, AlgoDir::Out).unwrap(), 2);

    // The edge type and direction legs stay honest under the filter.
    assert_eq!(
        db.degree_scoped("a", Some("LINKS"), AlgoDir::Both, &mask)
            .unwrap(),
        1
    );
    assert_eq!(
        db.degree_scoped("a", Some("NOSUCH"), AlgoDir::Both, &mask)
            .unwrap(),
        0,
        "an unknown edge type is still 0, not an error"
    );

    // Widen the scope and the hidden neighbour reappears.
    let all = NodeMask::from_keys(&db, ["a", "b", "c"]);
    assert_eq!(db.degree_scoped("a", None, AlgoDir::Out, &all).unwrap(), 2);
}

/// Hidden and absent are one answer here too: a degree of 0 for a hidden
/// subject would still confirm the key exists.
#[test]
fn scoped_degree_on_a_hidden_subject_is_key_not_found() {
    let (_dir, db) = star("degree-hidden-subject");
    let mask = NodeMask::from_keys(&db, ["a", "b"]);

    match db.degree_scoped("c", None, AlgoDir::Both, &mask) {
        Err(GraphError::KeyNotFound { key }) => assert_eq!(key, "c"),
        other => panic!("expected KeyNotFound, got {other:?}"),
    }
    match db.degree_scoped("never-existed", None, AlgoDir::Both, &mask) {
        Err(GraphError::KeyNotFound { key }) => assert_eq!(key, "never-existed"),
        other => panic!("expected KeyNotFound, got {other:?}"),
    }
}

/// `degrees` takes a key list, so the scope has to bite twice: a hidden key
/// must not come back as a row, and the rows that do come back must carry
/// visible-only counts.
#[test]
fn scoped_degrees_omits_hidden_keys_from_input_and_output() {
    let (_dir, db) = star("degrees-omits-hidden");
    let mask = NodeMask::from_keys(&db, ["a", "b"]);
    let keys: Vec<String> = ["a", "b", "c"].iter().map(|s| s.to_string()).collect();

    let got = db
        .degrees_scoped(Some(&keys), None, None, None, AlgoDir::Both, None, &mask)
        .unwrap();
    assert_eq!(
        got,
        vec![("a".to_string(), 1u64), ("b".to_string(), 1u64)],
        "`c` is hidden as a row, and as one of `a`'s neighbours"
    );

    // Unscoped: three rows, and `a` has two neighbours.
    let all = db
        .degrees(Some(&keys), None, None, None, AlgoDir::Both, None)
        .unwrap();
    assert_eq!(
        all,
        vec![
            ("a".to_string(), 2u64),
            ("b".to_string(), 1u64),
            ("c".to_string(), 1u64),
        ]
    );

    // The label-scan leg (`keys = None`) is filtered on the same terms.
    let scanned = db
        .degrees_scoped(None, Some("Doc"), None, None, AlgoDir::Both, None, &mask)
        .unwrap();
    assert_eq!(
        scanned.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(),
        vec!["a", "b"],
        "a label scan must not enumerate hidden nodes"
    );
}

/// A via-hop rule's evidence *is* the via node: `alice` and `proj_a` are linked
/// only because `techcorp` sits between them. `Explanation` names the hop's
/// edge type and never the hop's key, so there is nothing to redact — the
/// explanation goes, or the hidden node's existence is disclosed.
#[test]
fn scoped_explain_omits_a_path_through_a_hidden_node() {
    let (_dir, db) = via("explain-hidden-via");

    // Unscoped, the explanation exists — the fixture does derive the edge.
    let full = db.explain("alice", "proj_a").unwrap();
    assert_eq!(full.len(), 1, "fixture must derive one edge: {full:?}");
    assert_eq!(full[0].rule, "fit");
    assert_eq!(full[0].via_edge.as_deref(), Some("WORKS_AT"));

    // Both subjects visible, the via node hidden: nothing to say.
    let hides_via = NodeMask::from_keys(&db, ["alice", "proj_a"]);
    assert_eq!(
        db.explain_scoped("alice", "proj_a", &hides_via).unwrap(),
        Vec::<core_api::Explanation>::new(),
        "the only evidence runs through hidden `techcorp`"
    );

    // Show the via node and the explanation returns intact, not redacted.
    let all = NodeMask::from_keys(&db, ["alice", "proj_a", "techcorp"]);
    assert_eq!(db.explain_scoped("alice", "proj_a", &all).unwrap(), full);

    // Either endpoint hidden — or absent — is `KeyNotFound` (§5.3).
    let hides_dst = NodeMask::from_keys(&db, ["alice", "techcorp"]);
    match db.explain_scoped("alice", "proj_a", &hides_dst) {
        Err(GraphError::KeyNotFound { key }) => assert_eq!(key, "proj_a"),
        other => panic!("expected KeyNotFound, got {other:?}"),
    }
    match db.explain_scoped("never-existed", "proj_a", &all) {
        Err(GraphError::KeyNotFound { key }) => assert_eq!(key, "never-existed"),
        other => panic!("expected KeyNotFound, got {other:?}"),
    }
}
