use s3_proxy::{backends, build_app, sync, AppState, Config};
use tracing::Level;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    if arguments.iter().any(|arg| arg == "--backends") {
        opendal::init_default_registry();
        backends::probe();
        return Ok(());
    }

    let config = Config::from_env()?;
    tracing_subscriber::fmt().with_max_level(Level::INFO).init();

    if arguments
        .first()
        .is_some_and(|arg| arg == "--sync-metadata")
    {
        let options = parse_sync_options(&arguments[1..])?;
        let app_state = AppState::from_config(config).await?;
        let report = sync::sync_metadata(&app_state, &options).await?;
        println!(
            "metadata sync complete: namespaces={}, buckets={}, objects={}",
            report.namespaces, report.buckets, report.objects
        );
        return Ok(());
    }

    let server_host = config.server_host.clone();
    let app_state = AppState::from_config(config).await?;
    let app = build_app(app_state);
    let listener = tokio::net::TcpListener::bind(server_host).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

fn parse_sync_options(arguments: &[String]) -> anyhow::Result<sync::SyncOptions> {
    let mut options = sync::SyncOptions::default();
    let mut arguments = arguments.iter();
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--dry-run" => options.dry_run = true,
            "--namespace" => {
                options.namespace = Some(
                    arguments
                        .next()
                        .ok_or_else(|| anyhow::anyhow!("--namespace requires a value"))?
                        .to_string(),
                );
            }
            _ => anyhow::bail!("unknown --sync-metadata option `{argument}`"),
        }
    }
    Ok(options)
}
