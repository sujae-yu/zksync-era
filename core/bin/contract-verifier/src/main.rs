use std::{cell::RefCell, time::Duration};

use anyhow::Context;
use futures::{channel::mpsc, executor::block_on, SinkExt, StreamExt};
use structopt::StructOpt;
use tokio::sync::watch;
use zksync_config::configs::PrometheusConfig;
use zksync_contract_verifier_lib::ContractVerifier;
use zksync_core_leftovers::temp_config_store::{load_database_secrets, load_general_config};
use zksync_dal::{ConnectionPool, Core};
use zksync_queued_job_processor::JobProcessor;
use zksync_utils::wait_for_tasks::ManagedTasks;
use zksync_vlog::prometheus::PrometheusExporterConfig;

#[derive(StructOpt)]
#[structopt(name = "ZKsync contract code verifier", author = "Matter Labs")]
struct Opt {
    /// Number of jobs to process. If None, runs indefinitely.
    #[structopt(long)]
    jobs_number: Option<usize>,
    /// Path to the configuration file.
    #[structopt(long)]
    config_path: Option<std::path::PathBuf>,
    /// Path to the secrets file.
    #[structopt(long)]
    secrets_path: Option<std::path::PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let opt = Opt::from_args();

    let general_config = load_general_config(opt.config_path).context("general config")?;
    let database_secrets = load_database_secrets(opt.secrets_path).context("database secrets")?;

    let verifier_config = general_config
        .contract_verifier
        .context("ContractVerifierConfig")?;
    let prometheus_config = PrometheusConfig {
        listener_port: verifier_config.prometheus_port,
        ..general_config.api_config.context("ApiConfig")?.prometheus
    };
    let pool = ConnectionPool::<Core>::singleton(
        database_secrets
            .master_url()
            .context("Master DB URL is absent")?,
    )
    .build()
    .await
    .unwrap();

    let observability_config = general_config
        .observability
        .context("ObservabilityConfig")?;

    let _observability_guard = observability_config.install()?;

    let (stop_sender, stop_receiver) = watch::channel(false);
    let (stop_signal_sender, mut stop_signal_receiver) = mpsc::channel(256);
    {
        let stop_signal_sender = RefCell::new(stop_signal_sender.clone());
        ctrlc::set_handler(move || {
            let mut sender = stop_signal_sender.borrow_mut();
            block_on(sender.send(true)).expect("Ctrl+C signal send");
        })
        .expect("Error setting Ctrl+C handler");
    }

    let contract_verifier = ContractVerifier::new(verifier_config.compilation_timeout(), pool)
        .await
        .context("failed initializing contract verifier")?;
    let tasks = vec![
        // TODO PLA-335: Leftovers after the prover DB split.
        // The prover connection pool is not used by the contract verifier, but we need to pass it
        // since `JobProcessor` trait requires it.
        tokio::spawn(contract_verifier.run(stop_receiver.clone(), opt.jobs_number)),
        tokio::spawn(
            PrometheusExporterConfig::pull(prometheus_config.listener_port).run(stop_receiver),
        ),
    ];

    let mut tasks = ManagedTasks::new(tasks);
    tokio::select! {
        () = tasks.wait_single() => {},
        _ = stop_signal_receiver.next() => {
            tracing::info!("Stop signal received, shutting down");
        },
    };
    stop_sender.send_replace(true);

    // Sleep for some time to let verifier gracefully stop.
    tasks.complete(Duration::from_secs(5)).await;
    Ok(())
}
