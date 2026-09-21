"""`db.scoped(...)` — the child handle (v0.6.10 §5.2).

A scoped handle shares the parent's store and mutex, applies its scope to
every read, and refuses every write. It is not a second open: it takes no
lock, and creating one while the parent holds the write lock is legal.

The leak surface — "the scoped answer mentions no key outside the scope",
one assertion per row of spec §5.3 — lives in `test_scope_leaks.py`, on
purpose. This file is about the handle; that one is about what it discloses.
"""

from __future__ import annotations

import json
import pathlib

import pytest

from mushroomdb import GraphDb


def _roles(path, *, labels=("Doc",), keys=(), namespaces=None) -> None:
    """Write a one-role `roles.json` sidecar beside the store.

    The binding has no `apply_schema`, so the sidecar is written by hand and
    the store reopened — which is also the shape an operator deploys.
    """
    role: dict = {"name": "a-reader", "labels": list(labels), "keys": list(keys)}
    if namespaces is not None:
        role["namespaces"] = list(namespaces)
    pathlib.Path(path, "roles.json").write_text(
        json.dumps({"version": 4, "roles": [role]})
    )


# ── the scope applies to reads ────────────────────────────────────────────────


def test_scoped_handle_applies_to_every_read(tmp_path):
    db = GraphDb.open(str(tmp_path / "db"))
    db.insert_node("Doc", "a", {})
    db.insert_node("Doc", "b", {})
    db.insert_edge("LINKS", "a", "b")

    s = db.scoped(keys=["a"])
    assert s.node_info("b") is None, "a hidden node is absent, not restricted"
    with pytest.raises(RuntimeError):
        s.node_edges("b")  # hidden subject -> KeyNotFound
    assert s.degree("a", direction="out") == 0, "b is hidden, so it is not counted"
    db.close()


def test_scoped_by_role(tmp_path):
    path = tmp_path / "db"
    db = GraphDb.open(str(path))
    db.insert_node("Doc", "a", {})
    db.insert_node("Memo", "m", {})
    db.close()
    _roles(path, labels=("Doc",))

    db = GraphDb.open(str(path))
    s = db.scoped(role="a-reader")
    assert s.node_info("a") is not None
    assert s.node_info("m") is None, "the role's labels do not include Memo"
    db.close()


def test_scoped_by_namespace(tmp_path):
    db = GraphDb.open(str(tmp_path / "db"))
    db.insert_node("Doc", "a1", {}, namespace="tenant-a")
    db.insert_node("Doc", "b1", {}, namespace="tenant-b")

    s = db.scoped(namespace="tenant-a")
    assert s.node_info("a1") is not None
    assert s.node_info("b1") is None
    db.close()


def test_scoped_legs_intersect_and_never_widen(tmp_path):
    path = tmp_path / "db"
    db = GraphDb.open(str(path))
    for key in ("a", "b", "c"):
        db.insert_node("Doc", key, {})
    db.close()
    # A role's `keys` and `labels` legs are a union *within* the role, so this
    # role is spelled as keys alone to make it name exactly {a, b}.
    _roles(path, labels=(), keys=("a", "b"))

    db = GraphDb.open(str(path))
    s = db.scoped(role="a-reader", keys=["b", "c"])
    assert s.node_info("a") is None, "keys= narrows the role"
    assert s.node_info("b") is not None
    assert s.node_info("c") is None, "the role does not admit c"
    db.close()


def test_scoped_of_scoped_never_widens(tmp_path):
    db = GraphDb.open(str(tmp_path / "db"))
    for k in ("a", "b", "c"):
        db.insert_node("Doc", k, {})
    inner = db.scoped(keys=["a", "b"]).scoped(keys=["b", "c"])
    assert inner.node_info("a") is None and inner.node_info("c") is None
    assert inner.node_info("b") is not None
    db.close()


def test_scoped_requires_a_leg(tmp_path):
    db = GraphDb.open(str(tmp_path / "db"))
    with pytest.raises(ValueError):
        db.scoped()
    db.close()


