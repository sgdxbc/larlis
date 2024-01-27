use std::{
    iter::repeat_with,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};

use augustus::{
    app::Null,
    crypto::Crypto,
    event::{OnEvent, Session, SessionSender},
    net::Udp,
    pbft,
    replication::{Concurrent, ConcurrentEvent, ReplicaNet},
    unreplicated,
    worker::{spawn_backend, SpawnExecutor},
};
use axum::{
    extract::State,
    routing::{get, post},
    Json, Router,
};
use replication_control_messages::{BenchmarkResult, ClientConfig, Protocol, ReplicaConfig};
use tokio::{
    runtime,
    signal::ctrl_c,
    spawn,
    task::{spawn_blocking, JoinHandle, JoinSet},
};
use tokio_util::sync::CancellationToken;

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let app = Router::new()
        .route("/ok", get(ok))
        .route("/start-client", post(start_client))
        .route("/benchmark-result", get(benchmark_result))
        .route("/start-replica", post(start_replica))
        .route("/stop-replica", post(stop_replica))
        .with_state(AppState {
            session: Default::default(),
            benchmark_result: Default::default(),
        });
    let ip = std::env::args().nth(1);
    let ip = ip.as_deref().unwrap_or("0.0.0.0");
    let listener = tokio::net::TcpListener::bind(format!("{ip}:3000")).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(async { ctrl_c().await.unwrap() })
        .await?;
    Ok(())
}

#[derive(Debug, Clone)]
struct AppState {
    session: Arc<Mutex<Option<AppSession>>>,
    benchmark_result: Arc<Mutex<Option<BenchmarkResult>>>,
}

type AppSession = (JoinHandle<anyhow::Result<()>>, CancellationToken);

async fn ok(State(state): State<AppState>) {
    let mut handle = None;
    {
        let mut session = state.session.lock().unwrap();
        if session
            .as_ref()
            .map(|(handle, _)| handle.is_finished())
            .unwrap_or(false)
        {
            handle = Some(session.take().unwrap().0)
        }
    }
    if let Some(handle) = handle {
        handle.await.unwrap().unwrap()
    }
}

async fn start_client(State(state): State<AppState>, Json(config): Json<ClientConfig>) {
    let mut session = state.session.lock().unwrap();
    let cancel = CancellationToken::new();
    let benchmark_result = state.benchmark_result.clone();
    benchmark_result.lock().unwrap().take();
    let handle = spawn_blocking(move || {
        let runtime = &runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()?;
        let num_replica = config.num_replica;
        let num_faulty = config.num_faulty;
        match config.protocol {
            Protocol::Unreplicated => {
                let new_state = |client_id, addr, net, upcall| {
                    unreplicated::Client::new(
                        client_id,
                        addr,
                        unreplicated::ToReplicaMessageNet::new(net),
                        upcall,
                    )
                };
                runtime.block_on(client_session(
                    config,
                    new_state,
                    unreplicated::to_client_on_buf,
                    benchmark_result,
                ))
            }
            Protocol::Pbft => {
                let new_state = |client_id, addr, net, upcall| {
                    pbft::Client::new(
                        client_id,
                        addr,
                        pbft::ToReplicaMessageNet::new(net),
                        upcall,
                        num_replica,
                        num_faulty,
                    )
                };
                runtime.block_on(client_session(
                    config,
                    new_state,
                    pbft::to_client_on_buf,
                    benchmark_result,
                ))
            }
        }
    });
    let replaced = session.replace((handle, cancel));
    assert!(replaced.is_none())
}

async fn client_session<S: OnEvent<M> + Send + 'static, M: Send + 'static>(
    config: ClientConfig,
    mut new_state: impl FnMut(
        u32,
        SocketAddr,
        ReplicaNet<Udp, SocketAddr>,
        SessionSender<ConcurrentEvent>,
    ) -> S,
    on_buf: impl Fn(&[u8], &mut SessionSender<M>) -> anyhow::Result<()> + Clone + Send + Sync + 'static,
    benchmark_result: Arc<Mutex<Option<BenchmarkResult>>>,
) -> anyhow::Result<()>
where
    Vec<u8>: Into<M>,
{
    let mut concurrent = Concurrent::new();
    // let cancel = CancellationToken::new();
    // concurrent.insert_max_count(std::num::NonZeroUsize::new(1).unwrap(), {
    //     let cancel = cancel.clone();
    //     Box::new(move || Ok(cancel.cancel()))
    // });
    let mut concurrent_session = Session::new();
    let mut sessions = JoinSet::new();
    for client_id in repeat_with(rand::random).take(1) {
        let socket = tokio::net::UdpSocket::bind("0.0.0.0:0").await?;
        let addr = socket.local_addr()?;
        let net = Udp(socket.into());
        let mut state = new_state(
            client_id,
            addr,
            ReplicaNet::new(net.clone(), config.replica_addrs.clone(), None),
            concurrent_session.sender(),
        );
        let mut session = Session::new();
        concurrent.insert_client_sender(client_id, session.sender())?;
        let mut sender = session.sender();
        let on_buf = on_buf.clone();
        sessions.spawn(async move { net.recv_session(|buf| on_buf(buf, &mut sender)).await });
        sessions.spawn(async move { session.run(&mut state).await });
    }
    concurrent.launch()?;
    // TODO escape with an error indicating the root problem instead of a disconnected channel error
    // caused by the problem
    // is it (easily) possible?
    'select: {
        tokio::select! {
            result = concurrent_session.run(&mut concurrent) => result?,
            result = sessions.join_next() => result.unwrap()??,
            () = tokio::time::sleep(Duration::from_secs(1)) => break 'select,
            // () = cancel.cancelled() => break 'select,
        }
        return Err(anyhow::anyhow!("unexpected shutdown"));
    }
    let throughput = concurrent.latencies.len() as f32;
    let latency =
        concurrent.latencies.into_iter().sum::<Duration>() / (throughput.floor() as u32 + 1);
    sessions.shutdown().await;
    benchmark_result.lock().unwrap().replace(BenchmarkResult {
        throughput,
        latency,
    });
    Ok(())
}

