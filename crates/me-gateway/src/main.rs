mod api;
mod client;
mod config;
mod worker;
use clap::Parser;
use config::Config;
use me_financial::{
    fingerprint, transport::*, EngineIdentity, REQUIRED_CAPABILITIES, STORAGE_VERSION,
};
use me_ledger::Ledger;
use std::{
    io,
    sync::{
        atomic::{AtomicBool, AtomicU64},
        Arc,
    },
    time::Duration,
};
use tokio::sync::{Mutex, Semaphore};
use worker::State;
#[derive(Parser)]
struct Args {
    #[arg(long)]
    config: String,
}
#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    if let Err(e) = run(Args::parse()).await {
        tracing::error!(error=%e,"Gateway stopped");
        std::process::exit(1);
    }
}
async fn run(args: Args) -> io::Result<()> {
    let mut cfg: Config = serde_json::from_slice(&std::fs::read(args.config)?).map_err(error)?;
    if cfg.principals.is_empty() {
        return Err(error("gateway principals are empty"));
    }
    for identity in cfg.principals.keys() {
        validate_fingerprint(identity)?;
    }
    validate_fingerprint(&cfg.matcher.certificate_fingerprint)?;
    cfg.symbols.sort_by_key(|s| s.id.0);
    if cfg.symbols.is_empty()
        || cfg.symbols.windows(2).any(|s| s[0].id == s[1].id)
        || cfg.fees.len() != cfg.symbols.len()
    {
        return Err(error("incomplete/duplicate gateway instruments"));
    }
    let tls = tokio_rustls::TlsAcceptor::from(Arc::new(cfg.tls.server()?));
    let ledger = Ledger::connect(
        &std::env::var(&cfg.database_url_env)
            .map_err(|_| error("ledger database environment variable is missing"))?,
    )
    .await
    .map_err(error)?;
    ledger.migrate().await.map_err(error)?;
    let lease = ledger.acquire_gateway().await.map_err(error)?;
    let mut client = client::Client::new(cfg.matcher);
    let identity: EngineIdentity = client.query(FINANCIAL_IDENTITY, &()).await?;
    let hash = fingerprint(&cfg.symbols).map_err(error)?;
    if identity.definition_hash != hash
        || identity.storage_version != STORAGE_VERSION
        || identity.watermarks.len() != 1
        || identity.watermarks[0].shard != 0
        || REQUIRED_CAPABILITIES
            .iter()
            .any(|c| !identity.capabilities.iter().any(|v| v == c))
    {
        return Err(error("incompatible matcher capabilities/instruments"));
    }
    if !ledger.has_engine().await.map_err(error)? && identity.watermarks[0].sequence != 0 {
        return Err(error("new ledger requires an empty financial matcher"));
    }
    let mut assets = cfg.assets;
    for symbol in &cfg.symbols {
        assets.entry(symbol.base.clone()).or_insert(None);
        assets.entry(symbol.quote.clone()).or_insert(None);
    }
    for (asset, precision) in assets {
        ledger
            .add_asset(&asset, precision.unwrap_or(8))
            .await
            .map_err(error)?;
    }
    for symbol in cfg.symbols {
        let fees = cfg
            .fees
            .get(&symbol.id.0)
            .ok_or_else(|| error("missing fee schedule"))?;
        ledger
            .add_market(symbol.id.0, &symbol.base, &symbol.quote, fees, &hash)
            .await
            .map_err(error)?;
    }
    ledger
        .bind_engine(identity.engine_id, &hash)
        .await
        .map_err(error)?;
    ledger.audit().await.map_err(error)?;
    let state = State {
        ledger,
        lease: Arc::new(Mutex::new(lease)),
        client: Arc::new(Mutex::new(client)),
        ready: Arc::new(AtomicBool::new(false)),
        lag: Arc::new(AtomicU64::new(0)),
        disk_available: Arc::new(AtomicU64::new(0)),
        disk_minimum: Arc::new(AtomicU64::new(0)),
        journal_bytes: Arc::new(AtomicU64::new(0)),
        errors: Arc::new(AtomicU64::new(0)),
        engine: identity,
        maintenance_lock: Arc::new(Mutex::new(())),
    };
    let mut worker = tokio::spawn(state.clone().run());
    let listener = tokio::net::TcpListener::bind(&cfg.listen).await?;
    let router = api::router(state.clone());
    let principals = Arc::new(cfg.principals);
    let slots = Arc::new(Semaphore::new(128));
    let mut connections = tokio::task::JoinSet::new();
    let mut shutdown = std::pin::pin!(tokio::signal::ctrl_c());
    tracing::info!(listen=%cfg.listen,"Internal financial gateway listening");
    loop {
        tokio::select! {
            _=&mut shutdown=>break,
            result=&mut worker=>return Err(error(format!("gateway worker stopped: {result:?}"))),
            accepted=listener.accept()=> {
                let (stream,_)=accepted?;let Ok(permit)=slots.clone().try_acquire_owned() else {continue;};
                let tls=tls.clone();let principals=principals.clone();let router=router.clone();
                connections.spawn(async move {
                    let _permit=permit;
                    let result:io::Result<()>=async {
                        let stream=tokio::time::timeout(Duration::from_secs(5),tls.accept(stream)).await.map_err(error)??;
                        let identity=peer_identity(stream.get_ref().1.peer_certificates())?;
                        let principal=principals.get(&identity).ok_or_else(||error("unauthorized gateway caller certificate"))?.clone();
                        let router=router.layer(axum::Extension(api::Caller {identity,principal}));
                        hyper::server::conn::http1::Builder::new().serve_connection(hyper_util::rt::TokioIo::new(stream),hyper_util::service::TowerToHyperService::new(router)).await.map_err(error)
                    }.await;
                    if let Err(e)=result {tracing::debug!(error=%e,"Gateway connection closed");}
                });
            },
            Some(result)=connections.join_next()=>{result.map_err(error)?;},
        }
    }
    state.maintenance(true).await?;
    connections.abort_all();
    while connections.join_next().await.is_some() {}
    worker.abort();
    let _ = worker.await;
    Ok(())
}
