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
/// cached on this `Scope` on the same terms. Either way a read after a write
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
    /// The `keys` leg resolved, with the commit it was resolved at.
    ///
    /// [`NodeMask::from_keys`] is one hash lookup per key, so re-resolving a
    /// 50,000-key allow-list on every read would make a handle scope slower
    /// than the per-call `mask=` it replaces — for exactly the caller who needs
    /// it most. A read at the same `commit_seq` reuses the entry; a read after
    /// a write rebuilds. Steady-state cost is one integer comparison.
    keys_cache: Mutex<Option<(u64, NodeMask)>>,
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
    /// The cache is keyed on `commit_seq`, which identifies a graph state only
    /// within one handle. A store reopened from a snapshot seeds `commit_seq`
    /// from `max(last_change)`, which underestimates the WAL length — so a
    /// temporal handle from [`GraphDb::open_at`] can carry the same sequence as
    /// the live one while holding a different graph. One `Scope` resolved
    /// against both would then serve one's allow-list to the other, which is a
    /// leak and not a staleness bug.
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
        let seq = db.commit_seq();

        if cached {
            if let Ok(cache) = self.keys_cache.lock() {
                if let Some((at, mask)) = cache.as_ref() {
                    if *at == seq {
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
                *cache = Some((seq, mask.clone()));
            }
        }
        mask
    }
}

impl Clone for Scope {
    /// Carries the resolved `keys` leg across, cache included: it is stamped
    /// with the commit it was built at, so a clone can serve it only while that
    /// is still current.
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
