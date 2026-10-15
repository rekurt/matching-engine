use me_core::symbol::SymbolConfig;
use me_engine::financial::FinancialEngine;
use me_financial::{transport::*, FinancialCommand};
use serde::Deserialize;
use std::{
    collections::BTreeSet,
    io,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{net::TcpListener, sync::Semaphore};
use tokio_rustls::TlsAcceptor;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    listen: String,
    data_directory: String,
    tls: TlsFiles,
    gateway_fingerprints: BTreeSet<String>,
    symbols: Vec<SymbolConfig>,
    #[serde(default = "minimum_disk")]
    minimum_free_bytes: u64,
}
fn minimum_disk() -> u64 {
    256 * 1024 * 1024
}
fn now() -> io::Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(error)?
        .as_nanos()
        .try_into()
        .map_err(error)
}
pub async fn run(path: &str) -> io::Result<()> {
    let cfg: Config = serde_json::from_slice(&std::fs::read(path)?).map_err(error)?;
    if cfg.gateway_fingerprints.is_empty() {
        return Err(error("financial gateway allowlist is empty"));
    }
    for fingerprint in &cfg.gateway_fingerprints {
        validate_fingerprint(fingerprint)?;
    }
    let tls = TlsAcceptor::from(Arc::new(cfg.tls.server()?));
    let engine = Arc::new(Mutex::new(FinancialEngine::open(
        &cfg.data_directory,
        cfg.symbols,
    )?));
    let listener = TcpListener::bind(&cfg.listen).await?;
    tracing::info!(listen=%cfg.listen,"Financial engine ready");
    let slots = Arc::new(Semaphore::new(16));
    let active_gateway = Arc::new(Semaphore::new(1));
    let mut connections = tokio::task::JoinSet::new();
    let mut ticks = tokio::time::interval(Duration::from_millis(50));
    let mut shutdown = std::pin::pin!(tokio::signal::ctrl_c());
    loop {
        tokio::select! {
            _=&mut shutdown=>break,
            _=ticks.tick()=> {
                let engine=engine.clone();
                tokio::task::spawn_blocking(move ||engine.lock().map_err(error)?.tick(now()?)).await.map_err(error)??;
            },
            accepted=listener.accept()=> {
                let (stream,_)=accepted?;
                let Ok(permit)=slots.clone().try_acquire_owned() else {continue;};
                let active_gateway=active_gateway.clone();
                let tls=tls.clone();let allowed=cfg.gateway_fingerprints.clone();let engine=engine.clone();
                let directory=cfg.data_directory.clone();let minimum=cfg.minimum_free_bytes;
                connections.spawn(async move {
                    let _permit=permit;
                    let result:io::Result<()>=async {
                        let mut stream=tokio::time::timeout(Duration::from_secs(5),tls.accept(stream)).await.map_err(error)??;
                        let principal=peer_identity(stream.get_ref().1.peer_certificates())?;
                        if !allowed.contains(&principal) {return Err(error("unauthorized financial gateway certificate"));}
                        let _active=active_gateway.try_acquire_owned().map_err(|_|error("a financial gateway is already connected"))?;
                        let hello=tokio::time::timeout(Duration::from_secs(5),Frame::read(&mut stream)).await.map_err(error)??;
                        accept_hello(&hello)?.write(&mut stream).await?;
                        loop {
                            let frame=tokio::time::timeout(Duration::from_secs(30),Frame::read(&mut stream)).await.map_err(error)??;
                            if frame.request_id==0 || frame.payload.len()>16*1024 {return Err(error("invalid financial request identity/size"));}
                            let engine=engine.clone();let directory=directory.clone();let request_id=frame.request_id;
                            let result=tokio::task::spawn_blocking(move || {
                                let mut engine=engine.lock().map_err(error)?;
                                engine.healthy()?;
                                match frame.message_type {
                                    FINANCIAL_IDENTITY=> {
                                        let mut identity=engine.identity();
                                        identity.storage=Some(me_financial::StorageStatus {
                                            available_bytes:fs2::available_space(&directory)?,minimum_free_bytes:minimum,
                                            journal_bytes:std::fs::metadata(std::path::Path::new(&directory).join("active/financial.wal"))?.len(),
                                        });
                                        Ok(serde_json::to_value(identity).map_err(error)?)
                                    },
                                    FINANCIAL_COMMAND=> {
                                        if fs2::available_space(&directory)?<minimum {return Err(io::Error::new(io::ErrorKind::WouldBlock,"financial disk reserve reached"));}
                                        let command:FinancialCommand=serde_json::from_slice(&frame.payload).map_err(error)?;
                                        serde_json::to_value(engine.submit(command,now()?)?).map_err(error)
                                    },
                                    FINANCIAL_DECISIONS=> {
                                        #[derive(Deserialize)] #[serde(deny_unknown_fields)] struct Read {after:u64,digest:String,limit:usize}
                                        let request:Read=serde_json::from_slice(&frame.payload).map_err(error)?;
                                        serde_json::to_value(engine.decisions(request.after,&request.digest,request.limit)?).map_err(error)
                                    },
                                    FINANCIAL_MAINTENANCE=> {
                                        #[derive(Deserialize)] #[serde(deny_unknown_fields)] struct Maintenance {enabled:bool}
                                        let request:Maintenance=serde_json::from_slice(&frame.payload).map_err(error)?;
                                        serde_json::to_value(engine.maintenance(request.enabled,now()?)?).map_err(error)
                                    },
                                    _=>Err(error("financial mode rejects legacy trading commands")),
                                }
                            }).await.map_err(error)?;
                            let reply=match result {Ok(result)=>Reply {result:Some(result),error:None,retryable:false},Err(e)=>Reply {result:None,error:Some(e.to_string()),retryable:e.kind()==io::ErrorKind::WouldBlock}};
                            Frame::json(FINANCIAL_REPLY,request_id,&reply)?.write(&mut stream).await?;
                        }
                    }.await;
                    if let Err(e)=result {tracing::debug!(error=%e,"Financial connection closed");}
                });
            },
            Some(result)=connections.join_next()=>{result.map_err(error)?;},
        }
    }
    // The durable barrier stops GTD as well as incoming trading mutations.
    engine.lock().map_err(error)?.maintenance(true, now()?)?;
    connections.abort_all();
    while connections.join_next().await.is_some() {}
    Ok(())
}
