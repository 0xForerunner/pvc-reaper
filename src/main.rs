use anyhow::{Context, Result};
use clap::Parser;
use kube::Client;
use pvc_reaper::{reconcile, ReaperConfig};
use std::time::Duration;
use tracing::{error, info};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let config = ReaperConfig::parse();

    info!("Starting pvc-reaper");
    info!("Storage class names: {}", config.storage_classes.join(","));
    info!("Storage provisioner: {}", config.storage_provisioner);
    info!("Reconcile interval: {}s", config.reconcile_interval_secs);
    info!("Dry run: {}", config.dry_run);
    info!("Check pending pods: {}", config.check_pending_pods);

    let client = Client::try_default()
        .await
        .context("Failed to create Kubernetes client")?;

    loop {
        if let Err(e) = reconcile(&client, &config).await {
            error!("Reconciliation error: {:#}", e);
        }

        tokio::time::sleep(Duration::from_secs(config.reconcile_interval_secs)).await;
    }
}
