use core_storage::fs::Fs;
use core_storage::Result;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use crate::db::GraphDb;

/// Controls how hidden nodes are rendered when a [`NodeMask`] is used in
/// [`GraphDb::node_info_masked`], [`GraphDb::node_edges_masked`], and
/// [`GraphDb::neighborhood_masked`].
///
/// The default is [`MaskMode::Omit`], which preserves byte-identical behaviour
/// with all pre-existing masked paths.  [`MaskMode::Stub`] is an explicit
/// opt-in that discloses node *existence* — suitable only for full-token
/// client masks.  Role-token paths are hard-coded to `Omit`.
///
/// **Existence-disclosure warning**: `Stub` mode intentionally tells the caller
/// whether a node exists, even if its contents are hidden.  Only use this on
/// client-mask (full-token) paths where the caller already has that knowledge
/// implicitly.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum MaskMode {
    /// Hidden nodes are silently omitted from every result — behaviour is
    /// byte-identical to the pre-existing masked-query paths.  This is the
    /// default.
    #[default]
    Omit,
    /// Hidden nodes' existence is acknowledged via a restricted stub:
    /// `{"key": "<key>", "restricted": true}`.  No label, props, or other
    /// fields are included in the stub.
    Stub,
}

/// Query-scoped node visibility filter (ACL primitive).
///
/// When a `NodeMask` is passed to `query_masked`, only nodes whose dense id
/// appears in `visible` will be returned by label scans, key lookups, and
/// neighbor expansions. Edges where either endpoint is hidden are silently
/// dropped from the result.
///
/// Unknown keys in `from_keys` are silently ignored (they resolve to no id).
/// An empty mask hides every node.
#[derive(Clone, Debug)]
pub struct NodeMask {
    pub(crate) visible: HashSet<u32>,
    mode: MaskMode,
}

impl NodeMask {
    /// Resolve string keys to dense ids and build a mask.
    ///
    /// Keys that do not exist in the database are ignored.
    /// The mask mode defaults to [`MaskMode::Omit`]; call [`NodeMask::with_mode`]
    /// to opt into [`MaskMode::Stub`].
    pub fn from_keys<'a, F: Fs>(db: &GraphDb<F>, keys: impl IntoIterator<Item = &'a str>) -> Self {
        let visible = keys.into_iter().filter_map(|k| db.ids().get(k)).collect();
        NodeMask {
            visible,
            mode: MaskMode::default(),
        }
    }

    /// Build a mask from an already-resolved iterator of dense node ids.
    ///
    /// Used by `ReaderSnapshot` handlers that resolve keys against the frozen
    /// state without a `GraphDb` reference.
    pub fn from_ids(ids: impl IntoIterator<Item = u32>) -> Self {
        NodeMask {
            visible: ids.into_iter().collect(),
            mode: MaskMode::default(),
        }
    }

    /// Set the rendering mode, consuming `self` and returning a new mask.
    ///
    /// **SECURITY**: never call with [`MaskMode::Stub`] on role-token paths.
    pub fn with_mode(self, mode: MaskMode) -> Self {
        NodeMask { mode, ..self }
    }

    /// Return the current rendering mode.
    pub fn mode(&self) -> MaskMode {
        self.mode
    }

    pub fn len(&self) -> usize {
        self.visible.len()
    }

    pub fn is_empty(&self) -> bool {
        self.visible.is_empty()
    }

    /// Return a new mask that is the intersection of `self` and `other`.
    ///
    /// The result contains only nodes visible in both masks.  Used to enforce
    /// the never-widen rule when a role token also supplies a client mask:
    /// `effective = role_mask.intersect(&client_mask)`.
    ///
    /// The result always carries [`MaskMode::Omit`] — the role-path invariant
    /// means stubs must never slip through an intersection.
    pub fn intersect(&self, other: &NodeMask) -> NodeMask {
        NodeMask {
            visible: self.visible.intersection(&other.visible).copied().collect(),
            mode: MaskMode::Omit,
        }
    }

    /// Return `true` if the dense node id is visible in this mask.
    ///
    /// Used by `ReaderSnapshot` handlers where the key has already been resolved
    /// to a dense id (avoids a second lookup into a `GraphDb`).
    pub fn contains_id(&self, id: u32) -> bool {
        self.visible.contains(&id)
    }

    /// Return `true` if the node identified by `key` is visible in this mask.
    ///
    /// Returns `false` for keys that do not exist in the database (unknown keys
    /// are never visible), as well as for keys that exist but are not in the
    /// visible set.  Used by node-endpoint handlers to produce the same
    /// absent-key response for both missing and hidden nodes.
    pub fn contains_node<F: core_storage::fs::Fs>(
        &self,
        db: &crate::db::GraphDb<F>,
        key: &str,
    ) -> bool {
        db.ids()
            .get(key)
            .is_some_and(|id| self.visible.contains(&id))
    }
}

