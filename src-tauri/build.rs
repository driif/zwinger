fn main() {
    // tauri-build needs the bundled resource dir. scripts/fetch-pi.sh fills
    // it before `tauri dev` and `tauri build`; a plain cargo build only needs
    // it to exist.
    std::fs::create_dir_all("resources/pi").expect("create resources/pi");
    tauri_build::build()
}
