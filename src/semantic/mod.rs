//! Lexical-semantic retrieval over the sigil entity index.
//!
//! Today (Spike 1) this is pure BM25 over `name + sig + doc` text per
//! entity. Future spikes (#3-#5) layer static embeddings, RRF fusion, and
//! code-aware rerank signals on top of the same `Index` interface.

pub mod bm25;
pub mod cmd;
pub mod m2v;
pub mod m2v_index;
pub mod rerank;
pub mod tokenize;
