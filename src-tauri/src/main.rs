// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    match zwinger_lib::run_update_helper() {
        Ok(true) => return,
        Ok(false) => {}
        Err(error) => {
            eprintln!("update handoff failed: {error}");
            std::process::exit(1);
        }
    }
    zwinger_lib::run()
}
