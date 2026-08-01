//! Dev tool: print the raw sha256 and the signing-independent Mach-O content sha256 for each
//! file argument — the two values the sidecar pins record (scripts/*-pin.json + the code-side
//! pin tables). Used when re-recording from-source mac pins after a toolchain change (the
//! procedure documented in scripts/whisper-pin.json's header).
//!
//! Usage: cargo run -p anvil-media --example macho_hash -- <file> [<file>…]

use std::path::Path;

fn main() {
    for arg in std::env::args().skip(1) {
        let path = Path::new(&arg);
        let raw = anvil_media::sha256_file(path).unwrap_or_else(|e| format!("<error: {e}>"));
        let content =
            anvil_media::macho_content_sha256(path).unwrap_or_else(|e| format!("<error: {e}>"));
        println!("{arg}\n  raw     {raw}\n  content {content}");
    }
}
