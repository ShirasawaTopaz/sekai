#![cfg_attr(target_os = "none", no_std)]
#![cfg_attr(target_os = "none", no_main)]

#[cfg(not(target_os = "none"))]
fn main() {
    eprintln!("Build sekai-stm32 for the Cortex-M target of the selected board.");
    std::process::exit(1);
}

#[cfg(target_os = "none")]
mod firmware;
