"""Type stubs for the mushroomdb Python bindings (PyO3 extension module).

Kept in step with `bindings/python/src/lib.rs`; `tests/test_parity.py` fails
if a public method is missing here.
"""

from __future__ import annotations

from os import PathLike
from types import TracebackType
from typing import Any, Literal, Sequence

Scalar = int | float | str | bool | list[Any] | dict[str, Any]
"""A value the store can hold: int, float, str, bool, list, or dict."""

Params = dict[str, Scalar] | Sequence[tuple[str, Scalar]] | None
"""Query parameters: a name→value dict, a list of (name, value) tuples, or None."""

Row = dict[str, Any]
"""One result row, keyed by RETURN alias."""

class MushroomError(RuntimeError):
    """Base class for every error the engine raises.

    It subclasses `RuntimeError`, so every `except RuntimeError` written
    against an earlier release keeps catching exactly what it caught.

    Each subclass carries `code`, a stable snake_case string equal to the
    engine's variant name, and the failing variant's own fields as attributes.
    `code` is the compatibility surface: classes may be added, a code is never
    respelled. `str(e)` is the message the engine has always produced, so
    existing logs and substring checks keep working.

    `code` is `None` on this base, which is never raised directly.
    """

    code: str | None

class KeyNotFound(MushroomError):
    """No node with this key."""

    code: str
    key: str

class DuplicateKey(MushroomError):
    """A node with this key already exists."""

    code: str
    key: str

class IoError(MushroomError):
    """The store's filesystem refused a read or a write.

    The engine variant carries an unnamed `std::io::Error`, whose own text is
    already the message, so this class adds no attribute of its own.
    """

    code: str

class Corrupt(MushroomError):
    """The store's on-disk state did not parse."""

    code: str
    detail: str

class RuleInvalid(MushroomError):
    """The rule definition was rejected."""

    code: str
    detail: str

class RuleOwned(MushroomError):
    """The edge belongs to a rule and is not writable by hand."""

    code: str
    detail: str

class RuleNotFound(MushroomError):
    """No rule by this name."""

    code: str
    name: str

class QueryError(MushroomError):
    """The Cypher statement failed.

    `detail` is also the message: a Python caller has never seen a prefix.
    """

    code: str
    detail: str

class IngestError(MushroomError):
    """The batch was rejected before anything landed."""

    code: str
    detail: str

class ReadOnly(MushroomError):
    """This handle never writes.

    Raised by a write on an as-of instance and by a write on a handle
    `scoped()` produced.
    """

    code: str

class CommitOutOfRange(MushroomError):
    """The commit is outside the retained range `floor..total`.

    `floor` is the oldest commit still reachable — `0` when nothing has been
    pruned — and `total` is the exclusive upper bound.
    """

    code: str
    commit: int
    total: int
    floor: int

class ViewPropReadOnly(MushroomError):
    """The property is managed by a view and cannot be written directly."""

    code: str
    view_name: str

class CasConflict(MushroomError):
    """A compare-and-set precondition was not satisfied."""

    code: str
    key: str
    expected: int
    actual: int

class MaskedReadOnly(MushroomError):
    """A write statement reached a scoped or masked query path, which is read-only."""

    code: str

class RoleWriteDenied(MushroomError):
    """A role-bound write was denied.

    `reason` is also the message.
    """

    code: str
    reason: str

class MushroomBusy(MushroomError):
    """Another process holds the store's write lock.

    Nothing was written, so retrying later is always safe. Raised only by write
    calls: opening read-only and reading never take the lock.

    `holder` is the holding process id when the platform makes it cheaply
    knowable and `None` otherwise — a diagnostic hint, never something to
    branch on.
    """

    code: str
    holder: int | None

class NamespaceImmutable(MushroomError):
    """A namespace is set at insert and fixed for the node's lifetime.

    `from_` carries a trailing underscore because `from` is a Python keyword.
    """

    code: str
    key: str
    from_: str
    to: str

class CrossNamespace(MushroomError):
    """A hand-written edge would cross a namespace boundary.

    Only a global rule — one with no `namespace` — may derive one.
    """

    code: str
    src: str
    src_ns: str
    dst: str
    dst_ns: str

