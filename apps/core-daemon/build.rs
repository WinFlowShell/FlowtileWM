#[cfg(windows)]
fn main() {
    let uiaccess_enabled = std::env::var_os("FLOWTILE_UIACCESS_MANIFEST").is_some();
    let manifest_name = if uiaccess_enabled {
        "flowtile-core-daemon-uiaccess.manifest"
    } else {
        "flowtile-core-daemon.manifest"
    };
    let manifest_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(manifest_name);
    println!("cargo:rerun-if-changed={}", manifest_path.display());
    println!("cargo:rustc-link-arg-bin=flowtile-core-daemon=/MANIFEST:EMBED");
    println!(
        "cargo:rustc-link-arg-bin=flowtile-core-daemon=/MANIFESTINPUT:{}",
        manifest_path.display()
    );
    let _ = uiaccess_enabled;
    println!("cargo:rustc-link-arg-bin=flowtile-core-daemon=/MANIFESTUAC:NO");
}

#[cfg(not(windows))]
fn main() {}
