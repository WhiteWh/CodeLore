//! The secret-scan vectors are one set of cases shared with Zerus, which keeps its
//! own copy (tests/fixtures/secret-scan-vectors.json there) to test its team-text
//! scanner against. Each repository pins the same SHA-256 of the file (its LF form,
//! so a CRLF checkout agrees); a change to the vectors is made in both, with this
//! constant, or neither repository's test stays green.
use sha2::{Digest, Sha256};

const VECTORS_SHA256: &str = "dc44e984cd0d507751339cec34a1b5eec32a7de0463c5bff75c82f56267d9cf3";

#[test]
fn the_shared_secret_vectors_are_the_ones_zerus_pins() {
    let text = include_str!("fixtures/secret-scan-vectors.json").replace("\r\n", "\n");
    assert_eq!(
        format!("{:x}", Sha256::digest(text.as_bytes())),
        VECTORS_SHA256,
        "the secret-scan vectors changed: change Zerus's tests/fixtures copy and its pinned hash with them"
    );
}
