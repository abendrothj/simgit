mod commit_scheduler;
mod daemon;
mod vfs;
mod borrow;
mod delta;
mod session;
mod rpc;
mod events;
mod config;
mod metrics;

use anyhow::Result;
use opentelemetry::global;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry::KeyValue;
use opentelemetry_otlp::WithExportConfig;
use tracing::info;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing()?;

    let cfg = config::Config::load()?;
    info!(repo = %cfg.repo_path.display(), state = %cfg.state_dir.display(), "simgitd starting");

    let result = daemon::run(cfg).await;
    global::shutdown_tracer_provider();
    result
}

fn init_tracing() -> Result<()> {
    let env_filter = tracing_subscriber::EnvFilter::from_env("SIMGIT_LOG")
        .add_directive("simgitd=info".parse()?);

    if let Ok(endpoint) = std::env::var("SIMGIT_OTLP_ENDPOINT") {
        let exporter = opentelemetry_otlp::SpanExporter::builder()
            .with_tonic()
            .with_endpoint(endpoint)
            .build()?;

        let provider = opentelemetry_sdk::trace::TracerProvider::builder()
            .with_resource(opentelemetry_sdk::Resource::new(vec![
                KeyValue::new("service.name", "simgitd"),
            ]))
            .with_batch_exporter(exporter, opentelemetry_sdk::runtime::Tokio)
            .build();

        let tracer = provider.tracer("simgitd");
        global::set_tracer_provider(provider);

        tracing_subscriber::registry()
            .with(env_filter)
            .with(tracing_subscriber::fmt::layer())
            .with(tracing_opentelemetry::layer().with_tracer(tracer))
            .init();
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(env_filter)
            .init();
    }

    Ok(())
}