class GraphDb:
    """An embedded mushroomdb store.

    One writer at a time across processes. A handle sees commits made through
    it plus, after `refresh()`, everything other processes have committed.

    `scoped()` returns a read-only child of this handle that applies a scope to
    every read — see its docstring for the contract.
    """

    @staticmethod
    def open(path: str | PathLike[str], read_only: bool = False) -> GraphDb:
        """Open (creating if needed) the database rooted at `path`.

        Raises `MushroomBusy` if another read-write handle is open. Pass
        `read_only=True` for a handle that never writes and never waits.
        """

    def scoped(
        self,
        role: str | None = None,
        namespace: str | None = None,
        keys: Sequence[str] | None = None,
    ) -> GraphDb:
        """A child handle that scopes **every** read and refuses every write.

        It shares this handle's store and mutex — not a second open, no second
        lock, one small allocation — so `close()` on either name closes both.

        At least one leg is required; none raises `ValueError`. Legs intersect,
        so `scoped()` on a scoped handle narrows further and never widens, and
        `keys=[]` narrows to nothing. An unknown `role` raises here, not on the
        first read.

        Every read obeys one contract: the subject is checked first, so a key
        outside the scope is indistinguishable from a key that does not exist —
        `node_info` answers `None`, `node_edges` raises `KeyNotFound`,
        `node_history` is empty. Then every other node the answer would mention
        — neighbour, endpoint, candidate, evidence — is filtered to the scope,
        so `degree` counts only visible neighbours and `find_similar`,
        `pairwise_similar` and `search_hybrid` score only visible candidates.

        The scope resolves per read, so a handle held across a write is never
        stale. `refresh()` is permitted. `has_vector_rule` and
        `is_index_enabled` answer unscoped: they are schema, not node data.

        ```python
        s = db.scoped(role="reader-a")
        t = db.scoped(namespace="tenant-a", keys=visible_ids)
        ```
        """

    def insert_node(
        self,
        label: str,
        key: str,
        props: dict[str, Scalar],
        namespace: str | None = None,
    ) -> None:
        """Insert a new node; raises if `key` is already live.

        `namespace` is the namespace the node is created in — set at insert and
        immutable. Omitted means the `default` namespace.
        """

    def upsert_node(
        self, label: str, key: str, props: dict[str, Scalar]
    ) -> Literal["inserted", "updated"]:
        """Insert `key` if absent, else update changed fields in one WAL commit."""

    def insert_edge(self, edge_type: str, src: str, dst: str) -> bool:
        """Insert a user-owned edge; False if it already existed."""

    def insert_edge_upsert(
        self, edge_type: str, src: str, dst: str, placeholder_label: str
    ) -> dict[str, Any]:
        """Insert an edge, auto-creating missing endpoints as placeholder nodes."""

    def delete_edge(self, edge_type: str, src: str, dst: str) -> bool:
        """Delete a user-owned edge; raises for a rule-derived edge."""

    def delete_node(self, key: str) -> dict[str, int]:
        """Delete a node and its edges; returns `{manual_edges, derived_edges}`."""

    def set_prop(self, key: str, field: str, value: Scalar | None) -> None:
        """Set a property; `None` removes the field."""

    def remove_prop(self, key: str, field: str) -> bool:
        """Remove a property; False if it was already absent."""

    def query(
        self,
        cypher: str,
        params: Params = None,
        role: str | None = None,
        namespace: str | None = None,
    ) -> list[Row]:
        """Execute a read query and return one dict per row.

        `role` answers as one of the store's roles and `namespace` from one
        namespace; together they intersect, so neither ever widens the other.
        """

    def query_with_params(
        self, cypher: str, params: Sequence[tuple[str, Scalar]]
    ) -> list[Row]:
        """Back-compat alias for `query(cypher, params)` with a tuple list."""

    def query_write(self, cypher: str, params: Params = None) -> list[Row]:
        """Execute a Cypher write statement (CREATE / SET / DELETE / MERGE)."""

    def query_at(
        self,
        commit: int,
        cypher: str,
        params: Params = None,
        role: str | None = None,
        namespace: str | None = None,
    ) -> list[Row]:
        """Time-travel read: run `cypher` against the graph as of `commit`.

        `role` and `namespace` intersect the same way as live `query`.
        """

    def rename_node(self, old: str, new: str) -> None:
        """Rename a node's key, preserving its edges and history."""

    def create_rule(self, rule: dict[str, Any], if_not_exists: bool = False) -> bool:
        """Register a linking rule; False when `if_not_exists` skips a duplicate.

        A `"namespace"` key scopes the rule to one namespace; omitted is global.
        """

    def explain(self, a: str, b: str) -> list[Row]:
        """Why are `a` and `b` linked? One dict per derived edge."""

    def neighbors(self, key: str, edge_type: str, direction: str) -> list[str]:
        """One-hop neighbour keys along `edge_type` ('out' or 'in')."""

    def node_info(self, key: str) -> dict[str, Any] | None:
        """`{key, label, props}` for a live node, or None if absent."""

    def node_edges(self, key: str) -> list[Row]:
        """Edges incident on `key`; raises for an unknown key."""

    def node_history(self, key: str) -> Row:
        """Per-node change history: `{key, history, total_commits, horizon}`.

        `horizon` is the oldest commit still retained; events before it were
        pruned and are not in `history`.
        """

    def wal_total_commits(self) -> int:
        """Total number of committed WAL frames visible in the current horizon."""

    def edge_history(self, a: str, b: str) -> Row:
        """Per-edge change history: `{a, b, events, total_commits, horizon}`."""

    def was_linked(self, a: str, b: str, edge_type: str, at_commit: int) -> bool:
        """Whether `a` and `b` were linked by `edge_type` at or before `at_commit`."""

    def edges_at(self, key: str, commit: int) -> list[Row]:
        """Every edge incident on `key` at WAL `commit`, from one WAL scan.

        Returns `{edge_type, src, dst, derived, rule}` dicts sorted by
        `(edge_type, src, dst)`. Raises for a commit outside the horizon.
        """

    def what_if_set_prop(self, key: str, field: str, value: Any) -> dict[str, list[Row]]:
        """Derived edges a `set_prop(key, field, value)` would change.

        Returns `{"lost": [...], "gained": [...]}`, each entry shaped like an
        `edges_at` row. Writes nothing.
        """

    def enable_index(self, label: str, field: str) -> None:
        """Enable an equality index on `(label, field)`."""

    def disable_index(self, label: str, field: str) -> None:
        """Disable the equality index on `(label, field)`."""

    def is_index_enabled(self, label: str, field: str) -> bool:
        """Whether `(label, field)` currently has an equality index."""

    def has_vector_rule(self, field: str) -> bool:
        """Whether an approximate (HNSW) VectorSimilar rule covers `field`."""

    def find_similar(
        self,
        field: str,
        vector: Sequence[float],
        label: str | None = None,
        k: int = 10,
        min: float = 0.0,
        mask: Sequence[str] | None = None,
        where: dict | None = None,
        exact: bool = False,
    ) -> list[tuple[str, float]]:
        """The `k` nearest nodes to `vector` by cosine similarity on `field`."""

    def pairwise_similar(
        self,
        keys: Sequence[str],
        field: str,
        k: int = 10,
        min: float = 0.0,
    ) -> list[tuple[str, list[tuple[str, float]]]]:
        """Exact per-key cosine top-k among `keys`. Self excluded. No HNSW."""

    def degree(
        self, key: str, edge_type: str | None = None, direction: str = "both"
    ) -> int:
        """Unique directed degree of `key`. `direction` is out, in, or both (sum)."""

    def degrees(
        self,
        keys: Sequence[str] | None = None,
        label: str | None = None,
        where: dict | None = None,
        edge_type: str | None = None,
        direction: str = "both",
        limit: int | None = None,
    ) -> list[tuple[str, int]]:
        """Unique directed degree for a subset or label scan. Unknown keys omitted."""

    def search_hybrid(
        self,
        text_field: str,
        query_text: str,
        vector_field: str,
        vector: Sequence[float],
        label: str | None = None,
        k: int = 10,
    ) -> list[tuple[str, float]]:
        """Reciprocal-rank fusion over fulltext and vector similarity."""

    def get_edge_prop(
        self, edge_type: str, src_key: str, dst_key: str, field: str
    ) -> Any | None:
        """Read a single property from an edge, or None if absent."""

    def ingest_batch(
        self,
        nodes: Sequence[dict[str, Any]],
        edges: Sequence[dict[str, str]] | None = None,
        on_conflict: Literal["error", "skip", "replace"] = "error",
    ) -> dict[str, Any]:
        """Atomically ingest nodes and edges in a single WAL commit.

        `on_conflict` says what a node key that is already taken means:
        `"error"` (the default) rejects the whole frame with `DuplicateKey`;
        `"skip"` leaves the stored node untouched and counts it in `skipped`;
        `"replace"` makes its properties exactly the supplied props — fields
        absent from them are removed — and counts it in `replaced`. A label
        that differs from the stored one, and an `ns` that would move the node,
        are row errors under `"replace"`, not silent rewrites.

        The report is `{inserted, edges_inserted, skipped, replaced,
        row_errors, rules_created, skipped_fk_fields}`, where `row_errors` is a
        list of `(index into nodes, why)`.
        """

    def batch_edges(
        self,
        inserts: Sequence[dict[str, str]] | None = None,
        deletes: Sequence[dict[str, str]] | None = None,
    ) -> dict[str, int]:
        """Atomically apply edge inserts and deletes in a single WAL commit."""

    def stats(self) -> dict[str, Any]:
        """Node/edge counts, `history_floor`, `namespaces`, and per-rule size, latch, and fires."""

    def snapshot(self) -> None:
        """Write a durable snapshot and truncate the WAL tail."""

    def refresh(self) -> int:
        """Apply other processes' commits; return how many were applied.

        Writes nothing, so a `read_only=True` handle and a `scoped()` one may
        both call it.
        """

    def close(self) -> None:
        """Close the handle and release the store.

        A `scoped()` child shares the one store, so either name closes both.
        """

    def __enter__(self) -> GraphDb: ...
    def __exit__(
        self,
        exc_type: type[BaseException] | None,
        exc_value: BaseException | None,
        traceback: TracebackType | None,
    ) -> bool: ...
