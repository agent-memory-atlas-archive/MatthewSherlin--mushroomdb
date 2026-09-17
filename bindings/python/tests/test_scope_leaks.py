"""The leak surface of `db.scoped(...)` — one row of spec §5.3 per test.

Every test here makes the same assertion in the same way: **the scoped answer
mentions no key outside the scope.** `_mentions_hidden` walks whatever the
method returns — dict, list, tuple, string, at any depth — and fails if a
hidden key appears anywhere in it. A method that silently ignores the scope
is the defect this release exists to prevent, so this file is deliberately
uniform and deliberately separate from the per-method test modules.

Each test also pins the *unscoped* answer, so a test can never pass because
the fixture had nothing to leak.

Two rows read differently from the spec's table, and the difference matters:

- §5.3 says the history surfaces answer `KeyNotFound` for a hidden subject.
  `node_history`, `edge_history`, `edges_at` and `was_linked` do **not** raise
  for an *unknown* key — they answer empty / `False`. Raising for a hidden one
  would therefore say "this key exists but you may not see it", which is the
  existence oracle the contract's first sentence forbids. Hidden answers
  exactly as absent does, per method, and that is what is asserted below.
- `neighborhood` has no Python binding to scope; `neighbors` is its one-hop
  surface and carries the row.
"""

from __future__ import annotations

import pytest

from mushroomdb import GraphDb

HIDDEN = ("hidden_x", "hidden_y")
VISIBLE = ("vis_a", "vis_b")


def _mentions_hidden(obj) -> list[str]:
    """Every hidden key named anywhere inside `obj`, at any depth."""
    found: list[str] = []

    def walk(o) -> None:
        if isinstance(o, str):
            found.extend(h for h in HIDDEN if h in o)
        elif isinstance(o, dict):
            for k, v in o.items():
                walk(k)
                walk(v)
        elif isinstance(o, (list, tuple, set)):
            for v in o:
                walk(v)

    walk(obj)
    return found


def _assert_clean(label: str, answer) -> None:
    leaked = _mentions_hidden(answer)
    assert not leaked, f"{label} leaked {sorted(set(leaked))}: {answer!r}"


@pytest.fixture
def store(tmp_path):
    """Four Person nodes — two visible, two not — with edges, text and vectors.

    `vis_a`, `vis_b` and `hidden_x` share `team="red"` and near-identical
    embeddings, so every derived edge, degree, similarity and explanation that
    names a visible node also has a hidden one to name. `hidden_y` sits in its
    own namespace so `stats()`'s roster has something to narrow.
    """
    db = GraphDb.open(str(tmp_path / "db"))
    db.create_rule(
        {
            "name": "same_team",
            "src_label": "Person",
            "dst_label": "Person",
            "predicate": {"FieldEqual": {"field": "team"}},
            "edge_type": "SAME_TEAM",
            "weight_prop": None,
        }
    )
    db.create_rule(
        {
            "name": "close_emb",
            "src_label": "Person",
            "dst_label": "Person",
            "predicate": {"VectorSimilar": {"field": "emb", "min": 0.5}},
            "edge_type": "SIMILAR",
            "weight_prop": "score",
            "approximate": False,
        }
    )
    db.insert_node("Person", "vis_a", {"team": "red", "bio": "alpha one", "emb": [1.0, 0.0]})
    db.insert_node("Person", "vis_b", {"team": "red", "bio": "alpha two", "emb": [0.95, 0.05]})
    db.insert_node(
        "Person", "hidden_x", {"team": "red", "bio": "alpha three", "emb": [0.9, 0.1]}
    )
    db.insert_node(
        "Person",
        "hidden_y",
        {"team": "blue", "bio": "alpha four", "emb": [0.85, 0.15]},
        namespace="tenant-secret",
    )
    db.insert_edge("LINKS", "vis_a", "vis_b")
    db.insert_edge("LINKS", "vis_a", "hidden_x")
    db.insert_edge("LINKS", "vis_b", "hidden_x")
    yield db, db.scoped(keys=list(VISIBLE))
    db.close()


