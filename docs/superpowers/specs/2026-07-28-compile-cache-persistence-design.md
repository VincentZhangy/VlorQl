# CompileCache Bincode Persistence

> **Date:** 2026-07-28
> **Status:** Draft

## Problem

`CompileCache` is in-memory only — the cache is lost on every process restart.
After restart, every query re-compiles from scratch, wasting LLM calls and CPU time.

## Current State

- `CompileCache` backed by `moka::future::Cache<CompileCacheKey, Arc<CompiledQuery>>`
- `CompileCacheKey` — derives `Serialize`/`Deserialize`
- `CompiledQuery` — derives `Serialize`/`Deserialize`
- No iteration API on `moka::future::Cache`

## Design

### Shadow HashMap for persistence

Since moka does not expose an entry iterator, a `tokio::sync::RwLock<HashMap<CompileCacheKey, CompiledQuery>>`
is kept in sync with the moka cache. All mutating operations (`insert`, `invalidate`, `clear`)
update both stores. Reads (`get`) use only the fast moka cache.

### New types and methods

```rust
pub struct CompileCache {
    inner: MokaCache<CompileCacheKey, Arc<CompiledQuery>>,
    entries: tokio::sync::RwLock<HashMap<CompileCacheKey, CompiledQuery>>,
    max_size: u64,
    persist_path: Option<PathBuf>,
}

impl CompileCache {
    /// Creates a cache that auto-persists to `path` on drop.
    pub fn with_persistence(max_size: u64, ttl_seconds: u64, path: PathBuf) -> Self;

    /// Persists all entries to the configured path as bincode.
    pub async fn persist(&self) -> Result<(), VlorQLError>;

    /// Loads a previously persisted cache from disk.
    /// Returns an empty cache if the file does not exist or is corrupt.
    pub async fn load(path: &Path) -> Self;

    /// Sets or changes the persistence path at runtime.
    pub fn set_persist_path(&mut self, path: PathBuf);
}
```

### Lifecycle

| Phase | Action |
|-------|--------|
| **Builder** | `with_persistent_compile_cache(max_size, ttl, path)` calls `CompileCache::load(path)` then sets persist_path |
| **Runtime** | `insert`/`invalidate`/`clear` update both moka + HashMap |
| **Drop (VlorQl)** | Calls `persist()` via `tokio::runtime::Handle::current().block_on` |
| **Error handling** | Persist failures are logged (warn level), not propagated — cache loss is non-fatal |

### VlorQlBuilder API

```rust
pub fn with_persistent_compile_cache(
    mut self,
    max_size: u64,
    ttl_seconds: u64,
    path: impl Into<PathBuf>,
) -> Self;
```

### Dependencies

Add to `[workspace.dependencies]` in root `Cargo.toml`:
```toml
bincode = "1"
```

Add to `crates/vlorql-core/Cargo.toml`:
```toml
bincode.workspace = true
```

### Files changed

| File | Change |
|------|--------|
| `Cargo.toml` (workspace) | Add `bincode = "1"` |
| `crates/vlorql-core/Cargo.toml` | Add `bincode.workspace = true` |
| `crates/vlorql-core/src/cache/compile_cache.rs` | Add `entries`, `persist_path` fields; add `with_persistence`, `persist`, `load`, `set_persist_path`; update `insert`/`invalidate`/`clear` to sync HashMap |
| `crates/vlorql/src/builder.rs` | Add `with_persistent_compile_cache` method; update `build()` to load persisted cache |
| `crates/vlorql/src/lib.rs` | Wire `persist()` call in `VlorQl::Drop` |

### Testing

- **Unit test**: Persist cache → drop → load → verify entries match
- **Unit test**: Insert after load → entries merged correctly
- **Unit test**: Corrupt file → loads gracefully (empty cache)
- **Unit test**: Invalidate after persist → entry removed from both stores
