# Schema Grounding Integration — Complete the Pipeline

> **Date:** 2026-07-29
> **Status:** Draft

## Goal

Complete the partially-implemented Schema Grounding (vector search) integration in VlorQl, fixing build errors, wiring the pipeline end-to-end, adding real embedding via OpenAI API, and adding integration tests.

## Data Flow

```
VlorQlBuilder
  .with_vector_search(true)
  .with_schema_indexer(indexer)       // new setter
  .build()
  └─ VlorQl stores vector_search + schema_indexer (Arc)

VlorQl::query()
  └─ PromptBuilder::new(schema, dialect, policy)
       .with_vector_search(bool)
       .with_schema_indexer(Option<Arc<SchemaIndexer>>)
       .build_system_prompt(question)

filter_relevant_tables():
  if vector_search && schema_indexer.is_some()
    → Qdrant search(question) → top-K tables
  TF-IDF keyword match (existing)
  → merge + deduplicate
  → FK neighbor expansion (existing)
```

## Build Fixes

| # | Problem | Fix |
|---|---------|-----|
| 1 | `schema_indexer` field unconditionally references a `#[cfg(feature = "vector-search")]` type | Gate the field and import: `#[cfg(feature = "vector-search")] schema_indexer: ...` |
| 2 | `PromptBuilder` has no setter for new fields | Add `with_vector_search()` and `with_schema_indexer()` |
| 3 | `VlorQlBuilder.build()` doesn't wire through | Pass `self.vector_search` into `VlorQl`, store `schema_indexer`, forward to `PromptBuilder` in query() |
| 4 | `vector_search` in facade not feature-gated | `#[cfg(feature = "vector-search")]` on the field + setter in `VlorQlBuilder` |

## File Changes

### `vlorql-core/src/prompt/builder.rs`
- `#[cfg(feature = "vector-search")]` on `schema_indexer` field
- Add `with_vector_search(mut self, enabled: bool) -> Self`
- Add `with_schema_indexer(mut self, indexer: Arc<SchemaIndexer>) -> Self` (cfg-gated)
- In `filter_relevant_tables()`: when vector_search enabled and indexer present, use `tokio::runtime::Handle::try_current().block_on(indexer.search(...))` to call async from sync. Results are merged with TF-IDF results and deduplicated.
- Async→sync bridge is acceptable because the calling code (`VlorQl::query()`) always runs in a tokio runtime

### `vlorql-core/src/prompt/schema_index.rs`
- Replace `embed_text` placeholder: call OpenAI Embeddings API (`text-embedding-3-small`)
- Add `reqwest` dependency for the HTTP call (behind `vector-search` feature)
- Keep `table_to_text` / `column_to_text` unchanged

### `vlorql/src/builder.rs`
- `#[cfg(feature = "vector-search")]` on `vector_search` field
- Add `with_schema_indexer(mut self, indexer: Arc<SchemaIndexer>) -> Self` (cfg-gated)
- `build()` stores `vector_search` and `schema_indexer` in `VlorQl`

### `vlorql/src/lib.rs`
- Add `#[cfg(feature = "vector-search")]` fields to `VlorQl` struct
- In `query()`: forward to `PromptBuilder`
- When `schema_indexer` is present and `vector_search` enabled, call `indexer.index_schema(&schema)` on first use (lazy init)

### Cargo.toml
- `vlorql-core`: add `reqwest` behind `vector-search` feature
- `vlorql`: propagate `vector-search` feature to `vlorql-core`

## Embeddings

**Provider:** OpenAI `text-embedding-3-small` (512 dimensions, cheaper & faster than v2)
**Client:** `reqwest` with API key from `OPENAI_API_KEY` env var
**Cache:** Embeddings are cached per unique input text in a `HashMap<String, Vec<f32>>` to avoid re-embedding during `index_schema()` (schema rarely changes)

```rust
embed_text(text: &str) -> Result<Vec<f32>>
  // check cache
  // POST https://api.openai.com/v1/embeddings
  // return first embedding vector
```

## Tests

| Test | File | Approach |
|------|------|----------|
| `vector_search_returns_relevant_tables` | `builder.rs` tests | Mock `SchemaIndexer` via a test helper that wraps an Arc<dyn Fn>; verify `filter_relevant_tables` calls it |
| `search_merges_keyword_and_vector` | `builder.rs` tests | Build indexer returning fixed results, verify TF-IDF + vector results are merged and deduplicated |
| `vector_search_disabled_falls_back` | `builder.rs` tests | With `vector_search: false`, vector path is not taken |
| `build_with_vector_search` | integration test | End-to-end with real Qdrant (requires `qdrant` running on localhost) |
| `embed_text_returns_valid_vector` | `schema_index.rs` tests | Test with real OpenAI API (requires `OPENAI_API_KEY` env, skipped otherwise) |

## Out of Scope

- `all-MiniLM-L6-v2` ONNX runtime (can be added later)
- Performance benchmarks
- Schema auto-indexing on schema refresh