def test_scoped_with_an_empty_key_list_hides_everything(tmp_path):
    """`keys=[]` *is* a leg: it narrows to nothing, which is the safe reading."""
    db = GraphDb.open(str(tmp_path / "db"))
    db.insert_node("Doc", "a", {})
    s = db.scoped(keys=[])
    assert s.node_info("a") is None
    assert s.query("MATCH (n) RETURN key(n) AS k") == []
    db.close()


def test_unknown_role_raises_at_scoped_not_at_first_read(tmp_path):
    db = GraphDb.open(str(tmp_path / "db"))
    with pytest.raises(RuntimeError):
        db.scoped(role="nobody")
    db.close()


def test_scoped_rejects_a_malformed_namespace(tmp_path):
    db = GraphDb.open(str(tmp_path / "db"))
    with pytest.raises(ValueError):
        db.scoped(namespace="no spaces")
    db.close()


# ── the scope resolves per read, never once ──────────────────────────────────


def test_a_scoped_handle_held_across_a_write_is_not_stale(tmp_path):
    """The trap the plan names: resolve once in `scoped()` and this goes red.

    A key created after the handle was built is visible; a key deleted after
    it was built is not. Caching the resolved mask on the handle serves the
    allow-list the store had before the write, which is a leak rather than a
    freshness nicety.
    """
    db = GraphDb.open(str(tmp_path / "db"))
    db.insert_node("Doc", "a", {})

    s = db.scoped(keys=["a", "late"])
    assert s.node_info("late") is None, "not created yet"

    db.insert_node("Doc", "late", {})
    assert s.node_info("late") is not None, "the same handle resolves again"

    db.delete_node("late")
    assert s.node_info("late") is None, "and a deletion narrows it back"
    db.close()


def test_a_role_scoped_handle_sees_a_node_created_after_it(tmp_path):
    """The role leg is re-resolved too, through the `commit_seq` memo."""
    path = tmp_path / "db"
    db = GraphDb.open(str(path))
    db.insert_node("Doc", "a", {})
    db.close()
    _roles(path, labels=("Doc",))

    db = GraphDb.open(str(path))
    s = db.scoped(role="a-reader")
    assert [r["k"] for r in s.query("MATCH (n) RETURN key(n) AS k ORDER BY k")] == ["a"]

    db.insert_node("Doc", "b", {})
    assert [r["k"] for r in s.query("MATCH (n) RETURN key(n) AS k ORDER BY k")] == [
        "a",
        "b",
    ]
    db.close()


# ── the scope refuses writes ─────────────────────────────────────────────────


def test_scoped_handle_refuses_writes(tmp_path):
    db = GraphDb.open(str(tmp_path / "db"))
    db.insert_node("Doc", "a", {})
    db.insert_node("Doc", "b", {})
    db.insert_edge("LINKS", "a", "b")
    s = db.scoped(keys=["a"])

    mutations = [
        ("insert_node", lambda: s.insert_node("Doc", "x", {})),
        ("upsert_node", lambda: s.upsert_node("Doc", "a", {"t": 1})),
        ("insert_edge", lambda: s.insert_edge("LINKS", "a", "a")),
        ("insert_edge_upsert", lambda: s.insert_edge_upsert("L", "a", "z", "Doc")),
        ("delete_edge", lambda: s.delete_edge("LINKS", "a", "b")),
        ("delete_node", lambda: s.delete_node("a")),
        ("set_prop", lambda: s.set_prop("a", "t", 1)),
        ("remove_prop", lambda: s.remove_prop("a", "t")),
        ("rename_node", lambda: s.rename_node("a", "a2")),
        ("query_write", lambda: s.query_write("CREATE (n:Doc {id: 'z'})")),
        ("create_rule", lambda: s.create_rule(_rule("r1"))),
        ("enable_index", lambda: s.enable_index("Doc", "t")),
        ("disable_index", lambda: s.disable_index("Doc", "t")),
        ("ingest_batch", lambda: s.ingest_batch([{"key": "z", "label": "Doc", "props": {}}])),
        ("batch_edges", lambda: s.batch_edges([{"edge_type": "L", "src": "a", "dst": "b"}])),
        ("snapshot", lambda: s.snapshot()),
    ]
    for name, call in mutations:
        with pytest.raises(RuntimeError, match="scoped"):
            call()
        assert name  # names the failing row in the assertion message

    # Nothing landed.
    assert db.node_info("x") is None and db.node_info("z") is None
    assert db.node_info("a") is not None
    assert db.node_edges("a") != []
    db.close()


