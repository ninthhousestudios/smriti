//! BGE-M3 ONNX embedding pipeline (gated behind `embedding` feature).
//!
//! Computes dense embeddings for tier-1 documents not matching `[no-embed]`.
//! Stores vectors in the `document_vectors` sqlite-vec virtual table.
//! When available, search uses both BM25 and dense retrieval with RRF merge.

#[cfg(feature = "embedding")]
mod inner {
    use std::path::Path;

    use ndarray::Axis;
    use ort::session::Session;
    use ort::value::Tensor;
    use rusqlite::{Connection, params};

    use crate::error::{Result, SmritiError};

    const EMBEDDING_DIM: usize = 1024;
    const MAX_TOKENS: usize = 512;

    pub struct Embedder {
        session: Session,
        tokenizer: tokenizers::Tokenizer,
        model_id: String,
    }

    impl Embedder {
        pub fn load(model_dir: &Path) -> Result<Self> {
            let model_path = model_dir.join("model.onnx");
            let tokenizer_path = model_dir.join("tokenizer.json");

            if !model_path.exists() {
                return Err(SmritiError::Other(format!(
                    "ONNX model not found at {}",
                    model_path.display()
                )));
            }
            if !tokenizer_path.exists() {
                return Err(SmritiError::Other(format!(
                    "Tokenizer not found at {}",
                    tokenizer_path.display()
                )));
            }

            let session = Session::builder()
                .map_err(|e| SmritiError::Other(format!("ORT session builder error: {e}")))?
                .commit_from_file(&model_path)
                .map_err(|e| SmritiError::Other(format!("ORT model load error: {e}")))?;

            let tokenizer = tokenizers::Tokenizer::from_file(&tokenizer_path)
                .map_err(|e| SmritiError::Other(format!("Failed to load tokenizer: {e}")))?;

            let model_id = model_dir
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "bge-m3".to_string());

            Ok(Self { session, tokenizer, model_id })
        }

        pub fn model_id(&self) -> &str {
            &self.model_id
        }

        pub fn embed_text(&mut self, text: &str) -> Result<Vec<f32>> {
            let encoding = self.tokenizer
                .encode(text, true)
                .map_err(|e| SmritiError::Other(format!("Tokenization error: {e}")))?;

            let ids: Vec<i64> = encoding.get_ids().iter()
                .take(MAX_TOKENS)
                .map(|&id| id as i64)
                .collect();
            let attention: Vec<i64> = encoding.get_attention_mask().iter()
                .take(MAX_TOKENS)
                .map(|&m| m as i64)
                .collect();
            let token_types: Vec<i64> = encoding.get_type_ids().iter()
                .take(MAX_TOKENS)
                .map(|&t| t as i64)
                .collect();

            let seq_len = ids.len();

            let ids_tensor = Tensor::from_array((vec![1i64, seq_len as i64], ids))
                .map_err(|e| SmritiError::Other(format!("Tensor error: {e}")))?;
            let attn_tensor = Tensor::from_array((vec![1i64, seq_len as i64], attention))
                .map_err(|e| SmritiError::Other(format!("Tensor error: {e}")))?;
            let types_tensor = Tensor::from_array((vec![1i64, seq_len as i64], token_types))
                .map_err(|e| SmritiError::Other(format!("Tensor error: {e}")))?;

            let outputs = self.session.run(ort::inputs![ids_tensor, attn_tensor, types_tensor])
                .map_err(|e| SmritiError::Other(format!("ORT inference error: {e}")))?;

            let output_view = outputs[0].try_extract_array::<f32>()
                .map_err(|e| SmritiError::Other(format!("Tensor extraction error: {e}")))?;

            // BGE-M3 output shape: [batch=1, seq_len, hidden_dim]
            // Use CLS token embedding (index 0 along seq_len axis)
            let cls_embedding: Vec<f32> = output_view
                .index_axis(Axis(0), 0) // batch dim
                .index_axis(Axis(0), 0) // CLS token
                .iter()
                .copied()
                .collect();

            if cls_embedding.len() != EMBEDDING_DIM {
                return Err(SmritiError::Other(format!(
                    "Expected {EMBEDDING_DIM}-dim embedding, got {}",
                    cls_embedding.len()
                )));
            }

            // L2 normalize
            let norm: f32 = cls_embedding.iter().map(|x| x * x).sum::<f32>().sqrt();
            let normalized: Vec<f32> = if norm > 0.0 {
                cls_embedding.iter().map(|x| x / norm).collect()
            } else {
                cls_embedding
            };

            Ok(normalized)
        }

