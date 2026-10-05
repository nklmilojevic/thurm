//! Thurm's Linux front end: a GTK 4 / libadwaita client of `thurmd`, like the macOS app is an
//! AppKit one. It drives the daemon through the same C ABI (`thurm-ffi`), linked as a Rust
//! library.

#[cfg(target_os = "linux")]
mod actions;
#[cfg(target_os = "linux")]
mod app;
#[cfg(target_os = "linux")]
mod boxdraw;
#[cfg(target_os = "linux")]
mod config;
#[cfg(target_os = "linux")]
mod core;
#[cfg(target_os = "linux")]
mod dialogs;
#[cfg(target_os = "linux")]
mod integrations;
#[cfg(target_os = "linux")]
mod keys;
#[cfg(target_os = "linux")]
mod model;
#[cfg(target_os = "linux")]
mod notify;
#[cfg(target_os = "linux")]
mod palette;
#[cfg(target_os = "linux")]
mod processes;
#[cfg(target_os = "linux")]
mod quick;
#[cfg(target_os = "linux")]
mod remote;
#[cfg(target_os = "linux")]
mod render;
#[cfg(target_os = "linux")]
mod sidebar;
#[cfg(target_os = "linux")]
mod splitbox;
#[cfg(target_os = "linux")]
mod tab;
#[cfg(target_os = "linux")]
mod termarea;
#[cfg(target_os = "linux")]
mod view;
#[cfg(target_os = "linux")]
mod window;

#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    app::run()
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("thurm-gtk is the Linux front end; on macOS use Thurm.app");
    std::process::exit(1);
}
