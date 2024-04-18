use std::{
    net::{IpAddr, SocketAddr},
    time::Duration,
};

use tokio::{task::JoinSet, time::sleep};

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(1500))
        .build()?;
    let item = std::env::args().nth(1);
    match item.as_deref() {
        Some("mutex") => mutex_session(client).await,
        Some("cops") => cops_session(client).await,
        _ => Ok(()),
    }
}

async fn mutex_session(client: reqwest::Client) -> anyhow::Result<()> {
    let urls = (0..2)
        .map(|i| format!("http://127.0.0.{}:3000", i + 1))
        .collect::<Vec<_>>();
    let addrs = (0..2)
        .map(|i| SocketAddr::from(([127, 0, 0, i + 1], 4000)))
        .collect::<Vec<_>>();
    let client_addrs = (0..2)
        .map(|i| SocketAddr::from(([127, 0, 0, i + 1], 5000)))
        .collect::<Vec<_>>();

    let mut watchdog_sessions = JoinSet::new();
    for (index, url) in urls.iter().enumerate() {
        let config =
            // boson_control_messages::Mutex::Untrusted(boson_control_messages::MutexUntrusted {
            //     addrs: addrs.clone(),
            //     id: index as _,
            // });
            boson_control_messages::Mutex::Replicated(boson_control_messages::MutexReplicated {
                addrs: addrs.clone(),
                client_addrs: client_addrs.clone(),
                id: index as _,
                num_faulty: 0,
            });

        watchdog_sessions.spawn(mutex_start_session(client.clone(), url.clone(), config));
    }
    for _ in 0..10 {
        sleep(Duration::from_millis(1000)).await;
        tokio::select! {
            result = mutex_request_session(client.clone(), urls[0].clone()) => result?,
            Some(result) = watchdog_sessions.join_next() => result??,
        }
    }
    watchdog_sessions.shutdown().await;
    let mut stop_sessions = JoinSet::new();
    for url in urls {
        stop_sessions.spawn(mutex_stop_session(client.clone(), url));
    }
    while let Some(result) = stop_sessions.join_next().await {
        result??
    }
    Ok(())
}

async fn mutex_start_session(
    client: reqwest::Client,
    url: String,
    config: boson_control_messages::Mutex,
) -> anyhow::Result<()> {
    client
        .post(format!("{url}/mutex/start"))
        .json(&config)
        .send()
        .await?
        .error_for_status()?;
    loop {
        sleep(Duration::from_millis(1000)).await;
        client
            .get(format!("{url}/ok"))
            .send()
            .await?
            .error_for_status()?;
    }
}

async fn mutex_stop_session(client: reqwest::Client, url: String) -> anyhow::Result<()> {
    client
        .post(format!("{url}/mutex/stop"))
        .send()
        .await?
        .error_for_status()?;
    Ok(())
}

async fn mutex_request_session(client: reqwest::Client, url: String) -> anyhow::Result<()> {
    let latency = client
        .post(format!("{url}/mutex/request"))
        .send()
        .await?
        .error_for_status()?
        .json::<Duration>()
        .await?;
    println!("{latency:?}");
    Ok(())
}

async fn cops_session(client: reqwest::Client) -> anyhow::Result<()> {
    let urls = (0..2)
        .map(|i| format!("http://127.0.0.{}:3000", i + 1))
        .collect::<Vec<_>>();
    let client_urls = (0..2)
        .map(|i| format!("http://127.0.0.{}:3000", i + 101))
        .collect::<Vec<_>>();
    let addrs = (0..2)
        .map(|i| SocketAddr::from(([127, 0, 0, i + 1], 4000)))
        .collect::<Vec<_>>();
    let client_ips = (0..2)
        .map(|_| IpAddr::from([127, 0, 0, 101]))
        .collect::<Vec<_>>();
    let mut watchdog_sessions = JoinSet::new();
    use boson_control_messages::CopsVariant::*;
    // let variant = Replicated(boson_control_messages::CopsReplicated { num_faulty: 0 });
    let variant = Untrusted;
    println!("Start servers");
    for (i, url) in urls.iter().enumerate() {
        let config = boson_control_messages::CopsServer {
            addrs: addrs.clone(),
            id: i as _,
            record_count: 1000,
            variant: variant.clone(),
        };
        watchdog_sessions.spawn(cops_start_server_session(
            client.clone(),
            url.clone(),
            config,
        ));
    }
    tokio::select! {
        () = sleep(Duration::from_millis(5000)) => {}
        Some(result) = watchdog_sessions.join_next() => result??,
    }
    println!("Start clients");
    let mut client_sessions = JoinSet::new();
    for (i, url) in client_urls.into_iter().enumerate() {
        let config = boson_control_messages::CopsClient {
            addrs: addrs.clone(),
            ip: client_ips[i],
            index: i,
            num_concurrent: 1,
            num_concurrent_put: 1,
            record_count: 1000,
            put_range: 500 * i..500 * (i + 1),
            variant: variant.clone(),
        };
        client_sessions.spawn(cops_client_session(client.clone(), url, config));
    }
    let task = async {
        while let Some(result) = client_sessions.join_next().await {
            result??
        }
        anyhow::Ok(())
    };
    tokio::select! {
        result = task => result?,
        Some(result) = watchdog_sessions.join_next() => result??,
    }
    println!("Shutdown");
    watchdog_sessions.shutdown().await;
    let mut stop_sessions = JoinSet::new();
    for url in urls {
        stop_sessions.spawn(cops_stop_server_session(client.clone(), url));
    }
    while let Some(result) = stop_sessions.join_next().await {
        result??
    }
    Ok(())
}

async fn cops_start_server_session(
    client: reqwest::Client,
    url: String,
    config: boson_control_messages::CopsServer,
) -> anyhow::Result<()> {
    client
        .post(format!("{url}/cops/start-server"))
        .json(&config)
        .send()
        .await?
        .error_for_status()?;
    loop {
        sleep(Duration::from_millis(1000)).await;
        client
            .get(format!("{url}/ok"))
            .send()
            .await?
            .error_for_status()?;
    }
}

async fn cops_stop_server_session(client: reqwest::Client, url: String) -> anyhow::Result<()> {
    client
        .post(format!("{url}/cops/stop-server"))
        .send()
        .await?
        .error_for_status()?;
    Ok(())
}

async fn cops_client_session(
    client: reqwest::Client,
    url: String,
    config: boson_control_messages::CopsClient,
) -> anyhow::Result<()> {
    client
        .post(format!("{url}/cops/start-client"))
        .json(&config)
        .send()
        .await?
        .error_for_status()?;
    loop {
        sleep(Duration::from_millis(1000)).await;
        let results = client
            .post(format!("{url}/cops/poll-results"))
            .send()
            .await?
            .error_for_status()?
            .json::<Option<Vec<(f32, Duration)>>>()
            .await?;
        if let Some(results) = results {
            println!("{results:?}");
            break Ok(());
        }
    }
}

// cSpell:words reqwest