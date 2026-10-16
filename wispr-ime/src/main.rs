#![forbid(unsafe_code)]

use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() {
    let arg = std::env::args().nth(1);
    match arg.as_deref() {
        Some("--version") => {
            println!("wispr-ime {}", env!("CARGO_PKG_VERSION"));
            return;
        }
        Some("--help") | Some("-h") => {
            println!(
                "wispr-ime: IBus front for GNOME that hands input to fcitx5.\n\n\
                 Run it as a session service after fcitx5 (started with\n\
                 --disable ibusfrontend). Log level: WISPR_IME_LOG (default info).\n\
                 Dictation calls org.wisprime.WisprIme1.Commit(text) -> bool."
            );
            return;
        }
        _ => {}
    }
    let filter =
        EnvFilter::try_from_env("WISPR_IME_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
    if let Err(e) = wispr_ime::daemon::run().await {
        tracing::error!("{e}");
        std::process::exit(1);
    }
}
