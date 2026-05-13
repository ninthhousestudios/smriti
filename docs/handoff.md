# Handoff

## Current state

smriti v0.3.4 installed. Embedding feature fully removed (sqlite-vec, vec0, embedding.rs, IndexedNoEmbed, embed_excluded). DB was corrupted and has been deleted; smriti-watch is rescanning to rebuild a fresh index.db with the new schema.

## To pick up

- Verify rescan completed: `smriti health` should return status ok with correct file counts
- Close yojana task smriti/31 once health is confirmed
- The `[features]` section in Cargo.toml is now `default = []` with no other features — can be removed if desired (cosmetic)

## Context

- The corruption was in FTS5 or vec0 virtual tables; main B-tree tables (paths, documents) were intact but the whole DB was rebuilt from scratch since the schema changed
- `[no-embed]` section in .smritiignore files is still parsed for backwards compatibility but patterns are silently discarded
