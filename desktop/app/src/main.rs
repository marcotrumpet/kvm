//! KVM for Windows (and, later, Linux): a tray app that shares mouse, keyboard and
//! clipboard with a paired Mac. The core runs on a tokio runtime; the main thread runs the
//! OS's UI loop and input hooks (kvm-platform).

#![cfg_attr(windows, windows_subsystem = "windows")] // no console window

mod app;
mod clipboard;
mod config;

use std::sync::{Arc, Mutex};

use anyhow::Result;
use kvm_core::Node;
use kvm_platform::{dialog, ui, updater};

/// Updates: the same GitHub releases and signing key as the Mac app (see macos/release.sh).
const FEED_URL: &str = "https://github.com/marcotrumpet/kvm/releases/latest/download/appcast-windows.xml";
const UPDATE_PUBLIC_KEY: &str = "uTJI1jCYearLasko0mLDJI9aF2fec64AWr13pneHktA=";
const VERSION: &str = match option_env!("KVM_VERSION") {
    Some(v) => v,
    None => "0.0.0-dev",
};

fn main() {
    if let Err(err) = run() {
        tracing::error!("{err:#}");
        dialog::tell("KVM", &format!("KVM couldn't start: {err:#}"));
    }
}

fn run() -> Result<()> {
    let log_dir = config::log_dir();
    std::fs::create_dir_all(&log_dir)?;
    let log = std::fs::OpenOptions::new().create(true).append(true).open(log_dir.join("kvm.log"))?;
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,iroh=warn".into()),
        )
        .with_ansi(false)
        .with_writer(Mutex::new(log))
        .init();

    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let config_dir = config::config_dir();
    let node = runtime.block_on(Node::bind(&config_dir, config::computer_name()))?;
    tracing::info!("KVM {VERSION} started, id {}", node.id());

    let app = app::App::new(runtime.handle().clone(), node.clone(), config_dir);
    app.start();
    if updater::start(FEED_URL, UPDATE_PUBLIC_KEY, VERSION, quit_for_update) {
        tracing::info!("checking for updates");
    }
    let result = ui::run(app.clone() as Arc<dyn kvm_platform::Delegate>, app.hotkey());
    app.shutdown();
    runtime.block_on(node.close());
    result
}

/// WinSparkle has an update ready to install: quit, so it can replace this executable.
extern "C" fn quit_for_update() {
    ui::quit();
}
