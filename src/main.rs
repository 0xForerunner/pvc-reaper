use anyhow::{Context, Result};
use clap::Parser;
use kube::Client;
use pvc_reaper::{ReaperConfig, reap};
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
    info!("Reap interval: {}s", config.reap_interval_secs);
    info!("Dry run: {}", config.dry_run);
    info!("Cleanup PVCs: {}", config.cleanup_pvcs);
    info!(
        "Check unschedulable pods: {}",
        config.check_unschedulable_pods
    );
    info!("Cleanup PVs: {}", config.cleanup_pvs);
    info!("PV grace period: {}s", config.pv_grace_period_secs);
    info!(
        "Cleanup OpenEBS LVMVolumes: {}",
        config.cleanup_openebs_lvmvolumes
    );
    info!("OpenEBS namespace: {}", config.openebs_namespace);

    let client = Client::try_default()
        .await
        .context("Failed to create Kubernetes client")?;

    loop {
        if let Err(e) = reap(&client, &config).await {
            error!("Reaping error: {:#}", e);
        }

        tokio::time::sleep(Duration::from_secs(config.reap_interval_secs)).await;
    }
}
