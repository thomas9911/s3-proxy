use s3_proxy::{backends, build_app, AppState, Config};
use tracing::Level;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if std::env::args().any(|arg| arg == "--backends") {
        opendal::init_default_registry();
        backends::probe();
        return Ok(());
    }

    let config = Config::from_env()?;
    tracing_subscriber::fmt().with_max_level(Level::INFO).init();

    let server_host = config.server_host.clone();
    let app_state = AppState::from_config(config).await?;
    let app = build_app(app_state);
    let listener = tokio::net::TcpListener::bind(server_host).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
