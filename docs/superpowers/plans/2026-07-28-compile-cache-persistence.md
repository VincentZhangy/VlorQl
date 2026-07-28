# CompileCache Bincode Persistence — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add bincode-based disk persistence to `CompileCache` so cached compiled queries survive process restarts.

**Architecture:** A shadow `RwLock<HashMap<K, V>>` mirrors moka cache entries for serialization. `persist()` serializes the HashMap; `load()` deserializes and re-inserts. VlorQlBuilder gains a `with_persistent_compile_cache` method; VlorQl::Drop auto-persists.

**Tech Stack:** Rust, moka 0.12, bincode 1.x, serde

## Global Constraints

- Add `bincode = "1"` to workspace `[workspace.dependencies]`
- Add `bincode.workspace = true` to `crates/vlorql-core/Cargo.toml`
- All new methods on `CompileCache` keep existing `pub` signatures unchanged
- Corrupt/missing persist files load as empty cache (graceful degradation)
- Persist failures are logged at `warn!` level, never propagated as errors

---

### Task 1: Add bincode dependency + shadow HashMap fields

**Files:**
- Modify: `Cargo.toml` (root workspace)
- Modify: `crates/vlorql-core/Cargo.toml`
- Modify: `crates/vlorql-core/src/cache/compile_cache.rs`

**Interfaces:**
- Consumes: `CompileCacheKey` (Serialize+Deserialize), `CompiledQuery` (Serialize+Deserialize)
- Produces: Modified `CompileCache` struct with `entries` and `persist_path` fields

- [ ] **Step 1: Add bincode workspace dependency**

Edit `Cargo.toml` (root): add `bincode = "1"` to `[workspace.dependencies]`

Edit `crates/vlorql-core/Cargo.toml`: add `bincode.workspace = true`

- [ ] **Step 2: Add shadow HashMap + persist_path fields**

In `compile_cache.rs`, add imports:
```rust
use std::collections::HashMap;
use std::path::PathBuf;
use tokio::sync::RwLock;
```

Modify `CompileCache` struct:
```rust
pub struct CompileCache {
    inner: MokaCache<CompileCacheKey, Arc<CompiledQuery>>,
    entries: RwLock<HashMap<CompileCacheKey, CompiledQuery>>,
    max_size: u64,
    persist_path: Option<PathBuf>,
}
```

- [ ] **Step 3: Update `new()` constructor**

Initialize `entries: RwLock::new(HashMap::new())` and `persist_path: None`.

- [ ] **Step 4: Run existing tests to confirm no regression**

