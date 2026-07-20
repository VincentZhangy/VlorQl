//! Pluggable caching layer for VlorQl core operations.
//!
//! This module defines the generic [`Cache`] trait and the key types
//! used to identify cached artifacts:
//!
//! * `SchemaCacheKey` — versioned schema snapshots.
//! * `CompileCacheKey` — compiled SQL keyed by plan hash + dialect.
//!
//! # Examples
//!
//! ```
//! use vlorql_core::cache::{Cache, NoopCache};
//!
//! let cache = NoopCache::<String, String>::new();
//! assert_eq!(cache.size(), 0);
//! ```

mod compile_cache;
mod key;
mod normalize;
mod prompt_cache;
mod schema_cache;
mod traits;

pub use compile_cache::CompileCache;
pub use key::{CompileCacheKey, SchemaCacheKey};
pub use normalize::normalize_plan;
pub(crate) use prompt_cache::hash_policy;
pub use prompt_cache::{PromptCache, PromptCacheKey};
pub use schema_cache::SchemaCache;
pub use traits::{Cache, MemoryCache, NoopCache};