def test_a_scoped_create_rule_with_if_not_exists_still_refuses(tmp_path):
    """`if_not_exists` returns before the write path, so it needs its own guard."""
    db = GraphDb.open(str(tmp_path / "db"))
    db.create_rule(_rule("dup"))
    s = db.scoped(keys=["a"])
    with pytest.raises(RuntimeError, match="scoped"):
        s.create_rule(_rule("dup"), if_not_exists=True)
    db.close()


def test_a_scoped_cypher_write_is_refused_before_it_is_parsed(tmp_path):
    db = GraphDb.open(str(tmp_path / "db"))
    s = db.scoped(keys=["a"])
    with pytest.raises(RuntimeError):
        s.query("CREATE (n:Doc {id: 'z'})")
    assert db.node_info("z") is None
    db.close()


def _rule(name: str) -> dict:
    return {
        "name": name,
        "src_label": "Person",
        "dst_label": "Person",
        "predicate": {"FieldEqual": {"field": "team"}},
        "edge_type": "SAME_TEAM",
        "weight_prop": None,
    }


# ── lifecycle ────────────────────────────────────────────────────────────────


def test_refresh_is_permitted_on_a_scoped_handle(tmp_path):
    """`refresh()` writes nothing, so a scoped handle may call it."""
    path = str(tmp_path / "db")
    writer = GraphDb.open(path)
    writer.insert_node("Doc", "a", {})

    reader = GraphDb.open(path, read_only=True)
    s = reader.scoped(keys=["a", "b"])
    assert s.node_info("b") is None

    writer.insert_node("Doc", "b", {})
    assert s.refresh() >= 1
    assert s.node_info("b") is not None
    reader.close()
    writer.close()


def test_scoped_does_not_take_a_second_lock(tmp_path):
    """A child is one allocation, not an open: the parent keeps the lock."""
    path = str(tmp_path / "db")
    db = GraphDb.open(path)
    db.insert_node("Doc", "a", {})
    s = db.scoped(keys=["a"])
    assert s.node_info("a") is not None
    db.insert_node("Doc", "b", {}), "the parent still writes"
    db.close()


def test_close_on_the_parent_closes_the_child(tmp_path):
    db = GraphDb.open(str(tmp_path / "db"))
    db.insert_node("Doc", "a", {})
    s = db.scoped(keys=["a"])
    db.close()
    with pytest.raises(RuntimeError, match="closed"):
        s.node_info("a")


def test_close_on_the_child_closes_the_parent(tmp_path):
    """They are one store handle, so either name closes it."""
    db = GraphDb.open(str(tmp_path / "db"))
    db.insert_node("Doc", "a", {})
    s = db.scoped(keys=["a"])
    s.close()
    with pytest.raises(RuntimeError, match="closed"):
        db.node_info("a")


def test_scoped_read_surface_answers_after_the_parent_writes(tmp_path):
    """One handle, one mutex: interleaving a parent write and a child read works."""
    db = GraphDb.open(str(tmp_path / "db"))
    s = db.scoped(keys=["a", "b"])
    db.insert_node("Doc", "a", {})
    assert s.node_info("a") is not None
    db.insert_node("Doc", "b", {})
    db.insert_edge("LINKS", "a", "b")
    assert s.degree("a", direction="out") == 1
    db.close()


# ── stub parity ──────────────────────────────────────────────────────────────


def test_scoped_is_documented_and_stubbed():
    """`_DOCUMENTED` in test_parity.py is a fixed list; this covers the new name.

    Same three checks it makes: a docstring, a text signature, and a line in
    the packaged `.pyi`.
    """
    import importlib.util
    import inspect
    import pathlib

    assert inspect.getdoc(GraphDb.scoped)
    assert getattr(GraphDb.scoped, "__text_signature__", None)

    spec = importlib.util.find_spec("mushroomdb")
    assert spec is not None and spec.origin is not None
    stub = pathlib.Path(spec.origin).parent / "__init__.pyi"
    assert "def scoped(" in stub.read_text(), "scoped missing from the type stub"