Run: `cargo test -p vlorql-core -- cache::compile_cache`
Expected: all pass

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml crates/vlorql-core/Cargo.toml crates/vlorql-core/src/cache/compile_cache.rs
git commit -m "chore(deps): add bincode + prepare CompileCache for persistence"
```

---

### Task 2: Sync insert/invalidate/clear with shadow HashMap

**Files:**
- Modify: `crates/vlorql-core/src/cache/compile_cache.rs`

**Interfaces:**
- Consumes: `CompileCache` struct with `entries` field from Task 1
- Produces: `insert()`/`invalidate_plan()`/`clear()` methods that keep HashMap in sync

- [ ] **Step 1: Update `insert()` to also write to HashMap**

```rust
pub async fn insert(&self, plan: &ValidatedPlan, profile: &DialectProfile, query: CompiledQuery) {
    let key = CompileCacheKey::new(plan, profile);
    tracing::debug!(...);
    self.inner.insert(key.clone(), Arc::new(query.clone())).await;
    self.entries.write().await.insert(key, query);
}
```

- [ ] **Step 2: Update `invalidate_plan()` to remove from HashMap**

```rust
pub async fn invalidate_plan(&self, plan: &ValidatedPlan, profile: &DialectProfile) {
    let key = CompileCacheKey::new(plan, profile);
    self.inner.invalidate(&key).await;
    self.entries.write().await.remove(&key);
}
```

- [ ] **Step 3: Update `clear()` to also clear HashMap**

```rust
pub fn clear(&self) {
    self.inner.invalidate_all();
    self.entries.blocking_write().clear();
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p vlorql-core -- cache::compile_cache`
Expected: all pass

- [ ] **Step 5: Commit**

```bash
git commit -a -m "feat(cache): sync shadow HashMap on CompileCache mutating operations"
```

---

### Task 3: Add `with_persistence`, `persist`, `load`, `set_persist_path` methods

**Files:**
- Modify: `crates/vlorql-core/src/cache/compile_cache.rs`

**Interfaces:**
- Produces: `CompileCache::with_persistence(max_size, ttl, path) -> Self`
- Produces: `CompileCache::persist(&self) -> Result<(), VlorQLError>`
- Produces: `CompileCache::load(path: &Path) -> Self`
- Produces: `CompileCache::set_persist_path(&mut self, path: PathBuf)`

- [ ] **Step 1: Implement `with_persistence`**

```rust
pub fn with_persistence(max_size: u64, ttl_seconds: u64, path: PathBuf) -> Self {
    let cache = Self::new(max_size, ttl_seconds);
    cache.set_persist_path(path);
    cache
}
```

Wait — `set_persist_path` needs `&mut self` but `new()` returns `Self` (owned). Let `with_persistence` directly construct:

```rust
pub fn with_persistence(max_size: u64, ttl_seconds: u64, path: PathBuf) -> Self {
    let mut cache = Self::new(max_size, ttl_seconds);
    cache.persist_path = Some(path);
    cache
}
```

- [ ] **Step 2: Implement `persist()`**

```rust
pub async fn persist(&self) -> Result<(), VlorQLError> {
    let Some(ref path) = self.persist_path else {
        return Ok(());
    };
    let entries = self.entries.read().await;
    let bytes = bincode::serialize(&*entries).map_err(|e| {
        VlorQLError::internal("compile cache persist failed", json!({"error": e.to_string()}))
    })?;
    // Atomic write: write to temp file then rename
    let tmp = path.with_extension("tmp");
    tokio::fs::write(&tmp, &bytes).await.map_err(|e| {
        VlorQLError::internal("compile cache persist write failed", json!({"error": e.to_string()}))
    })?;
    tokio::fs::rename(&tmp, path).await.map_err(|e| {
        VlorQLError::internal("compile cache persist rename failed", json!({"error": e.to_string()}))
    })?;
    Ok(())
}
```

Note: `VlorQLError::internal` may not exist — check the error module and use the closest available variant. If no `internal` variant exists, use `VlorQLError::llm(LlmErrorKind::ParseError { details }, json!({...}))` or add a helper. Check `crates/vlorql-core/src/errors/`.

- [ ] **Step 3: Implement `load()`**

```rust
pub async fn load(path: &Path, max_size: u64, ttl_seconds: u64) -> Self {
    let cache = Self::new(max_size, ttl_seconds);
    let bytes = match tokio::fs::read(path).await {
        Ok(b) => b,
        Err(_) => return cache, // file not found or unreadable → empty cache
    };
    let entries: HashMap<CompileCacheKey, CompiledQuery> = match bincode::deserialize(&bytes) {
        Ok(e) => e,
        Err(_) => {
            tracing::warn!(target: "vlorql::cache", "corrupt compile cache file, starting fresh");
            return cache;
        }
    };
    // Re-insert into moka (ignore TTL — entries will expire naturally)
    for (key, query) in &entries {
        cache.inner.insert(key.clone(), Arc::new(query.clone())).await;
        cache.entries.write().await.insert(key.clone(), query.clone());
    }
    cache
}
```

- [ ] **Step 4: Implement `set_persist_path()`**

```rust
pub fn set_persist_path(&mut self, path: PathBuf) {
    self.persist_path = Some(path);
}
```

- [ ] **Step 5: Check error type availability**

Check `crates/vlorql-core/src/errors/mod.rs` for a suitable "internal error" constructor. If none exists, use:
```rust
VlorQLError::internal("...", json!({...}))
```
where `internal` is defined as:
```rust
pub fn internal(msg: impl Into<String>, details: Value) -> Self {
    Self::Llm {
        kind: LlmErrorKind::ApiError { status: 0, message: msg.into() },
        details,
    }
}
```
If it doesn't exist, add it to the `VlorQLError` impl block in `errors/mod.rs`.

- [ ] **Step 6: Run tests**

Run: `cargo test -p vlorql-core -- cache::compile_cache`
Expected: all pass

- [ ] **Step 7: Commit**

```bash
git commit -a -m "feat(cache): add persist/load/set_persist_path to CompileCache"
```

---

### Task 4: Add `with_persistent_compile_cache` to VlorQlBuilder

**Files:**
- Modify: `crates/vlorql/src/builder.rs`

**Interfaces:**
- Consumes: `CompileCache::load()` and `CompileCache::with_persistence()` from Task 3
- Produces: `VlorQlBuilder::with_persistent_compile_cache(max_size, ttl, path) -> Self`
- Produces: Updated `build()` that calls `CompileCache::load()` at startup

- [ ] **Step 1: Add import**

Add to `crates/vlorql/src/builder.rs`:
```rust
use std::path::PathBuf;
```

- [ ] **Step 2: Add `with_persistent_compile_cache` method**

```rust
/// Configures a [`CompileCache`] with bincode disk persistence.
///
/// The cache is loaded from `path` on startup (if the file exists) and
/// persisted on shutdown (via [`VlorQl::drop`]). Uses bincode format.
pub fn with_persistent_compile_cache(
    mut self,
    max_size: u64,
    ttl_seconds: u64,
    path: impl Into<PathBuf>,
) -> Self {
    let path = path.into();
    // Load existing cache from disk, or create empty one
    let cache = if path.exists() {
        // Use tokio runtime if available, otherwise block
        Arc::new(CompileCache::load(&path, max_size, ttl_seconds))
    } else {
        Arc::new(CompileCache::with_persistence(max_size, ttl_seconds, path))
    };
    self.compile_cache = Some(cache);
    self
}
```

Wait — `CompileCache::load()` is async and returns `Self`, not `Arc<CompileCache>`. Need to adjust.

Actually, let's keep it simpler. The builder can use `tokio::runtime::Handle` to run the async `load()`:

```rust
pub fn with_persistent_compile_cache(
    mut self,
    max_size: u64,
    ttl_seconds: u64,
    path: impl Into<PathBuf>,
) -> Self {
    let path = path.into();
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        let cache = handle.block_on(CompileCache::load(&path, max_size, ttl_seconds));
        self.compile_cache = Some(Arc::new(cache));
    } else {
        self.compile_cache = Some(Arc::new(CompileCache::with_persistence(max_size, ttl_seconds, path)));
    }
    self
}
```

- [ ] **Step 3: Run tests**

Run: `cargo test -p vlorql`
Expected: all pass

- [ ] **Step 4: Commit**

```bash
git commit -a -m "feat(builder): add with_persistent_compile_cache"
```

---

### Task 5: Wire auto-persist in VlorQl::Drop

**Files:**
- Modify: `crates/vlorql/src/lib.rs`

**Interfaces:**
- Consumes: `CompileCache::persist()` from Task 3
- Consumes: `VlorQl.compile_cache: Option<Arc<CompileCache>>`

- [ ] **Step 1: Update `VlorQl::Drop` to call `persist()`**

In `crates/vlorql/src/lib.rs`, find the `impl Drop for VlorQl` block and update it:

```rust
impl Drop for VlorQl {
    fn drop(&mut self) {
        if let Some(ref cache) = self.compile_cache {
            if cache.persist_path.is_some() {
                if let Ok(handle) = tokio::runtime::Handle::try_current() {
                    if let Err(e) = handle.block_on(cache.persist()) {
                        tracing::warn!(target: "vlorql", "failed to persist compile cache: {e}");
                    }
                }
            }
        }
        if let Some(mut guard) = self.telemetry_guard.take() {
            guard.shutdown();
        }
    }
}
```

Wait — `persist_path` is a field of `CompileCache`, not a method. Need to check if it's accessible (`pub(crate)` or similar). Since `CompileCache` is in `vlorql-core` and `vlorql` uses it through `pub use`, the field is not directly accessible. Better to add a method:

In `compile_cache.rs`:
```rust
pub fn has_persist_path(&self) -> bool {
    self.persist_path.is_some()
}
```

Or just always call `persist()` (it's a no-op when `persist_path` is None).

```rust
impl Drop for VlorQl {
    fn drop(&mut self) {
        if let Some(ref cache) = self.compile_cache {
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                if let Err(e) = handle.block_on(cache.persist()) {
                    tracing::warn!(target: "vlorql", "failed to persist compile cache: {e}");
                }
            }
        }
        if let Some(mut guard) = self.telemetry_guard.take() {
            guard.shutdown();
        }
    }
}
```

This is safe: `persist()` returns `Ok(())` immediately when no `persist_path` is set.

- [ ] **Step 2: Run tests**

Run: `cargo test -p vlorql`
Expected: all pass

- [ ] **Step 3: Commit**

```bash
git commit -a -m "feat(facade): auto-persist compile cache on VlorQl::Drop"
```

---

### Task 6: Integration tests

**Files:**
- Modify: `crates/vlorql-core/src/cache/compile_cache.rs` (add tests)

- [ ] **Step 1: Add test: persist then load**

```rust
#[tokio::test]
async fn persist_then_load_roundtrip() {
    use std::path::Path;
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("compile_cache.bin");

    // Create cache, insert entry, persist
    let cache = CompileCache::with_persistence(1024, 60, path.clone());
    let plan = make_plan();
    let profile = DialectProfile::default();
    let compiled = make_compiled(SqlDialect::Postgres);
    cache.insert(&plan, &profile, compiled.clone()).await;
    cache.persist().await.expect("persist should succeed");

    // Load into a new cache
    let loaded = CompileCache::load(&path, 1024, 60).await;
    let cached = loaded.get(&plan, &profile).await;
    assert_eq!(cached, Some(Arc::new(compiled)));
}
```

- [ ] **Step 2: Add test: corrupt file loads as empty**

```rust
#[tokio::test]
async fn corrupt_file_loads_gracefully() {
    use std::path::Path;
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("corrupt.bin");
    tokio::fs::write(&path, b"not valid bincode data").await.expect("write");

    let cache = CompileCache::load(&path, 1024, 60).await;
    assert_eq!(cache.size(), 0, "corrupt file should load as empty");
}
```

- [ ] **Step 3: Add test: missing file loads as empty**

```rust
#[tokio::test]
async fn missing_file_loads_gracefully() {
    use std::path::Path;
    let cache = CompileCache::load(Path::new("/tmp/nonexistent_cache_XXXX.bin"), 1024, 60).await;
    assert_eq!(cache.size(), 0, "missing file should load as empty");
}
```

- [ ] **Step 4: Add test: invalidate also removes from persisted state**

```rust
#[tokio::test]
async fn invalidate_removes_from_persist_state() {
    let cache = CompileCache::new(1024, 60);
    let plan = make_plan();
    let profile = DialectProfile::default();
    let compiled = make_compiled(SqlDialect::Postgres);

    cache.insert(&plan, &profile, compiled).await;
    assert_eq!(cache.entries.blocking_read().len(), 1);

    cache.invalidate_plan(&plan, &profile).await;
    assert_eq!(cache.entries.blocking_read().len(), 0);
}
```

- [ ] **Step 5: Run all cache tests**

Run: `cargo test -p vlorql-core -- cache::compile_cache`
Expected: all pass

- [ ] **Step 6: Commit**

```bash
git commit -a -m "test(cache): add persist/load/invalidate persistence tests"
```

---

### Task 7: Verify end-to-end

**Files:**
- None (verification task)

- [ ] **Step 1: Run full workspace tests**

Run: `cargo test --workspace`
Expected: all pass

- [ ] **Step 2: Run clippy**

Run: `cargo clippy --workspace`
Expected: no warnings

- [ ] **Step 3: Commit if any fixes needed**

```bash
git commit -a -m "chore: fix clippy warnings after compile cache persistence"
```
