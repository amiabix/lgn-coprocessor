#![feature(generic_const_exprs)]
use std::collections::BTreeMap;
use std::fmt::Debug;
use std::panic;
use std::result::Result::Ok;
use std::str::FromStr;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use anyhow::bail;
use anyhow::Context;
use backtrace::Backtrace;
use checksum::fetch_checksums;
use clap::Parser;
use ethers::signers::Wallet;
use jwt::Claims;
use jwt::RegisteredClaims;
use k256::ecdsa::SigningKey;
use lagrange::worker_done::Reply;
use lagrange::WorkerDone;
use lagrange::WorkerToGwRequest;
use lagrange::WorkerToGwResponse;
use lgn_auth::jwt::JWTAuth;
use lgn_messages::Message;
use lgn_messages::Response;
use lgn_messages::ToMessageClass;
use lgn_worker::avs::utils::read_keystore;
use metrics::counter;
use metrics::histogram;
use mimalloc::MiMalloc;
use tokio_stream::StreamExt;
use tonic::metadata::MetadataValue;
use tonic::transport::ClientTlsConfig;
use tonic::Request;
use tracing::debug;
use tracing::error;
use tracing::info;
use tracing::level_filters::LevelFilter;
use tracing::span;
use tracing::trace;
use tracing::Level;
use tracing_subscriber::fmt::format::FmtSpan;
use tracing_subscriber::EnvFilter;
use warp::Filter;

use crate::config::Config;
use crate::manager::ProversManager;

pub mod lagrange {
    tonic::include_proto!("lagrange");
}

mod checksum;
mod config;
mod manager;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

const MAX_GRPC_MESSAGE_SIZE_MB: usize = 16;

#[derive(Parser, Clone, Debug)]
struct Cli {
    /// Path to the configuration file.
    #[clap(short, long)]
    config: Option<String>,

    /// If set, output logs in JSON format.
    #[clap(short, long, action)]
    json: bool,
}

fn setup_logging(json: bool) {
    if json {
        let subscriber = tracing_subscriber::fmt()
            .json()
            .with_level(true)
            .with_file(true)
            .with_line_number(true)
            .with_target(true)
            .with_env_filter(
                EnvFilter::builder()
                    .with_default_directive(LevelFilter::INFO.into())
                    .from_env_lossy(),
            )
            .with_span_events(FmtSpan::NEW | FmtSpan::CLOSE)
            .finish();
        tracing::subscriber::set_global_default(subscriber).expect("Setting up logging failed");
    } else {
        let subscriber = tracing_subscriber::fmt()
            .pretty()
            .compact()
            .with_level(true)
            .with_file(true)
            .with_line_number(true)
            .with_target(true)
            .with_env_filter(
                EnvFilter::builder()
                    .with_default_directive(LevelFilter::INFO.into())
                    .from_env_lossy(),
            )
            .with_span_events(FmtSpan::NEW | FmtSpan::CLOSE)
            .finish();
        tracing::subscriber::set_global_default(subscriber).expect("Setting up logging failed");
    };
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    setup_logging(cli.json);

    let mp2_version = semver::Version::parse(verifiable_db::version())?;
    let mp2_requirement = semver::VersionReq::parse(&format!("^{mp2_version}"))?;

    info!("Running MR2 version {mp2_version} - requiring {mp2_requirement}");

    panic::set_hook(Box::new(|panic_info| {
        let msg = match panic_info.payload().downcast_ref::<&'static str>() {
            Some(s) => *s,
            None => {
                match panic_info.payload().downcast_ref::<String>() {
                    Some(s) => &s[..],
                    None => "Box<dyn Any>",
                }
            },
        };
        let (file, lineno, col) = match panic_info.location() {
            Some(l) => (l.file(), l.line(), l.column()),
            None => ("<unknown>", 0, 0),
        };

        error!(
            msg,
            file,
            lineno,
            col,
            "Panic occurred: {:?}",
            Backtrace::new(),
        );
    }));

    if let Err(err) = run(cli, mp2_requirement).await {
        panic!("Worker exited due to an error: {err:?}")
    } else {
        Ok(())
    }
}

async fn run(
    cli: Cli,
    mp2_requirement: semver::VersionReq,
) -> anyhow::Result<()> {
    let version = env!("CARGO_PKG_VERSION");
    info!("Starting worker. version: {}", version);
    let config = Config::load(cli.config);
    config.validate();
    debug!("Loaded configuration: {:?}", config);

    let span = span!(
        Level::INFO,
        "Starting node",
        "worker" = config.avs.worker_id.to_string(),
        "issuer" = config.avs.issuer.to_string(),
        "version" = version,
        "class" = config.worker.instance_type.to_string(),
    );
    let _guard = span.enter();

    metrics_exporter_prometheus::PrometheusBuilder::new()
        .with_http_listener(([0, 0, 0, 0], config.prometheus.port))
        .install()
        .context("setting up Prometheus")?;

    run_worker(&config, mp2_requirement).await
}

