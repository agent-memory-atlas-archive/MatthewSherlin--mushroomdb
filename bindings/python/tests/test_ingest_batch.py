"""`ingest_batch(on_conflict=...)` — v0.6.10 Task 6.

A mirror rebuild writes a frame onto a store that already has content. The
argument says what a key that is already taken means: refuse the frame (the
0.6.9 answer, unchanged), leave the stored node alone, or make its properties
exactly the supplied ones.
"""

from __future__ import annotations

import time

import pytest

from mushroomdb import GraphDb


def _open(tmp_path, name="db"):
    return GraphDb.open(str(tmp_path / name))


# ── error: the default, and the 0.6.9 behaviour ──────────────────────────────


def test_default_still_raises_on_duplicate(tmp_path):
    db = _open(tmp_path)
    db.ingest_batch([{"key": "a", "label": "Doc", "props": {"title": "one"}}])
    with pytest.raises(RuntimeError):
        db.ingest_batch(
            [
                {"key": "a", "label": "Doc", "props": {"title": "two"}},
                {"key": "b", "label": "Doc", "props": {}},
            ]
        )
    # Atomic: the good row in the same frame did not land either.
    assert db.node_info("b") is None
    assert db.node_info("a")["props"]["title"] == "one"
    db.close()


def test_explicit_error_is_the_same_as_the_default(tmp_path):
    db = _open(tmp_path)
    db.ingest_batch([{"key": "a", "label": "Doc", "props": {}}])
    with pytest.raises(RuntimeError):
        db.ingest_batch(
            [{"key": "a", "label": "Doc", "props": {}}], on_conflict="error"
        )
    db.close()


def test_an_unknown_policy_is_a_value_error(tmp_path):
    db = _open(tmp_path)
    with pytest.raises(ValueError):
        db.ingest_batch([], on_conflict="upsert")
    db.close()


# ── skip ─────────────────────────────────────────────────────────────────────


def test_skip_leaves_the_existing_node_untouched(tmp_path):
    db = _open(tmp_path)
    db.ingest_batch(
        [
            {"key": "a", "label": "Doc", "props": {"title": "one", "tag": "x"}},
            {"key": "b", "label": "Doc", "props": {}},
        ],
        [{"edge_type": "LINK", "src": "a", "dst": "b"}],
    )
    report = db.ingest_batch(
        [{"key": "a", "label": "Note", "props": {"title": "two"}}],
        on_conflict="skip",
    )
    assert report["skipped"] == 1
    assert report["replaced"] == 0
    assert report["inserted"] == 0, "a skipped row is not an insert"
    assert report["row_errors"] == []
    info = db.node_info("a")
    assert info["label"] == "Doc", "label as it was"
    assert info["props"]["title"] == "one", "props as they were"
    assert info["props"]["tag"] == "x", "props as they were"
    assert db.neighbors("a", "LINK", "out") == ["b"], "edges as they were"
    db.close()


def test_skip_still_inserts_the_rows_that_are_new(tmp_path):
    db = _open(tmp_path)
    db.ingest_batch([{"key": "a", "label": "Doc", "props": {}}])
    report = db.ingest_batch(
        [
            {"key": "a", "label": "Doc", "props": {}},
            {"key": "b", "label": "Doc", "props": {}},
        ],
        on_conflict="skip",
    )
    assert report["skipped"] == 1
    assert report["inserted"] == 1
    assert db.node_info("b") is not None
    db.close()


# ── replace ──────────────────────────────────────────────────────────────────


def test_replace_makes_props_exactly_the_supplied_props(tmp_path):
    db = _open(tmp_path)
    db.ingest_batch([{"key": "a", "label": "Doc", "props": {"title": "a", "tag": "x"}}])
    report = db.ingest_batch(
        [{"key": "a", "label": "Doc", "props": {"title": "b"}}],
        on_conflict="replace",
    )
    assert report["replaced"] == 1
    assert report["skipped"] == 0
    assert report["inserted"] == 0, "a replace is not an insert"
    props = db.node_info("a")["props"]
    assert props["title"] == "b", "the supplied field is set"
    assert "tag" not in props, "a field absent from the supplied props is removed"
    db.close()


def test_replace_of_a_free_key_is_an_ordinary_insert(tmp_path):
    db = _open(tmp_path)
    report = db.ingest_batch(
        [{"key": "a", "label": "Doc", "props": {"title": "a"}}],
        on_conflict="replace",
    )
    assert report["inserted"] == 1
    assert report["replaced"] == 0
    assert db.node_info("a")["props"]["title"] == "a"
    db.close()


def test_replace_with_a_different_label_is_a_row_error(tmp_path):
    db = _open(tmp_path)
    db.ingest_batch([{"key": "a", "label": "Doc", "props": {"title": "a"}}])
    report = db.ingest_batch(
        [
            {"key": "a", "label": "Note", "props": {"title": "b"}},
            {"key": "z", "label": "Doc", "props": {}},
        ],
        on_conflict="replace",
    )
    assert report["replaced"] == 0, "a relabel is refused, not performed"
    assert len(report["row_errors"]) == 1
    index, message = report["row_errors"][0]
    assert index == 0, "row_errors carries the index into `nodes`"
    assert "Doc" in message and "Note" in message
    info = db.node_info("a")
    assert info["label"] == "Doc", "the node keeps its label"
    assert info["props"]["title"] == "a", "and its props"
    assert db.node_info("z") is not None, "the other rows still committed"
    db.close()


