use std::fs;

// The dashboard build output (dist/dashboard) is embedded into the binary at compile
// time via rust-embed (#20). Ensure the folder exists so a dashboard-less `cargo build`
// still compiles — it then serves a "not built" placeholder. Build the dashboard
// (`npm install && npm run build`) before the binary to embed the real UI.
fn main() {
    let _ = fs::create_dir_all("dist/dashboard");
    println!("cargo:rerun-if-changed=dist/dashboard");
}