async fn run_worker(
    config: &Config,
    mp2_requirement: semver::VersionReq,
) -> anyhow::Result<()> {
    let max_message_size = config
        .avs
        .max_grpc_message_size_mb
        .unwrap_or(MAX_GRPC_MESSAGE_SIZE_MB)
        * 1024
        * 1024;

    // Preparing the prover
    let checksums = fetch_checksums(config.public_params.checksum_file_url())
        .await
        .context("downloading checksum file")?;
    let provers_manager = tokio::task::block_in_place(move || {
        ProversManager::new(config, &checksums, mp2_requirement)
    })
    .context("creating prover managers")?;

    // Connecting to the GW
    let wallet = get_wallet(config).context("fetching wallet")?;
    let claims = get_claims(config).context("building claims")?;
    let token = JWTAuth::new(claims, &wallet)?.encode()?;

    let grpc_url = &config.avs.gateway_url;
    info!(
        "Connecting to the gateway. url: {} max_messsage_size = {}MB",
        grpc_url,
        max_message_size / (1024 * 1024)
    );

    let uri = grpc_url
        .parse::<tonic::transport::Uri>()
        .context("parsing gateway URL")?;

    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    let channel = tonic::transport::Channel::builder(uri.clone())
        .tls_config(ClientTlsConfig::new().with_enabled_roots())?
        .connect()
        .await
        .with_context(|| format!("creating transport channel builder for {uri}"))?;
    let token: MetadataValue<_> = format!("Bearer {token}").parse()?;
    let mut client = lagrange::workers_service_client::WorkersServiceClient::with_interceptor(
        channel,
        move |mut req: Request<()>| {
            req.metadata_mut().insert("authorization", token.clone());
            Ok(req)
        },
    )
    .max_encoding_message_size(max_message_size)
    .max_decoding_message_size(max_message_size);

    let (outbound, outbound_rx) = tokio::sync::mpsc::channel(50);
    let outbound_rx = tokio_stream::wrappers::ReceiverStream::new(outbound_rx);
    outbound
        .send(WorkerToGwRequest {
            request: Some(lagrange::worker_to_gw_request::Request::WorkerReady(
                lagrange::WorkerReady {
                    version: env!("CARGO_PKG_VERSION").to_string(),
                    worker_class: format!(
                        "{}-{}",
                        config.worker.instance_type,
                        semver::Version::parse(verifiable_db::version())
                            .unwrap()
                            .major
                    ),
                },
            )),
        })
        .await?;

    let response = client
        .worker_to_gw(tonic::Request::new(outbound_rx))
        .await
        .context("connecting worker_to_gw")?;

    info!("Bidirectional stream with GW opened");
    let mut inbound = response.into_inner();

    let liveness_check_interval = config.worker.liveness_check_interval;
    let last_task_processed = Arc::new(AtomicU64::new(
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
    ));
    let last_task_processed_clone = Arc::clone(&last_task_processed);

    // Start readiness and liveness check server
    tokio::spawn(async move {
        let readiness_route = warp::path!("readiness")
            .map(|| warp::reply::with_status("OK", warp::http::StatusCode::OK));
        let liveness_route = warp::path!("liveness").map(move || {
            let last_processed = last_task_processed_clone.load(Ordering::Relaxed);
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs();
            if now - last_processed <= liveness_check_interval {
                warp::reply::with_status("OK", warp::http::StatusCode::OK)
            } else {
                warp::reply::with_status("FAIL", warp::http::StatusCode::INTERNAL_SERVER_ERROR)
            }
        });
        let routes = readiness_route.or(liveness_route);
        warp::serve(routes).run(([0, 0, 0, 0], 8080)).await;
    });

    loop {
        debug!("Waiting for message...");

        match inbound.next().await {
            Some(Ok(msg)) => {
                counter!("zkmr_worker_messages_total").increment(1);

                let task_id = msg.task_id.clone();

                let uuid = parse_uuid(&msg);
                let result = tokio::task::block_in_place(|| {
                    process_downstream_payload(&provers_manager, msg, uuid)
                });

                match result {
                    Ok(reply_envelope) => {
                        let response = WorkerToGwRequest {
                            request: Some(lagrange::worker_to_gw_request::Request::WorkerDone(
                                WorkerDone {
                                    task_id,
                                    reply: Some(Reply::TaskOutput(serde_json::to_vec(
                                        &reply_envelope,
                                    )?)),
                                },
                            )),
                        };
                        outbound.send(response).await?;
                        counter!("zkmr_worker_messages_successful_total").increment(1);
                        last_task_processed.store(
                            SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
                            Ordering::Relaxed,
                        );
                    },
                    Err(err) => {
                        let response = WorkerToGwRequest {
                            request: Some(lagrange::worker_to_gw_request::Request::WorkerDone(
                                WorkerDone {
                                    task_id,
                                    reply: Some(Reply::WorkerError(format!("{:?}", err))),
                                },
                            )),
                        };
                        outbound.send(response).await?;
                        counter!("zkmr_worker_messages_error_total").increment(1);
                        error!("failed to process task. uuid: {:?} err: {:?}", uuid, err);
                        bail!("task processing failed. uuid: {:?} err: {:?}", uuid, err);
                    },
                }
            },
            Some(Err(status)) => {
                counter!("zkmr_worker_error_total").increment(1);
                bail!("connection to the gateway ended. status: {}", status);
            },
            None => bail!("inbound connection broken"),
        }
    }
}

