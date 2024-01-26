use std::{env::args, net::SocketAddr};

use augustus::{
    crypto::Crypto,
    event::{
        erasured::{Session, SessionSender},
        SendEvent,
    },
    kademlia::{self, Buckets, Peer, PeerId, SendCryptoEvent},
    net::{
        events::Recv,
        kademlia::{Control, Net},
        SendMessage, Udp,
    },
    worker::erasured::spawn_backend,
};
use bincode::Options;
use primitive_types::H256;
use rand::{rngs::StdRng, thread_rng, SeedableRng};
use serde::{Deserialize, Serialize};
use tokio::{net::UdpSocket, spawn};
use tokio_util::sync::CancellationToken;

#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, Serialize, Deserialize)]
enum Message {
    Kademlia(kademlia::Message<SocketAddr>),
    Hello(PeerId),
    HelloOk,
}

struct MessageNet<T>(T);

impl<T: SendMessage<SocketAddr, Vec<u8>>, N> SendMessage<SocketAddr, N> for MessageNet<T>
where
    N: Into<kademlia::Message<SocketAddr>>,
{
    fn send(&mut self, dest: SocketAddr, message: N) -> anyhow::Result<()> {
        self.0.send(
            dest,
            bincode::options().serialize(&Message::Kademlia(message.into()))?,
        )
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let socket = UdpSocket::bind("127.0.0.1:0").await?;
    let addr = socket.local_addr()?;
    println!("SocketAddr {addr}");
    let socket_net = Udp(socket.into());

    let mut control_session = Session::<Control<_, _>>::new();
    let (crypto_worker, mut crypto_session);
    let peer_id;
    let mut peer;
    let bootstrap_finished = CancellationToken::new();
    let mut send_hello = None;

    let seed_crypto = Crypto::new_random(&mut StdRng::seed_from_u64(117418));
    if let Some(seed_addr) = args().nth(1) {
        let crypto = Crypto::new_random(&mut thread_rng());
        let peer_record = crypto.peer(addr);
        peer_id = peer_record.id;
        println!("PeerId {}", H256(peer_id));
        (crypto_worker, crypto_session) = spawn_backend(crypto);

        let mut buckets = Buckets::new(peer_record);
        let seed_peer = seed_crypto.peer(seed_addr.parse()?);
        send_hello = Some(seed_peer.id);
        buckets.insert(seed_peer);
        peer = Peer::new(
            buckets,
            MessageNet(socket_net.clone()),
            control_session.sender(),
            crypto_worker,
        );
        let cancel = bootstrap_finished.clone();
        peer.bootstrap(Box::new(move || {
            cancel.cancel();
            Ok(())
        }))?;
    } else {
        let peer_record = seed_crypto.peer(addr);
        peer_id = peer_record.id;
        println!("SEED PeerId {}", H256(peer_id));
        (crypto_worker, crypto_session) = spawn_backend(seed_crypto);

        let buckets = Buckets::new(peer_record);
        peer = Peer::new(
            buckets,
            MessageNet(socket_net.clone()),
            control_session.sender(),
            crypto_worker,
        );
        bootstrap_finished.cancel(); // skip bootstrap on seed peer
    }

    let mut peer_session = Session::<Peer<_>>::new();
    let mut peer_sender = peer_session.sender();
    let mut peer_net = Net(control_session.sender());
    let hello_session = spawn({
        let mut peer_net = peer_net.clone();
        async move {
            bootstrap_finished.cancelled().await;
            println!("Bootstrap finished");
            if let Some(seed_id) = send_hello {
                peer_net.send(seed_id, Message::Hello(peer_id)).unwrap()
            }
        }
    });
    let socket_session = socket_net.recv_session(|buf| {
        let message = bincode::options()
            .allow_trailing_bytes()
            .deserialize::<Message>(buf)?;
        match message {
            Message::Kademlia(kademlia::Message::FindPeer(message)) => {
                peer_sender.send(Recv(message))?
            }
            Message::Kademlia(kademlia::Message::FindPeerOk(message)) => {
                peer_sender.send(Recv(message))?
            }
            Message::Hello(peer_id) => {
                println!("Replying Hello from {}", H256(peer_id));
                peer_net.send(peer_id, Message::HelloOk)?
            }
            Message::HelloOk => {
                println!("Received HelloOk")
            }
        }
        Ok(())
    });

    #[derive(Clone)]
    struct S(SessionSender<Peer<SocketAddr>>);
    impl AsMut<dyn SendCryptoEvent<SocketAddr> + Send + Sync + 'static> for S {
        fn as_mut(&mut self) -> &mut (dyn SendCryptoEvent<SocketAddr> + Send + Sync + 'static) {
            &mut self.0
        }
    }

    let crypto_session = crypto_session.run(S(peer_session.sender()));
    let mut control = Control::new(
        augustus::net::MessageNet::<_, Message>::new(socket_net.clone()),
        peer_session.sender(),
    );
    let peer_session = peer_session.run(&mut peer);
    let control_session = control_session.run(&mut control);
    tokio::select! {
        result = socket_session => result?,
        result = crypto_session => result?,
        result = peer_session => result?,
        result = control_session => result?,
        Err(err) = hello_session => Err(err)?,
    }
    Err(anyhow::anyhow!("unreachable"))
}