// ── Store identity ────────────────────────────────────────────────────────────

/// Identifies one *loaded* store within this process.
///
/// # Where an id is minted, and where one is not
///
/// A [`GraphDb`] mints a fresh id at **two** points, both of which replace the
/// graph behind the handle:
///
/// 1. `GraphDb::new_empty` — the handle is constructed, so there is no earlier
///    state anything could have memoised against.
/// 2. `GraphDb::reset_for_reload` — the store is reloaded from disk. This is
///    the one that matters: `commit_seq` is zeroed and reseeded from
///    `max(last_change)`, which a delete-only commit leaves where it was, so a
///    reload can land back on a sequence a caller's [`Scope`] already cached a
///    mask at.
///
/// A fresh [`RoleMaskCache`] is installed at **three** points — those two, plus
/// `GraphDb::commit_roles`, which rewrites `roles.json` without committing, so
/// `commit_seq` does not move and a memoised role mask would still match its
/// version. The counts differ on purpose: `commit_roles` changes what a *role*
/// name resolves to, and the only memo a `StoreStamp` guards is the `keys` leg
/// of a [`Scope`], which is dense ids for literal key strings and does not
/// depend on role definitions at all. Minting there would evict a live entry
/// for nothing.
///
/// Read that as the rule: **the stamp tracks the identity of the graph, the
/// cache tracks the identity of the answers.** A change that replaces the graph
/// does both; a change that replaces only role definitions does one.
///
/// Ids come from a process-wide counter and are never reused, so two ids are
/// equal only when they name the same store at the same load.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct StoreId(u64);

impl StoreId {
    /// Mint an id no other store has held.
    pub(crate) fn next() -> StoreId {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        StoreId(NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
    }
}

/// Everything a resolved mask depends on: which store, and which commit of it.
///
/// # The invariant, stated where the cache lives
///
/// A dense node id means something only inside one loaded store. A memo of
/// dense ids is therefore valid only while **both** halves of this stamp still
/// match: a bare `commit_seq` is not enough on either axis.
///
/// - *Across stores*: two unrelated stores of the same age carry the same
///   `commit_seq`, and [`Scope::resolve`] accepts any `&GraphDb`, so a `Scope`
///   the caller owns can meet a store its mask was never built for.
/// - *Across a reload*: `reset_for_reload` zeroes `commit_seq` and
///   `load_from_disk` reseeds it from `max(last_change)`. `DeleteNode` records
///   no `last_change` entry, so a store snapshotted after delete-only commits
///   comes back at a sequence it already held, with a different graph behind it.
///
/// [`RoleMaskCache`] is immune to both because the `GraphDb` **owns** it and
/// replaces it on reload; a `Scope` is owned by the caller and outlives any
/// store it is handed to. The stamp is how that ownership is carried into the
/// entry instead.
///
/// Every field here is part of the validity test, because the test is
/// `entry.stamp == StoreStamp::of(db)` on a derived `PartialEq` and
/// [`StoreStamp::of`] is the only place a stamp is built. A field added to this
/// struct joins the comparison automatically and will not compile until `of`
/// fills it in.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct StoreStamp {
    store: StoreId,
    commit_seq: u64,
}

impl StoreStamp {
    /// The stamp `db` carries at this instant.
    fn of<F: Fs>(db: &GraphDb<F>) -> StoreStamp {
        StoreStamp {
            store: db.store_id(),
            commit_seq: db.commit_seq(),
        }
    }
}

// ── Scope ─────────────────────────────────────────────────────────────────────