/// Parses the uuid in the GW message.
fn parse_uuid(message: &WorkerToGwResponse) -> uuid::Uuid {
    message.task_id.as_ref().map_or(uuid::Uuid::nil(), |id| {
        uuid::Uuid::from_bytes_le(id.id.clone().try_into().unwrap())
    })
}

#[tracing::instrument(skip(provers_manager, message), err(Debug))]
fn process_downstream_payload(
    provers_manager: &ProversManager,
    message: WorkerToGwResponse,
    uuid: uuid::Uuid,
) -> anyhow::Result<Response> {
    let envelope = serde_json::from_slice::<Message>(&message.task).with_context(|| {
        format!(
            "Failed to deserialize envelope. uuid: {} message_len: {}",
            uuid,
            message.task.len(),
        )
    })?;

    let message_class = envelope.message_class();

    counter!(
        "zkmr_worker_tasks_received_total",
        "message_class" => message_class,
    )
    .increment(1);

    let span = span!(
        Level::INFO,
        "task_uuid",
        uuid = uuid.to_string(),
        task_id = envelope.task_id(),
        message_class,
    );
    let _guard = span.enter();
    info!(
        "Received Task. uuid: {} task_id: {:?} message_class: {}",
        uuid,
        envelope.task_id(),
        message_class,
    );

    let task_id = envelope.task_id().map(|s| s.to_owned());
    let start_time = std::time::Instant::now();

    match std::panic::catch_unwind(|| provers_manager.delegate_proving(envelope)) {
        Ok(result) => {
            match result {
                Ok(reply) => {
                    counter!(
                        "zkmr_worker_tasks_successful_total",
                        "message_class" => message_class,
                    )
                    .increment(1);
                    histogram!(
                        "zkmr_worker_task_successful_processing_duration_seconds",
                        "message_class" => message_class,
                    )
                    .record(start_time.elapsed().as_secs_f64());

                    trace!(
                        "Sending reply. uuid: {} task_id: {:?} reply: {:?}",
                        uuid,
                        task_id,
                        reply
                    );
                    Ok(reply)
                },
                Err(err) => {
                    counter!(
                        "zkmr_worker_tasks_failed_total",
                        "message_class" => message_class,
                    )
                    .increment(1);
                    histogram!(
                        "zkmr_worker_task_failure_processing_duration_seconds",
                        "message_class" => message_class,
                    )
                    .record(start_time.elapsed().as_secs_f64());

                    error!(
                        "Error processing task. uuid: {} task_id: {:?} err: {:?}",
                        uuid, task_id, err
                    );
                    return Err(err);
                },
            }
        },
        Err(panic) => {
            counter!(
                "zkmr_worker_tasks_failed_total",
                "message_class" => message_class,
            )
            .increment(1);
            histogram!(
                "zkmr_worker_task_failure_processing_duration_seconds",
                "message_class" => message_class,
            )
            .record(start_time.elapsed().as_secs_f64());

            let msg = match panic.downcast_ref::<&'static str>() {
                Some(s) => *s,
                None => {
                    match panic.downcast_ref::<String>() {
                        Some(s) => &s[..],
                        None => "Box<dyn Any>",
                    }
                },
            };

            error!(
                "panic encountered while proving. uuid: {} task_id: {:?} msg: {}",
                uuid, task_id, msg,
            );
            bail!(
                "panic encountered while proving. uuid: {} task_id: {:?} msg: {}",
                uuid,
                task_id,
                msg,
            )
        },
    }
}

fn get_wallet(config: &Config) -> anyhow::Result<Wallet<SigningKey>> {
    let res = match (
        &config.avs.lagr_keystore,
        &config.avs.lagr_pwd,
        &config.avs.lagr_private_key,
    ) {
        (Some(keystore_path), Some(password), None) => {
            read_keystore(keystore_path, password.expose_secret())?
        },
        (Some(_), None, Some(pkey)) => {
            Wallet::from_str(pkey.expose_secret()).context("Failed to create wallet")?
        },
        _ => bail!("Must specify either keystore path w/ password OR private key"),
    };

    Ok(res)
}

fn get_claims(config: &Config) -> anyhow::Result<Claims> {
    let registered = RegisteredClaims {
        issuer: Some(config.avs.issuer.clone()),
        subject: Some(config.avs.worker_id.clone()),
        issued_at: Some(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("Epoch can not be in the future")
                .as_secs(),
        ),
        ..Default::default()
    };

    let private = [(
        "worker_class".to_string(),
        serde_json::Value::String(config.worker.instance_type.to_string()),
    )]
    .into_iter()
    .collect::<BTreeMap<String, serde_json::Value>>();

    Ok(Claims {
        registered,
        private,
    })
}