        pub fn embed_document_text(
            &mut self,
            title: Option<&str>,
            summary: Option<&str>,
            topics: &[String],
            content: Option<&str>,
        ) -> Result<Vec<f32>> {
            let mut text = String::new();
            if let Some(t) = title {
                text.push_str(t);
                text.push('\n');
            }
            if let Some(s) = summary {
                text.push_str(s);
                text.push('\n');
            }
            if !topics.is_empty() {
                text.push_str(&topics.join(", "));
                text.push('\n');
            }
            if let Some(c) = content {
                let remaining = 2000usize.saturating_sub(text.len());
                if remaining > 0 {
                    text.push_str(&c[..c.len().min(remaining)]);
                }
            }

            if text.is_empty() {
                return Err(SmritiError::Other("No text to embed".to_string()));
            }

            self.embed_text(&text)
        }
    }

    pub fn store_embedding(conn: &Connection, content_hash: &str, embedding: &[f32], model_id: &str) -> Result<()> {
        let blob: Vec<u8> = embedding.iter().flat_map(|f| f.to_le_bytes()).collect();

        conn.execute(
            "INSERT OR REPLACE INTO document_vectors (content_hash, embedding) VALUES (?1, ?2)",
            params![content_hash, blob],
        )?;

        conn.execute(
            "UPDATE documents SET embedding_model = ?1 WHERE content_hash = ?2 AND embedding_model IS NULL",
            params![model_id, content_hash],
        )?;

        Ok(())
    }

    pub fn search_dense(conn: &Connection, query_embedding: &[f32], k: u32) -> Result<Vec<(String, f64)>> {
        let blob: Vec<u8> = query_embedding.iter().flat_map(|f| f.to_le_bytes()).collect();

        let mut stmt = conn.prepare(
            "SELECT content_hash, distance
             FROM document_vectors
             WHERE embedding MATCH ?1
             ORDER BY distance
             LIMIT ?2",
        )?;

        let results: Vec<(String, f64)> = stmt.query_map(params![blob, k], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))
        })?.filter_map(|r| r.ok()).collect();

        Ok(results)
    }

    /// Reciprocal Rank Fusion: merge BM25 and dense results.
    pub fn rrf_merge(
        bm25_hashes: &[String],
        dense_hashes: &[String],
        k_constant: f64,
    ) -> Vec<String> {
        use std::collections::HashMap;

        let mut scores: HashMap<String, f64> = HashMap::new();

        for (rank, hash) in bm25_hashes.iter().enumerate() {
            *scores.entry(hash.clone()).or_default() += 1.0 / (k_constant + rank as f64 + 1.0);
        }
        for (rank, hash) in dense_hashes.iter().enumerate() {
            *scores.entry(hash.clone()).or_default() += 1.0 / (k_constant + rank as f64 + 1.0);
        }

        let mut ranked: Vec<(String, f64)> = scores.into_iter().collect();
        ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        ranked.into_iter().map(|(hash, _)| hash).collect()
    }

    pub fn embed_pending_documents(conn: &Connection, embedder: &mut Embedder) -> Result<u64> {
        let mut count = 0u64;

        let mut stmt = conn.prepare(
            "SELECT d.content_hash, d.title, d.summary, d.topics
             FROM documents d
             LEFT JOIN document_vectors v ON v.content_hash = d.content_hash
             WHERE v.content_hash IS NULL
               AND d.embed_excluded = 0
               AND d.is_binary = 0",
        )?;
        let rows: Vec<(String, Option<String>, Option<String>, Option<String>)> =
            stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            })?.filter_map(|r| r.ok()).collect();

        for (content_hash, title, summary, topics_json) in &rows {
            let topics: Vec<String> = topics_json
                .as_ref()
                .and_then(|j| serde_json::from_str(j).ok())
                .unwrap_or_default();

            match embedder.embed_document_text(title.as_deref(), summary.as_deref(), &topics, None) {
                Ok(embedding) => {
                    store_embedding(conn, content_hash, &embedding, embedder.model_id())?;
                    count += 1;
                }
                Err(e) => {
                    tracing::warn!(
                        "Failed to embed {}: {}",
                        &content_hash[..12.min(content_hash.len())],
                        e
                    );
                }
            }
        }

        Ok(count)
    }
}

#[cfg(feature = "embedding")]
pub use inner::*;