# ── §5.3, row by row ─────────────────────────────────────────────────────────


def test_query(store):
    db, s = store
    q = "MATCH (n:Person) RETURN key(n) AS k ORDER BY k"
    assert _mentions_hidden(db.query(q)), "fixture: the unscoped answer names them"
    rows = s.query(q)
    _assert_clean("query", rows)
    assert [r["k"] for r in rows] == list(VISIBLE)


def test_query_with_params(store):
    """Not in §5.3's table, and a full read of the store if left unscoped."""
    db, s = store
    q = "MATCH (n:Person) WHERE n.team = $t RETURN key(n) AS k ORDER BY k"
    assert _mentions_hidden(db.query_with_params(q, [("t", "red")]))
    rows = s.query_with_params(q, [("t", "red")])
    _assert_clean("query_with_params", rows)
    assert [r["k"] for r in rows] == list(VISIBLE)


def test_query_at(store):
    db, s = store
    commit = db.wal_total_commits() - 1
    q = "MATCH (n:Person) RETURN key(n) AS k ORDER BY k"
    assert _mentions_hidden(db.query_at(commit, q))
    rows = s.query_at(commit, q)
    _assert_clean("query_at", rows)
    assert [r["k"] for r in rows] == list(VISIBLE)


def test_node_info(store):
    db, s = store
    assert db.node_info("hidden_x") is not None
    _assert_clean("node_info", s.node_info("vis_a"))
    assert s.node_info("hidden_x") is None, "hidden reads exactly as absent"


def test_node_edges(store):
    db, s = store
    assert _mentions_hidden(db.node_edges("vis_a"))
    _assert_clean("node_edges", s.node_edges("vis_a"))
    with pytest.raises(RuntimeError):
        s.node_edges("hidden_x")


def test_neighborhood_has_no_python_binding(store):
    """§5.3 lists `neighborhood`; the binding exposes only `neighbors`.

    If a `neighborhood` method is ever added, this fails and says so — the
    point being that a new read surface must arrive already scoped.
    """
    _db, _s = store
    assert not hasattr(GraphDb, "neighborhood"), (
        "GraphDb.neighborhood now exists: route it through neighborhood_scoped "
        "and give it a row in this file"
    )


def test_neighbors(store):
    db, s = store
    assert _mentions_hidden(db.neighbors("vis_a", "LINKS", "out"))
    got = s.neighbors("vis_a", "LINKS", "out")
    _assert_clean("neighbors", got)
    assert got == ["vis_b"]
    with pytest.raises(RuntimeError):
        s.neighbors("hidden_x", "LINKS", "out")


def test_explain(store):
    db, s = store
    assert db.explain("vis_a", "hidden_x"), "fixture: they are linked"
    _assert_clean("explain", s.explain("vis_a", "vis_b"))
    with pytest.raises(RuntimeError):
        s.explain("vis_a", "hidden_x")


def test_degree(store):
    db, s = store
    assert db.degree("vis_a", edge_type="LINKS", direction="out") == 2
    assert s.degree("vis_a", edge_type="LINKS", direction="out") == 1, (
        "an unfiltered count discloses a hidden neighbour by arithmetic"
    )
    with pytest.raises(RuntimeError):
        s.degree("hidden_x")


def test_degrees(store):
    db, s = store
    assert _mentions_hidden(db.degrees(label="Person"))
    got = s.degrees(label="Person")
    _assert_clean("degrees", got)
    assert sorted(k for k, _ in got) == list(VISIBLE)


def test_find_similar(store):
    db, s = store
    assert _mentions_hidden(db.find_similar("emb", [1.0, 0.0], k=10))
    got = s.find_similar("emb", [1.0, 0.0], k=10)
    _assert_clean("find_similar", got)
    assert sorted(k for k, _ in got) == list(VISIBLE)


