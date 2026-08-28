//! One-shot manual verification for the cloudflared download+verify path.
//! Downloads the pinned artifact (with mirror fallback) into a temporary
//! desktop home and reports the outcome. Run:
//!   cargo run -p dsh-core --example remote_fetch_cloudflared

use dsh_core::{ApplicationPaths, remote};

fn main() {
    let temp = tempfile::tempdir().expect("temp home");
    let paths = ApplicationPaths::from_home(temp.path());
    let started = std::time::Instant::now();
    match remote::ensure_cloudflared(&paths) {
        Ok(binary) => {
            let size = std::fs::metadata(&binary).map(|m| m.len()).unwrap_or(0);
            println!(
                "OK: {} ({} bytes) in {:?}",
                binary.display(),
                size,
                started.elapsed()
            );
        }
        Err(error) => {
            println!("FAILED after {:?}: {error}", started.elapsed());
            std::process::exit(1);
        }
    }
}
