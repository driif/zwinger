fn main() {
    // tauri-build needs the bundled resource dir. scripts/fetch-pi.sh fills
    // it before `tauri dev` and `tauri build`; a plain cargo build only needs
    // it to exist.
    std::fs::create_dir_all("resources/pi").expect("create resources/pi");
    // The cache key of the host pi's TUI probe has the runtime version, so an
    // upgrade of microsandbox probes again.
    println!("cargo:rerun-if-changed=Cargo.lock");
    let lock = std::fs::read_to_string("Cargo.lock").expect("read Cargo.lock");
    let version = lock
        .split("[[package]]")
        .find(|package| package.contains("\nname = \"microsandbox\"\n"))
        .and_then(|package| package.lines().find_map(|line| line.strip_prefix("version = \"")))
        .and_then(|rest| rest.strip_suffix('"'))
        .expect("microsandbox in Cargo.lock");
    println!("cargo:rustc-env=MICROSANDBOX_VERSION={version}");
    tauri_build::build()
}