// Test-only: counts how many times the `keys` leg was actually rebuilt, as
// opposed to served from `keys_cache`. Thread-local because the test harness
// gives each test its own thread, so a parallel test's resolve cannot be
// mistaken for this one's.
#[cfg(test)]
thread_local! {
    static SCOPE_KEYS_RESOLVES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// A read scope: what a handle may see, as a descriptor rather than a mask.
///
/// A `Scope` names its legs — a role, a namespace, an explicit key allow-list —
/// and resolves them to a [`NodeMask`] **per read**. It is the live-read
/// analogue of [`AsOfScope`](crate::db::AsOfScope), and resolves its role leg
/// through the same [`GraphDb::mask_for_role`], so a role name means one thing
/// on both.
///
/// # Never widens
///
/// Present legs are **intersected**; an absent leg contributes nothing. A scope
/// with no legs at all is refused by [`Scope::new`] rather than treated as
/// unscoped — an empty scope must never be the accident that widens a caller to
/// everything.
///
/// # Never stale
///
/// Resolution happens on every read, not once at construction. The role leg
/// goes through [`RoleMaskCache`], keyed on `commit_seq`; the `keys` leg is
/// cached on this `Scope` under a [`StoreStamp`], which is `commit_seq` plus
/// the identity of the store that commit belongs to — a `Scope` is the caller's
/// and can be carried to another store or held across a reload, neither of
/// which a sequence number can detect. Either way a read after a write
/// rebuilds, so a scoped handle held across a write cannot serve the allow-list
/// it had before — a key created since is visible, a key deleted since is not.
/// That is a security property, not a freshness nicety.
///
/// Mode is hard-coded [`MaskMode::Omit`]. [`MaskMode::Stub`] discloses node
/// existence and belongs only to full-token client masks.
pub struct Scope {
    /// Role legs, intersected. `new` sets at most one; [`Scope::intersect`]
    /// appends, because two roles cannot be collapsed into one name without
    /// resolving them against a store.
    roles: Vec<String>,
    /// Namespace legs, intersected on the same terms as `roles`.
    namespaces: Vec<String>,
    /// The explicit allow-list, already intersected across every scope that
    /// contributed one: unknown keys resolve to nothing in every leg, so the
    /// intersection of two key lists is exact as strings.
    keys: Option<Vec<String>>,
    /// The `keys` leg resolved, stamped with the store *and* the commit it was
    /// resolved against — see [`StoreStamp`] for why neither half alone is
    /// enough, and why this `Scope`-owned memo needs a stamp at all when the
    /// `GraphDb`-owned [`RoleMaskCache`] does not.
    ///
    /// [`NodeMask::from_keys`] is one hash lookup per key, so re-resolving a
    /// 50,000-key allow-list on every read would make a handle scope slower
    /// than the per-call `mask=` it replaces — for exactly the caller who needs
    /// it most. A read carrying the same stamp reuses the entry; anything else
    /// rebuilds. Steady-state cost is one comparison of two integers.
    keys_cache: Mutex<Option<(StoreStamp, NodeMask)>>,
}

impl Scope {
    /// Build a scope from the legs that are present.
    ///
    /// At least one leg is required: `Scope::new(None, None, None)` is
    /// [`GraphError::QueryError`](core_storage::GraphError::QueryError), never
    /// an unscoped handle.
    ///
    /// `keys: Some(vec![])` *is* a leg — it narrows to nothing, which is safe.
    /// An unknown role is not detected here; it surfaces from
    /// [`Scope::resolve`], which is where a store exists to check it against.
    pub fn new(
        role: Option<String>,
        namespace: Option<String>,
        keys: Option<Vec<String>>,
    ) -> Result<Scope> {
        if role.is_none() && namespace.is_none() && keys.is_none() {
            return Err(core_storage::GraphError::QueryError {
                detail: "a scope needs at least one of role, namespace or keys; \
                         an empty scope is refused rather than read as unscoped"
                    .into(),
            });
        }
        Ok(Scope {
            roles: role.into_iter().collect(),
            namespaces: namespace.into_iter().collect(),
            keys,
            keys_cache: Mutex::new(None),
        })
    }

    /// Resolve every present leg against `db` and intersect the results.
    ///
    /// Returns `Err` when a role leg names no defined role, or when
    /// `roles.json` was corrupt at open — the same refusals
    /// [`GraphDb::mask_for_role`] makes, unchanged.
    pub fn resolve<F: Fs>(&self, db: &GraphDb<F>) -> Result<NodeMask> {
        self.resolve_with(db, true)
    }

    /// Resolve against `db` without reading or writing the `keys` cache.
    ///
    /// A temporal handle from [`GraphDb::open_at`] is a distinct store with its
    /// own [`StoreId`], so the entry could not be *mistaken* between the two —
    /// the stamp settles that. What it would still do is evict: a per-call
    /// temporal handle is thrown away immediately, so caching against it buys
    /// nothing and costs the live handle its entry.
    ///
    /// Time-travel reads therefore resolve cold, in both directions: they do
    /// not consult the entry and they do not leave one behind.
    pub(crate) fn resolve_uncached<F: Fs>(&self, db: &GraphDb<F>) -> Result<NodeMask> {
        self.resolve_with(db, false)
    }

    /// The body of [`Scope::resolve`] and [`Scope::resolve_uncached`].
    fn resolve_with<F: Fs>(&self, db: &GraphDb<F>, cached: bool) -> Result<NodeMask> {
        let mut out: Option<NodeMask> = None;
        let mut narrow = |mask: NodeMask| {
            out = Some(match out.take() {
                Some(acc) => acc.intersect(&mask),
                None => mask,
            });
        };

        for role in &self.roles {
            narrow(db.mask_for_role(role)?);
        }
        for namespace in &self.namespaces {
            narrow(db.mask_for_namespace(namespace));
        }
        if self.keys.is_some() {
            narrow(self.resolve_keys(db, cached));
        }

        // `new` refuses a legless scope, so at least one leg ran.
        Ok(out
            .expect("a Scope always has at least one leg")
            .with_mode(MaskMode::Omit))
    }

    /// Return a scope seeing only what both `self` and `other` see.
    ///
    /// Legs accumulate rather than replace: two role legs are both resolved and
    /// intersected, and two key lists are intersected as strings. The result
    /// starts with a cold cache, which costs one rebuild and cannot be wrong.
    pub fn intersect(&self, other: &Scope) -> Scope {
        let keys = match (&self.keys, &other.keys) {
            (Some(a), Some(b)) => {
                let b: HashSet<&str> = b.iter().map(String::as_str).collect();
                Some(
                    a.iter()
                        .filter(|k| b.contains(k.as_str()))
                        .cloned()
                        .collect(),
                )
            }
            (Some(a), None) => Some(a.clone()),
            (None, b) => b.clone(),
        };
        Scope {
            roles: [self.roles.clone(), other.roles.clone()].concat(),
            namespaces: [self.namespaces.clone(), other.namespaces.clone()].concat(),
            keys,
            keys_cache: Mutex::new(None),
        }
    }

    /// The `keys` leg, from the cache when it was resolved at this commit.
    ///
    /// Callers check `self.keys.is_some()` first; an absent leg is not a leg
    /// resolving to the empty mask, which would hide everything.
    ///
    /// `cached = false` skips the memo entirely — see
    /// [`Scope::resolve_uncached`] for why a temporal handle must.
    fn resolve_keys<F: Fs>(&self, db: &GraphDb<F>, cached: bool) -> NodeMask {
        let keys = self.keys.as_deref().unwrap_or_default();
        let stamp = StoreStamp::of(db);

        if cached {
            if let Ok(cache) = self.keys_cache.lock() {
                if let Some((at, mask)) = cache.as_ref() {
                    if *at == stamp {
                        return mask.clone();
                    }
                }
            }
        }

        #[cfg(test)]
        SCOPE_KEYS_RESOLVES.with(|c| c.set(c.get() + 1));
        let mask = NodeMask::from_keys(db, keys.iter().map(String::as_str));

        if cached {
            if let Ok(mut cache) = self.keys_cache.lock() {
                *cache = Some((stamp, mask.clone()));
            }
        }
        mask
    }
}

impl Clone for Scope {
    /// Carries the resolved `keys` leg across, cache included: it is stamped
    /// with the store and the commit it was built against, so a clone can serve
    /// it only against that same store while that commit is still current.
    fn clone(&self) -> Scope {
        Scope {
            roles: self.roles.clone(),
            namespaces: self.namespaces.clone(),
            keys: self.keys.clone(),
            keys_cache: Mutex::new(self.keys_cache.lock().ok().and_then(|cache| cache.clone())),
        }
    }
}

impl std::fmt::Debug for Scope {
    /// Omits the resolved cache: it is a derived value, and printing a
    /// 50,000-id mask in a log line helps nobody.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Scope")
            .field("roles", &self.roles)
            .field("namespaces", &self.namespaces)
            .field("keys", &self.keys.as_ref().map(Vec::len))
            .finish()
    }
}

