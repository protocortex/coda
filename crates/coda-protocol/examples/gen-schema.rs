// SPDX-License-Identifier: AGPL-3.0-only

//! Rewrites `schema/coda-protocol.schema.json` from the Rust types.
//! Run with `cargo run -p coda-protocol --example gen-schema`.

use std::fs;
use std::path::PathBuf;

fn main() -> std::io::Result<()> {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../schema/coda-protocol.schema.json");
    fs::write(&path, coda_protocol::schema::render())?;
    println!("wrote {}", path.display());
    Ok(())
}
