// The web UI is embedded from web/dist in release builds. Rebuild when it appears or changes,
// so building the UI after a first API-only build is picked up by the next cargo build.
fn main() {
    println!("cargo:rerun-if-changed=web/dist");
}