// ── Role → mask memo ──────────────────────────────────────────────────────────

/// Role → resolved mask, valid for exactly one commit sequence.
///
/// Resolving a role is a full scan of the label vector, and with a
/// [`visible_where`](crate::roles::RoleDef::visible_where) predicate it is also
/// a property read per candidate node. A scoped reader pays that on every
/// request, and between two writes the answer cannot have changed — so it is
/// paid once and remembered.
///
/// **Never stale**: an entry records the store's `commit_seq` at the moment it
/// was built and is served only when that is still the current one. Any write
/// bumps `commit_seq` and the entry simply stops matching. The cache can be
/// cold, but it cannot be wrong.
///
/// `commit_seq` does not move when a role *definition* changes — `roles.json`
/// is a sidecar, not a WAL record — so the owner of the cache installs a fresh
/// one whenever roles are rewritten or the store is reloaded. That also leaves
/// any reader snapshot holding the old `Arc` with a private cache, so a
/// snapshot frozen against the old definitions can never publish an answer the
/// live handle would read back.
#[derive(Default)]
pub struct RoleMaskCache {
    entries: Mutex<HashMap<String, (u64, Arc<NodeMask>)>>,
}

impl RoleMaskCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Return the memoised mask for `role` at `version`, building it if the
    /// entry is absent or was built against a different commit sequence.
    ///
    /// `build` runs outside the lock: it reads the store, and the cache must
    /// never be a lock ordering between two readers.
    pub fn get_or_build(
        &self,
        role: &str,
        version: u64,
        build: impl FnOnce() -> Result<NodeMask>,
    ) -> Result<Arc<NodeMask>> {
        if let Ok(entries) = self.entries.lock() {
            if let Some((v, mask)) = entries.get(role) {
                if *v == version {
                    return Ok(Arc::clone(mask));
                }
            }
        }
        let mask = Arc::new(build()?);
        if let Ok(mut entries) = self.entries.lock() {
            entries.insert(role.to_string(), (version, Arc::clone(&mask)));
        }
        Ok(mask)
    }

    /// Drop every entry. Correctness never depends on this — a mismatched
    /// version is already ignored — but the owner calls it when the role
    /// definitions themselves change, which `commit_seq` does not record.
    pub fn clear(&self) {
        if let Ok(mut entries) = self.entries.lock() {
            entries.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::roles::RoleDef;
    use crate::schema::Schema;

    fn tmp_dir(name: &str) -> std::path::PathBuf {
        let d =
            std::env::temp_dir().join(format!("graphdb-mask-unit-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    /// A scope with nothing in it is not "unscoped" — it is a mistake, and it
    /// is refused where it is made rather than where it would have widened.
    #[test]
    fn scope_with_no_legs_is_refused() {
        let err = Scope::new(None, None, None).expect_err("an empty scope must not be built");
        match err {
            core_storage::GraphError::QueryError { detail } => {
                for arg in ["role", "namespace", "keys"] {
                    assert!(
                        detail.contains(arg),
                        "the refusal must name `{arg}`; got {detail:?}"
                    );
                }
            }
            other => panic!("expected QueryError, got {other:?}"),
        }
    }

    /// Two legs are an intersection, never a union: the key leg cannot hand a
    /// role a node the role could not already see.
    #[test]
    fn scope_legs_intersect_and_never_widen() {
        let dir = tmp_dir("scope-intersect");
        let mut db = GraphDb::open(&dir).unwrap();
        db.insert_node("Doc", "a", vec![]).unwrap();
        db.insert_node("Doc", "b", vec![]).unwrap();
        db.insert_node("Secret", "c", vec![]).unwrap();
        db.apply_schema(&Schema {
            roles: vec![RoleDef {
                name: "reader".into(),
                keys: vec![],
                labels: vec!["Doc".into()],
                visible_where: None,
                namespaces: None,
                write: None,
            }],
            ..Default::default()
        })
        .unwrap();

        let scope = Scope::new(
            Some("reader".into()),
            None,
            Some(vec!["b".into(), "c".into()]),
        )
        .unwrap();
        let mask = scope.resolve(&db).unwrap();

        let b = db.ids().get("b").unwrap();
        assert_eq!(mask.len(), 1, "only `b` is in both legs");
        assert!(mask.contains_id(b));
        assert_eq!(mask.mode(), MaskMode::Omit);
    }

    /// The key leg is re-resolved, not frozen: a key that names nothing when
    /// the scope is built is visible once it exists.
    #[test]
    fn scope_keys_leg_sees_a_key_created_after_construction() {
        let dir = tmp_dir("scope-late-key");
        let mut db = GraphDb::open(&dir).unwrap();
        db.insert_node("Doc", "early", vec![]).unwrap();

        let scope = Scope::new(None, None, Some(vec!["late".into()])).unwrap();
        assert!(
            scope.resolve(&db).unwrap().is_empty(),
            "`late` does not exist yet"
        );

        db.insert_node("Doc", "late", vec![]).unwrap();

        let mask = scope.resolve(&db).unwrap();
        let late = db.ids().get("late").unwrap();
        assert!(
            mask.contains_id(late),
            "the key leg must be re-resolved after the write"
        );
        assert_eq!(mask.len(), 1);
    }

    /// Re-resolving a key leg is one hash lookup per key, so it is remembered
    /// for the commit it was resolved at — and only for that commit.
    #[test]
    fn scope_keys_leg_is_cached_within_a_commit() {
        let dir = tmp_dir("scope-keys-cache");
        let mut db = GraphDb::open(&dir).unwrap();
        db.insert_node("Doc", "a", vec![]).unwrap();

        let scope = Scope::new(None, None, Some(vec!["a".into()])).unwrap();
        let before = SCOPE_KEYS_RESOLVES.with(|c| c.get());

        scope.resolve(&db).unwrap();
        scope.resolve(&db).unwrap();
        assert_eq!(
            SCOPE_KEYS_RESOLVES.with(|c| c.get()) - before,
            1,
            "two resolves at one commit rebuild the key leg once"
        );

        db.insert_node("Doc", "b", vec![]).unwrap();
        scope.resolve(&db).unwrap();
        assert_eq!(
            SCOPE_KEYS_RESOLVES.with(|c| c.get()) - before,
            2,
            "a write invalidates the cached key leg"
        );
    }

    /// A time-travel read resolves cold, and leaves the cache as it found it.
    ///
    /// The cache is keyed on `commit_seq`, which identifies a graph state only
    /// *within one handle*: a store reopened from a snapshot seeds its
    /// `commit_seq` from `max(last_change)`, which underestimates the WAL
    /// length (`db.rs`'s own note at the archive rename says so). So a
    /// temporal handle from `open_at` can carry the same sequence as the live
    /// one while holding a different graph — and a shared `Scope` would serve
    /// one's allow-list to the other. `resolve_uncached` is the way out, and
    /// it has to be cold in both directions to work.
    #[test]
    fn scope_resolve_uncached_neither_reads_nor_fills_the_cache() {
        let dir = tmp_dir("scope-uncached");
        let mut db = GraphDb::open(&dir).unwrap();
        db.insert_node("Doc", "a", vec![]).unwrap();

        let scope = Scope::new(None, None, Some(vec!["a".into()])).unwrap();
        let before = SCOPE_KEYS_RESOLVES.with(|c| c.get());

        scope.resolve_uncached(&db).unwrap();
        scope.resolve_uncached(&db).unwrap();
        assert_eq!(
            SCOPE_KEYS_RESOLVES.with(|c| c.get()) - before,
            2,
            "an uncached resolve never serves the cached entry"
        );

        scope.resolve(&db).unwrap();
        scope.resolve(&db).unwrap();
        assert_eq!(
            SCOPE_KEYS_RESOLVES.with(|c| c.get()) - before,
            3,
            "and never fills it either: the first cached resolve still rebuilds"
        );
    }

    /// A resolved key leg belongs to the store it was resolved against.
    ///
    /// Dense ids are store-local, so serving store B an allow-list built in
    /// store A does not merely go stale — it hands B whichever of *its* nodes
    /// happen to hold those ids. `commit_seq` alone cannot tell the two apart:
    /// two stores of the same age carry the same one.
    #[test]
    fn scope_keys_cache_never_crosses_stores() {
        let dir_a = tmp_dir("scope-store-a");
        let dir_b = tmp_dir("scope-store-b");
        let mut a = GraphDb::open(&dir_a).unwrap();
        a.insert_node("Doc", "filler", vec![]).unwrap();
        a.insert_node("Doc", "target", vec![]).unwrap();

        let mut b = GraphDb::open(&dir_b).unwrap();
        b.insert_node("Doc", "other", vec![]).unwrap();
        b.insert_node("Doc", "secret", vec![]).unwrap();

        assert_eq!(
            a.commit_seq(),
            b.commit_seq(),
            "the two stores must collide on commit_seq for this to test anything"
        );

        let scope = Scope::new(None, None, Some(vec!["target".into()])).unwrap();
        let mask_a = scope.resolve(&a).unwrap();
        assert!(mask_a.contains_id(a.ids().get("target").unwrap()));

        let mask_b = scope.resolve(&b).unwrap();
        for key in ["other", "secret"] {
            let id = b.ids().get(key).unwrap();
            assert!(
                !mask_b.contains_id(id),
                "store B's mask admits `{key}`, a node this scope never named"
            );
        }
        assert!(
            mask_b.is_empty(),
            "store B has no `target`, so the key leg resolves to nothing there"
        );
    }

    /// A reload is a new store as far as a resolved mask is concerned.
    ///
    /// `reset_for_reload` zeroes `commit_seq` and `load_from_disk` reseeds it
    /// from `max(last_change)`. `DeleteNode` writes no `last_change` entry, so
    /// a store snapshotted after a delete-only commit comes back at a sequence
    /// it already held — with a different graph behind it.
    #[test]
    fn scope_keys_cache_is_dropped_when_the_store_reloads() {
        let dir = tmp_dir("scope-reload");
        let mut w = GraphDb::open(&dir).unwrap();
        w.insert_node("Doc", "filler", vec![]).unwrap();
        w.insert_node("Doc", "target", vec![]).unwrap();
        w.insert_node("Doc", "keep", vec![]).unwrap();

        // A read-only handle takes no lock, so it can follow the writer.
        let mut r = GraphDb::open_with_options(
            &dir,
            crate::db::OpenOptions {
                read_only: true,
                ..Default::default()
            },
        )
        .unwrap();
        let seq_before = r.commit_seq();

        let scope = Scope::new(None, None, Some(vec!["target".into()])).unwrap();
        assert_eq!(
            scope.resolve(&r).unwrap().len(),
            1,
            "`target` is visible before the delete"
        );

        // A delete-only commit: it moves the graph but writes no `last_change`.
        w.delete_node("target").unwrap();
        w.snapshot().unwrap();
        r.refresh().unwrap();
        assert_eq!(
            r.commit_seq(),
            seq_before,
            "the reseed must land back on the sequence the mask was cached at"
        );

        let mask = scope.resolve(&r).unwrap();
        for key in ["filler", "keep"] {
            let id = r.ids().get(key).unwrap();
            assert!(
                !mask.contains_id(id),
                "after the reload the stale mask admits `{key}`, which the scope never named"
            );
        }
        assert!(
            mask.is_empty(),
            "`target` is gone, so the key leg must resolve to nothing"
        );
    }

    // ── Scope::intersect ──────────────────────────────────────────────────────

    /// Build a two-node store and a `reader` role that sees only `Doc`.
    fn intersect_fixture(name: &str) -> (std::path::PathBuf, GraphDb<core_storage::fs::RealFs>) {
        let dir = tmp_dir(name);
        let mut db = GraphDb::open(&dir).unwrap();
        db.insert_node("Doc", "a", vec![]).unwrap();
        db.insert_node("Doc", "b", vec![]).unwrap();
        db.insert_node("Secret", "c", vec![]).unwrap();
        db.apply_schema(&Schema {
            roles: vec![RoleDef {
                name: "reader".into(),
                keys: vec![],
                labels: vec!["Doc".into()],
                visible_where: None,
                namespaces: None,
                write: None,
            }],
            ..Default::default()
        })
        .unwrap();
        (dir, db)
    }

    fn visible_keys<F: Fs>(db: &GraphDb<F>, mask: &NodeMask, keys: &[&str]) -> Vec<String> {
        keys.iter()
            .filter(|k| mask.contains_node(db, k))
            .map(|k| (*k).to_string())
            .collect()
    }

    /// Two key legs meet as an intersection: the result names only keys both
    /// sides named.
    #[test]
    fn intersect_of_two_key_legs_keeps_only_the_keys_in_both() {
        let (_dir, db) = intersect_fixture("intersect-both-keys");
        let left = Scope::new(None, None, Some(vec!["a".into(), "b".into()])).unwrap();
        let right = Scope::new(None, None, Some(vec!["b".into(), "c".into()])).unwrap();

        let mask = left.intersect(&right).resolve(&db).unwrap();
        assert_eq!(visible_keys(&db, &mask, &["a", "b", "c"]), vec!["b"]);
        // The operation is symmetric.
        let mask = right.intersect(&left).resolve(&db).unwrap();
        assert_eq!(visible_keys(&db, &mask, &["a", "b", "c"]), vec!["b"]);
    }

    /// The asymmetric arms: a side with no key leg contributes no keys, and
    /// must not be read as "every key". The surviving list is the other side's,
    /// whichever side that is.
    #[test]
    fn intersect_carries_a_lone_key_leg_from_either_side() {
        let (_dir, db) = intersect_fixture("intersect-one-key");
        let keyed = Scope::new(None, None, Some(vec!["b".into()])).unwrap();
        let roled = Scope::new(Some("reader".into()), None, None).unwrap();

        // (Some, None)
        let mask = keyed.intersect(&roled).resolve(&db).unwrap();
        assert_eq!(visible_keys(&db, &mask, &["a", "b", "c"]), vec!["b"]);
        // (None, Some)
        let mask = roled.intersect(&keyed).resolve(&db).unwrap();
        assert_eq!(visible_keys(&db, &mask, &["a", "b", "c"]), vec!["b"]);
    }

    /// (None, None): neither side named keys, so the result has no key leg —
    /// not an empty one, which would hide everything.
    #[test]
    fn intersect_of_two_keyless_scopes_has_no_key_leg() {
        let (_dir, db) = intersect_fixture("intersect-no-keys");
        let roled = Scope::new(Some("reader".into()), None, None).unwrap();
        let namespaced = Scope::new(None, Some("default".into()), None).unwrap();

        let both = roled.intersect(&namespaced);
        assert!(
            both.keys.is_none(),
            "an absent key leg must stay absent, not become `Some(vec![])`"
        );
        let mask = both.resolve(&db).unwrap();
        assert_eq!(
            visible_keys(&db, &mask, &["a", "b", "c"]),
            vec!["a", "b"],
            "the role leg still decides; the missing key leg narrows nothing"
        );
    }

    /// Role and namespace legs accumulate rather than replace: an intersection
    /// resolves both and narrows by each.
    #[test]
    fn intersect_accumulates_role_and_namespace_legs() {
        let (_dir, db) = intersect_fixture("intersect-legs");
        let reader = Scope::new(Some("reader".into()), None, None).unwrap();
        let elsewhere = Scope::new(None, Some("other".into()), None).unwrap();

        let both = reader.intersect(&elsewhere);
        assert_eq!(both.roles, vec!["reader".to_string()]);
        assert_eq!(both.namespaces, vec!["other".to_string()]);
        let mask = both.resolve(&db).unwrap();
        assert!(
            mask.is_empty(),
            "every node is in the `default` namespace, so the two legs share nobody"
        );
    }

    /// The property the whole handle-scoping story rests on: whatever two
    /// scopes are combined, the result sees no node either one could not.
    #[test]
    fn intersect_can_never_widen_either_side() {
        let (_dir, db) = intersect_fixture("intersect-never-widens");
        let all = ["a", "b", "c"];
        let scopes = || {
            vec![
                Scope::new(Some("reader".into()), None, None).unwrap(),
                Scope::new(None, Some("default".into()), None).unwrap(),
                Scope::new(None, None, Some(vec!["b".into(), "c".into()])).unwrap(),
                Scope::new(None, None, Some(vec![])).unwrap(),
                Scope::new(Some("reader".into()), None, Some(vec!["a".into()])).unwrap(),
            ]
        };
        for left in scopes() {
            for right in scopes() {
                let l = visible_keys(&db, &left.resolve(&db).unwrap(), &all);
                let r = visible_keys(&db, &right.resolve(&db).unwrap(), &all);
                let both = visible_keys(&db, &left.intersect(&right).resolve(&db).unwrap(), &all);
                for key in &both {
                    assert!(
                        l.contains(key) && r.contains(key),
                        "{left:?} ∩ {right:?} sees `{key}`, which one side alone does not"
                    );
                }
            }
        }
    }

    #[test]
    fn a_version_change_rebuilds_and_clear_empties() {
        let cache = RoleMaskCache::new();
        let built = std::cell::Cell::new(0u32);
        let build = |ids: Vec<u32>| {
            built.set(built.get() + 1);
            Ok(NodeMask::from_ids(ids))
        };

        let m = cache.get_or_build("r", 1, || build(vec![1])).unwrap();
        assert_eq!(m.len(), 1);
        assert_eq!(built.get(), 1);

        // Same version → memo hit, `build` never runs.
        let m = cache.get_or_build("r", 1, || build(vec![1, 2])).unwrap();
        assert_eq!(m.len(), 1, "the memoised mask is returned unchanged");
        assert_eq!(built.get(), 1);

        // New version → rebuild.
        let m = cache.get_or_build("r", 2, || build(vec![1, 2])).unwrap();
        assert_eq!(m.len(), 2);
        assert_eq!(built.get(), 2);

        // A different role is a different entry.
        let m = cache.get_or_build("other", 2, || build(vec![9])).unwrap();
        assert_eq!(m.len(), 1);
        assert_eq!(built.get(), 3);

        cache.clear();
        let _ = cache.get_or_build("r", 2, || build(vec![1, 2])).unwrap();
        assert_eq!(built.get(), 4, "clear drops the entry, so it rebuilds");
    }

    #[test]
    fn a_failed_build_is_not_cached() {
        let cache = RoleMaskCache::new();
        assert!(cache
            .get_or_build("r", 1, || Err(core_storage::GraphError::KeyNotFound {
                key: "role:r".into()
            }))
            .is_err());
        let m = cache
            .get_or_build("r", 1, || Ok(NodeMask::from_ids(vec![7])))
            .unwrap();
        assert_eq!(m.len(), 1);
    }
}
