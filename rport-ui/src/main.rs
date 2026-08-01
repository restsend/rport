mod app;
mod client_manager;
mod config;

use app::RportApp;

fn main() -> eframe::Result<()> {
    let log_store = app::LogStore::new();
    let log_entries = log_store.entries.clone();

    // Dual logging: stderr (with colors) + in-app buffer
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    use tracing_subscriber::{EnvFilter, Registry};

    let stderr_layer = tracing_subscriber::fmt::Layer::new()
        .with_writer(std::io::stderr)
        .with_ansi(true);
    let buffer_layer = tracing_subscriber::fmt::Layer::new()
        .with_writer(move || app::LogWriter::new(log_entries.clone()))
        .with_ansi(false);

    Registry::default()
        .with(EnvFilter::new("debug"))
        .with(stderr_layer)
        .with(buffer_layer)
        .init();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([800.0, 600.0])
            .with_min_inner_size([600.0, 400.0])
            .with_title("RPort Manager"),
        ..Default::default()
    };

    tokio::runtime::Runtime::new()
        .expect("Failed to create tokio runtime")
        .block_on(async {
            eframe::run_native(
                "RPort Manager",
                options,
                Box::new(|_cc| Ok(Box::new(RportApp::new(log_store)))),
            )
        })
}
