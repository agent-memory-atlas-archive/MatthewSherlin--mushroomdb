"""`db.roles()` — the role list, read back (spec §5.8).

A sidecar is handed a role name in configuration and builds a scoped handle per
request. Without a readout it discovers a typo on the first request, in front of
a caller. `roles()` is what lets it check at boot.

Three things are load-bearing:

- a store with no `roles.json` answers `[]`;
- a store whose `roles.json` was **corrupt** at open raises, exactly as
  `mask_for_role` does. Answering `[]` there would read as "no roles are
  defined", which is indistinguishable from an unrestricted store — the one
  wrong answer a boot-time check must not get;
- a scoped handle is refused. The list names every role, the node keys each
  role is granted by name, and the namespace roster `stats()` narrows — the
  scope exists to keep exactly those out of an answer. Its row is in
  `test_scope_leaks.py`.
"""

from __future__ import annotations

import json
import pathlib

import pytest

from mushroomdb import Corrupt, GraphDb


def _write_roles(path, doc: dict) -> None:
    pathlib.Path(path, "roles.json").write_text(json.dumps(doc))


def test_roles_is_empty_when_none_are_defined(tmp_path):
    db = GraphDb.open(str(tmp_path / "db"))
    db.insert_node("Doc", "d1", {})
    assert db.roles() == []
    db.close()


def test_roles_lists_what_roles_json_defines(tmp_path):
    path = tmp_path / "db"
    db = GraphDb.open(str(path))
    db.insert_node("Doc", "d1", {"state": "published"})
    db.insert_node("Doc", "a1", {}, namespace="tenant-a")
    db.close()
    _write_roles(
        path,
        {
            "version": 4,
            "roles": [
                {
                    "name": "a-reader",
                    "labels": ["Doc"],
                    "keys": ["d1"],
                    "namespaces": ["tenant-a"],
                    "visible_where": {"field": "state", "eq": "published"},
                },
                {"name": "plain", "labels": ["Doc"], "keys": []},
            ],
        },
    )

    db = GraphDb.open(str(path))
    got = db.roles()
    assert [r["name"] for r in got] == ["a-reader", "plain"]
    assert got[0] == {
        "name": "a-reader",
        "labels": ["Doc"],
        "keys": ["d1"],
        "namespaces": ["tenant-a"],
        "visible_where": {"field": "state", "eq": "published", "in": None},
    }
    # An absent `namespaces` is unscoped, and reads as `None` rather than `[]`:
    # the empty list is a role that sees no namespace at all.
    assert got[1] == {
        "name": "plain",
        "labels": ["Doc"],
        "keys": [],
        "namespaces": None,
        "visible_where": None,
    }
    # The name a sidecar validates is the name `scoped()` accepts.
    assert db.scoped(role="a-reader") is not None
    db.close()


def test_roles_reads_the_in_form_of_visible_where(tmp_path):
    path = tmp_path / "db"
    GraphDb.open(str(path)).close()
    _write_roles(
        path,
        {
            "version": 3,
            "roles": [
                {
                    "name": "r",
                    "labels": ["Doc"],
                    "visible_where": {"field": "state", "in": ["published", "review"]},
                }
            ],
        },
    )
    db = GraphDb.open(str(path))
    assert db.roles()[0]["visible_where"] == {
        "field": "state",
        "eq": None,
        "in": ["published", "review"],
    }
    db.close()


@pytest.mark.parametrize(
    "doc",
    ["{not json at all", json.dumps({"version": 99, "roles": []})],
    ids=["unparseable", "unrecognised-version"],
)
def test_roles_raises_when_roles_json_was_corrupt_at_open(tmp_path, doc):
    """A poisoned sidecar must not read as "no roles are defined".

    That answer is the answer an unrestricted store gives, so a boot-time check
    would pass on a store no role can read. `mask_for_role` fails loud here;
    `roles()` says the same thing in the same class.
    """
    path = tmp_path / "db"
    db = GraphDb.open(str(path))
    db.insert_node("Doc", "d1", {})
    db.close()
    pathlib.Path(path, "roles.json").write_text(doc)

    db = GraphDb.open(str(path))
    with pytest.raises(Corrupt) as err:
        db.roles()
    assert "roles.json" in str(err.value)
    assert err.value.code == "corrupt"
    # The same cause, the same class, through the resolver a read would use.
    with pytest.raises(Corrupt):
        db.scoped(role="anything")
    db.close()


def test_roles_follows_a_rewritten_sidecar_across_a_reopen(tmp_path):
    """`roles.json` is read at open, so a readout is the file the handle opened.

    Stated as a test because a sidecar polling `roles()` for changes would
    otherwise be relying on something the engine does not promise.
    """
    path = tmp_path / "db"
    GraphDb.open(str(path)).close()
    _write_roles(path, {"version": 1, "roles": [{"name": "one", "labels": ["Doc"]}]})
    db = GraphDb.open(str(path))
    assert [r["name"] for r in db.roles()] == ["one"]
    _write_roles(path, {"version": 1, "roles": [{"name": "two", "labels": ["Doc"]}]})
    assert [r["name"] for r in db.roles()] == ["one"], "the file is read at open"
    db.close()

    db = GraphDb.open(str(path))
    assert [r["name"] for r in db.roles()] == ["two"]
    db.close()
