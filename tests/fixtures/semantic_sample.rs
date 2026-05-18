//! Fixture for semantic_integration.rs.
//!
//! Three deliberately semantically-distinct functions whose names, signatures,
//! and docstrings cluster on three different topics. BM25 should easily
//! discriminate them on topical queries even though each only mentions its
//! topic in `name + sig + doc` (no body text yet).

/// Parse a JSON document from a file path and return the structured value.
pub fn parse_json_file(_path: &str) -> Result<serde_json::Value, std::io::Error> {
    todo!()
}

/// Compile Rust source code into an executable binary using rustc.
pub fn compile_rust_binary(_source: &str) -> Result<Vec<u8>, std::io::Error> {
    todo!()
}

/// Send an HTTP GET request to a remote URL and return the response body.
pub fn send_http_request(_url: &str) -> Result<String, std::io::Error> {
    todo!()
}