def test_replace_cannot_move_a_node_between_namespaces(tmp_path):
    db = _open(tmp_path)
    db.ingest_batch([{"key": "a", "label": "Doc", "props": {"ns": "t1", "title": "a"}}])
    report = db.ingest_batch(
        [{"key": "a", "label": "Doc", "props": {"ns": "t2", "title": "b"}}],
        on_conflict="replace",
    )
    assert report["replaced"] == 0
    assert len(report["row_errors"]) == 1, "the ns refusal is a row error"
    message = report["row_errors"][0][1]
    assert "t1" in message and "t2" in message
    props = db.node_info("a")["props"]
    assert props["ns"] == "t1"
    assert props["title"] == "a"
    db.close()


def test_replace_that_drops_ns_is_the_same_move_and_is_refused(tmp_path):
    # Absent `ns` means `default`, so dropping it is a move out of the
    # namespace just as naming a different one is.
    db = _open(tmp_path)
    db.ingest_batch([{"key": "a", "label": "Doc", "props": {"ns": "t1", "title": "a"}}])
    report = db.ingest_batch(
        [{"key": "a", "label": "Doc", "props": {"title": "b"}}],
        on_conflict="replace",
    )
    assert report["replaced"] == 0
    assert len(report["row_errors"]) == 1
    props = db.node_info("a")["props"]
    assert props["ns"] == "t1"
    assert props["title"] == "a"
    db.close()


def test_replace_inside_one_namespace_is_allowed(tmp_path):
    db = _open(tmp_path)
    db.ingest_batch([{"key": "a", "label": "Doc", "props": {"ns": "t1", "title": "a"}}])
    report = db.ingest_batch(
        [{"key": "a", "label": "Doc", "props": {"ns": "t1", "title": "b"}}],
        on_conflict="replace",
    )
    assert report["replaced"] == 1
    assert report["row_errors"] == []
    props = db.node_info("a")["props"]
    assert props["ns"] == "t1"
    assert props["title"] == "b"
    db.close()


def test_replace_leaves_edges_alone(tmp_path):
    db = _open(tmp_path)
    db.ingest_batch(
        [
            {"key": "a", "label": "Doc", "props": {"title": "a"}},
            {"key": "b", "label": "Doc", "props": {}},
        ],
        [{"edge_type": "LINK", "src": "a", "dst": "b"}],
    )
    report = db.ingest_batch(
        [{"key": "a", "label": "Doc", "props": {"title": "b"}}],
        on_conflict="replace",
    )
    assert report["replaced"] == 1
    assert db.neighbors("a", "LINK", "out") == ["b"]
    db.close()


def test_a_duplicate_edge_stays_a_no_op_under_every_policy(tmp_path):
    db = _open(tmp_path)
    db.ingest_batch(
        [
            {"key": "a", "label": "Doc", "props": {}},
            {"key": "b", "label": "Doc", "props": {}},
        ],
        [{"edge_type": "LINK", "src": "a", "dst": "b"}],
    )
    for policy in ("error", "skip", "replace"):
        report = db.ingest_batch(
            [], [{"edge_type": "LINK", "src": "a", "dst": "b"}], on_conflict=policy
        )
        assert report["edges_inserted"] == 0, policy
    db.close()


# ── the frame stays atomic ───────────────────────────────────────────────────


def test_a_bad_edge_still_rejects_a_frame_of_skips(tmp_path):
    db = _open(tmp_path)
    db.ingest_batch([{"key": "a", "label": "Doc", "props": {}}])
    with pytest.raises(RuntimeError):
        db.ingest_batch(
            [
                {"key": "a", "label": "Doc", "props": {}},
                {"key": "b", "label": "Doc", "props": {}},
            ],
            [{"edge_type": "LINK", "src": "b", "dst": "ghost"}],
            on_conflict="skip",
        )
    assert db.node_info("b") is None, "nothing from the frame landed"
    db.close()


# ── the reason the decision has to be O(1) ───────────────────────────────────


def test_reingest_100k_is_linear(tmp_path):
    """A quadratic conflict check would pass every other test in this file and
    be useless at the size that motivated the feature.

    Both passes are chunked at 10 000, the size `ingest_batch`'s own docstring
    recommends, so the two passes differ in exactly one thing: whether each row
    hits a key that is already taken.
    """
    db = _open(tmp_path)
    chunk = 10_000
    total = 100_000
    nodes = [
        {"key": f"n-{i:06d}", "label": "Thing", "props": {"n": i}} for i in range(total)
    ]

    t0 = time.perf_counter()
    inserted = sum(
        db.ingest_batch(nodes[i : i + chunk])["inserted"] for i in range(0, total, chunk)
    )
    first_elapsed = time.perf_counter() - t0
    assert inserted == total

    t0 = time.perf_counter()
    reports = [
        db.ingest_batch(nodes[i : i + chunk], on_conflict="skip")
        for i in range(0, total, chunk)
    ]
    second_elapsed = time.perf_counter() - t0
    assert sum(r["skipped"] for r in reports) == total
    assert sum(r["inserted"] for r in reports) == 0

    assert second_elapsed < first_elapsed * 3, (
        f"re-ingest of the same {total} took {second_elapsed:.2f}s against "
        f"{first_elapsed:.2f}s for the first pass — the conflict check is not O(1) per row"
    )
    db.close()
