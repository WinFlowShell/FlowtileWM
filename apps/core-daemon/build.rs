#[cfg(windows)]
fn main() {
    let manifest_path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("flowtile-core-daemon.manifest");
    println!("cargo:rerun-if-changed={}", manifest_path.display());
    println!("cargo:rustc-link-arg-bin=flowtile-core-daemon=/MANIFEST:EMBED");
    println!(
        "cargo:rustc-link-arg-bin=flowtile-core-daemon=/MANIFESTINPUT:{}",
        manifest_path.display()
    );
}

#[cfg(not(windows))]
fn main() {}
