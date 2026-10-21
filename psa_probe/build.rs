use std::{env, path::PathBuf};
fn main() {
    println!("cargo:rerun-if-env-changed=PSA_PROBE_ASSETS");
    let architecture = env::var("CARGO_CFG_TARGET_ARCH").unwrap();
    let assets = env::var_os("PSA_PROBE_ASSETS")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap())
                .join("../target/probe-assets")
                .join(&architecture)
        });
    for name in [
        "rg",
        "fd",
        "ripgrep-COPYING",
        "ripgrep-LICENSE-MIT",
        "fd-LICENSE-MIT",
        "fd-LICENSE-APACHE",
    ] {
        let path = assets.join(name);
        assert!(
            path.is_file(),
            "missing bundled tool asset {}; run PSA_PREPARE_ONLY=1 scripts/package-probe.sh {}-unknown-linux-musl",
            path.display(),
            architecture
        );
        println!("cargo:rerun-if-changed={}", path.display());
    }
    println!(
        "cargo:rustc-env=PSA_BUNDLED_ASSETS={}",
        assets.canonicalize().unwrap().display()
    );
}
