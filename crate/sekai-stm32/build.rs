use std::{env, fs, path::PathBuf};

fn main() {
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("none") {
        return;
    }
    let board = match (
        env::var_os("CARGO_FEATURE_BLUE_PILL").is_some(),
        env::var_os("CARGO_FEATURE_DISCOVERY").is_some(),
        env::var("TARGET").as_deref(),
    ) {
        (true, false, Ok("thumbv7m-none-eabi")) => "blue-pill",
        (false, true, Ok("thumbv7em-none-eabi")) => "discovery",
        _ => panic!("select blue-pill on thumbv7m-none-eabi or discovery on thumbv7em-none-eabi"),
    };
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let memory = format!("{board}.x");
    println!("cargo:rerun-if-changed={memory}");
    fs::copy(memory, out.join("memory.x")).unwrap();
    println!("cargo:rustc-link-search={}", out.display());
    println!("cargo:rustc-link-arg-bin=sekai-stm32=-Tlink.x");
}