def test_find_similar_with_a_per_call_mask_narrows_further(store):
    _db, s = store
    got = s.find_similar("emb", [1.0, 0.0], k=10, mask=["vis_a", "hidden_x"])
    _assert_clean("find_similar(mask=)", got)
    assert [k for k, _ in got] == ["vis_a"], "a per-call mask narrows, never widens"


def test_pairwise_similar(store):
    db, s = store
    keys = ["vis_a", "vis_b", "hidden_x"]
    assert _mentions_hidden(db.pairwise_similar(keys, "emb", k=10))
    got = s.pairwise_similar(keys, "emb", k=10)
    _assert_clean("pairwise_similar", got)
    assert [src for src, _ in got] == list(VISIBLE)


def test_search_hybrid(store):
    db, s = store
    assert _mentions_hidden(db.search_hybrid("bio", "alpha", "emb", [1.0, 0.0], k=10))
    got = s.search_hybrid("bio", "alpha", "emb", [1.0, 0.0], k=10)
    _assert_clean("search_hybrid", got)
    assert sorted(k for k, _ in got) == list(VISIBLE)


def test_was_linked(store):
    db, s = store
    commit = db.wal_total_commits() - 1
    assert db.was_linked("vis_a", "hidden_x", "LINKS", commit) is True
    assert s.was_linked("vis_a", "hidden_x", "LINKS", commit) is False, (
        "hidden answers as absent does, and an unknown key here is False"
    )
    assert s.was_linked("vis_a", "vis_b", "LINKS", commit) is True


def test_edges_at(store):
    db, s = store
    commit = db.wal_total_commits() - 1
    assert _mentions_hidden(db.edges_at("vis_a", commit))
    _assert_clean("edges_at", s.edges_at("vis_a", commit))
    assert s.edges_at("hidden_x", commit) == [], "as an unknown key answers"


def test_node_history(store):
    db, s = store
    assert _mentions_hidden(db.node_history("vis_a")["history"])
    _assert_clean("node_history", s.node_history("vis_a")["history"])
    assert s.node_history("hidden_x")["history"] == [], "as an unknown key answers"


def test_edge_history(store):
    db, s = store
    assert db.edge_history("vis_a", "hidden_x")["events"], "fixture: there are events"
    assert s.edge_history("vis_a", "hidden_x")["events"] == []
    _assert_clean("edge_history", s.edge_history("vis_a", "vis_b")["events"])
    assert s.edge_history("vis_a", "vis_b")["events"]


def test_what_if_set_prop(store):
    db, s = store
    assert _mentions_hidden(db.what_if_set_prop("vis_a", "team", "green"))
    _assert_clean("what_if_set_prop", s.what_if_set_prop("vis_a", "team", "green"))
    with pytest.raises(RuntimeError):
        s.what_if_set_prop("hidden_x", "team", "green")


def test_get_edge_prop(store):
    db, s = store
    assert db.get_edge_prop("SIMILAR", "vis_a", "hidden_x", "score") is not None
    assert s.get_edge_prop("SIMILAR", "vis_a", "hidden_x", "score") is None
    assert s.get_edge_prop("SIMILAR", "vis_a", "vis_b", "score") is not None


def test_stats(store):
    db, s = store
    names = [n["name"] for n in db.stats()["namespaces"]]
    assert "tenant-secret" in names, "fixture: the roster has something to narrow"
    scoped = s.stats()
    _assert_clean("stats", scoped["namespaces"])
    assert [n["name"] for n in scoped["namespaces"]] == ["default"]
    assert [n["nodes_live"] for n in scoped["namespaces"]] == [2]


def test_schema_facts_stay_unscoped(store):
    """`has_vector_rule` and `is_index_enabled` are schema, not node data."""
    db, s = store
    db.enable_index("Person", "team")
    assert s.is_index_enabled("Person", "team") == db.is_index_enabled("Person", "team")
    assert s.has_vector_rule("emb") == db.has_vector_rule("emb")
