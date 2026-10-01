//! Model-id vector lookup, feature-gated.
//!
//! Scope: answering "which provider owns this model id" from an embedding of
//! that id. Storing and serving embeddings is the media sibling's job
//! (docs/04, `/v1/embeddings`); nothing here stores a client-visible vector.
//!
//! `usearch` proper is a C++ HNSW build — real RAM, real build time — set
//! against a <35 MB idle budget (docs/00). So the default build exposes the
//! whole surface and refuses to index, and `--features usearch` links a
//! brute-force cosine index instead: RAM cost, no C++ toolchain. A caller that
//! gets [`VectorError::Disabled`] learns the feature name rather than finding
//! out that its index was silently empty.

use ar_core::Strng;

/// Ways a vector operation can fail.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum VectorError {
    /// This build has no index behind the surface.
    #[error("vector index is disabled in this build; rebuild with --features usearch")]
    Disabled,
    /// The query width does not match the stored vectors.
    #[error("vector width mismatch: index holds {stored}, query is {query}")]
    Width {
        /// Width every stored vector has.
        stored: usize,
        /// Width the query had.
        query: usize,
    },
}

/// A model-id index small enough to keep inside a proxy that serves completions.
pub trait VectorIndex {
    /// Adds `vector` under `key`, replacing any previous vector for it.
    fn add(&mut self, key: Strng, vector: Vec<f32>) -> Result<(), VectorError>;

    /// The `k` closest keys to `query`, nearest first. Fewer than `k` results
    /// when the index holds fewer.
    fn search(&self, query: &[f32], k: usize) -> Result<Vec<(Strng, f32)>, VectorError>;

    /// How many keys are indexed.
    fn len(&self) -> usize;

    /// Whether nothing is indexed.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The index this build ships: cosine under `usearch`, a refusal without it.
#[cfg(feature = "usearch")]
#[derive(Default)]
pub struct Index(Vec<(Strng, Vec<f32>)>);

/// The index this build ships: a refusal, because `usearch` is off by default.
#[cfg(not(feature = "usearch"))]
#[derive(Default)]
pub struct Index;

/// Cosine similarity of two equal-width vectors; 0.0 when either is degenerate,
/// so a zero vector sorts last instead of producing a NaN ordering.
#[cfg(feature = "usearch")]
fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let norm = |v: &[f32]| v.iter().map(|x| x * x).sum::<f32>().sqrt();
    let (na, nb) = (norm(a), norm(b));
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na * nb)
    }
}

#[cfg(feature = "usearch")]
impl VectorIndex for Index {
    // ponytail: exhaustive scan on every add and search. O(n) per lookup is the
    // point — swap the bodies for usearch HNSW when the indexed model-id count
    // outgrows a linear pass, keeping this trait as the seam.
    fn add(&mut self, key: Strng, vector: Vec<f32>) -> Result<(), VectorError> {
        match self.0.iter_mut().find(|(k, _)| *k == key) {
            Some(slot) => slot.1 = vector,
            None => self.0.push((key, vector)),
        }
        Ok(())
    }

    fn search(&self, query: &[f32], k: usize) -> Result<Vec<(Strng, f32)>, VectorError> {
        let Some(width) = self.0.first().map(|(_, v)| v.len()) else {
            return Ok(Vec::new());
        };
        if width != query.len() {
            return Err(VectorError::Width {
                stored: width,
                query: query.len(),
            });
        }
        let mut scored: Vec<(Strng, f32)> = self
            .0
            .iter()
            .map(|(key, v)| (std::sync::Arc::clone(key), cosine(v, query)))
            .collect();
        scored.sort_by(|a, b| b.1.total_cmp(&a.1));
        scored.truncate(k);
        Ok(scored)
    }

    fn len(&self) -> usize {
        self.0.len()
    }
}

#[cfg(not(feature = "usearch"))]
impl VectorIndex for Index {
    fn add(&mut self, _key: Strng, _vector: Vec<f32>) -> Result<(), VectorError> {
        Err(VectorError::Disabled)
    }

    fn search(&self, _query: &[f32], _k: usize) -> Result<Vec<(Strng, f32)>, VectorError> {
        Err(VectorError::Disabled)
    }

    fn len(&self) -> usize {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "usearch")]
    #[test]
    fn ranks_nearest_key_first() {
        let mut i = Index::default();
        i.add(Strng::from("near"), vec![1.0, 0.0]).unwrap();
        i.add(Strng::from("far"), vec![0.0, 1.0]).unwrap();
        assert_eq!(i.search(&[1.0, 0.1], 1).unwrap()[0].0, Strng::from("near"));
        assert_eq!(i.len(), 2);
    }

    #[cfg(feature = "usearch")]
    #[test]
    fn rejects_query_of_a_different_width() {
        let mut i = Index::default();
        i.add(Strng::from("a"), vec![1.0, 0.0]).unwrap();
        assert_eq!(
            i.search(&[1.0], 1),
            Err(VectorError::Width {
                stored: 2,
                query: 1
            })
        );
    }

    #[cfg(not(feature = "usearch"))]
    #[test]
    fn refuses_rather_than_silently_staying_empty() {
        let mut i = Index;
        assert_eq!(
            i.add(Strng::from("a"), vec![1.0]),
            Err(VectorError::Disabled)
        );
        assert_eq!(i.search(&[1.0], 1), Err(VectorError::Disabled));
        assert!(i.is_empty());
    }
}