async fn benchmark_result(State(state): State<AppState>) -> Json<Option<BenchmarkResult>> {
    state.benchmark_result.lock().unwrap().clone().into()
}

async fn start_replica(State(state): State<AppState>, Json(config): Json<ReplicaConfig>) {
    let mut session = state.session.lock().unwrap();
    let cancel = CancellationToken::new();
    let session_cancel = cancel.clone();
    let handle = spawn_blocking(move || {
        let runtime = runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let socket = runtime.block_on(tokio::net::UdpSocket::bind(
            config.replica_addrs[config.replica_id as usize],
        ))?;
        println!(
            "Replica {} bind to address {:?}",
            config.replica_id,
            socket.local_addr()
        );
        let net = Udp(socket.into());
        let crypto = Crypto::new_hardcoded_replication(config.num_replica, config.replica_id)?;
        match config.protocol {
            Protocol::Unreplicated => {
                assert_eq!(config.replica_id, 0);
                let state = unreplicated::Replica::new(
                    Null,
                    unreplicated::ToClientMessageNet::new(net.clone()),
                );
                let (_crypto_worker, crypto_executor) = spawn_backend(crypto);
                runtime.block_on(replica_session(
                    state,
                    unreplicated::to_replica_on_buf,
                    net,
                    crypto_executor,
                    session_cancel,
                ))
            }
            Protocol::Pbft => {
                let (crypto_worker, crypto_executor) = spawn_backend(crypto);
                let state = pbft::Replica::<_, _, _, SocketAddr>::new(
                    config.replica_id,
                    Null,
                    pbft::ToReplicaMessageNet::new(ReplicaNet::new(
                        net.clone(),
                        config.replica_addrs,
                        config.replica_id,
                    )),
                    pbft::ToClientMessageNet::new(net.clone()),
                    crypto_worker,
                    config.num_replica,
                    config.num_faulty,
                );
                runtime.block_on(replica_session(
                    state,
                    pbft::to_replica_on_buf,
                    net,
                    crypto_executor,
                    session_cancel,
                ))
            }
        }
    });
    let replaced = session.replace((handle, cancel));
    assert!(replaced.is_none())
}

async fn replica_session<M: Send + 'static>(
    mut state: impl OnEvent<M> + Send + 'static,
    on_buf: impl Fn(&[u8], &mut SessionSender<M>) -> anyhow::Result<()> + Send + Sync + 'static,
    net: Udp,
    mut crypto_executor: SpawnExecutor<Crypto<u8>, M>,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
    let mut session = Session::new();
    let mut recv_session = spawn({
        let mut sender = session.sender();
        async move { net.recv_session(|buf| on_buf(buf, &mut sender)).await }
    });
    let mut crypto_session = spawn({
        let sender = session.sender();
        async move { crypto_executor.run(sender).await }
    });
    let mut state_session = spawn(async move { session.run(&mut state).await });
    'select: {
        tokio::select! {
            result = &mut recv_session => result??,
            result = &mut crypto_session => result??,
            result = &mut state_session => result??,
            () = cancel.cancelled() => break 'select,
        }
        return Err(anyhow::anyhow!("unexpected shutdown"));
    }
    recv_session.abort();
    crypto_session.abort();
    state_session.abort();
    let _ = recv_session.await;
    let _ = crypto_session.await;
    let _ = state_session.await;
    Ok(())
}

async fn stop_replica(State(state): State<AppState>) {
    let (handle, cancel) = {
        let mut session = state.session.lock().unwrap();
        session.take().unwrap()
    };
    cancel.cancel();
    handle.await.unwrap().unwrap()
}
