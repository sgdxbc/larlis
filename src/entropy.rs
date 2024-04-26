use std::{
    collections::{HashMap, HashSet},
    fmt::Debug,
    mem::replace,
    num::NonZeroUsize,
    sync::Arc,
};

use bytes::Bytes;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;
use wirehair::{Decoder, Encoder};

use crate::{
    bulk::{self, RecvOffer, ServiceExt as _},
    crypto::{
        peer::{Crypto, PublicKey, Verifiable},
        DigestHash, H256,
    },
    event::{
        erased::{OnEventRichTimer as OnEvent, RichTimer as Timer},
        SendEvent,
    },
    kademlia::{self, FindPeer, FindPeerOk, PeerId, Target},
    net::{deserialize, events::Recv, kademlia::Multicast, Addr, Payload, SendMessage},
    worker::Submit,
};

type Chunk = Target;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Invite {
    chunk: Chunk,
    peer_id: PeerId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InviteOk {
    chunk: Chunk,
    index: u32,
    proof: (),
    peer_id: PeerId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendFragment {
    chunk: Chunk,
    index: u32,
    peer_id: Option<PeerId>, // Some(id) if expecting receipt
}

#[derive(Debug, Clone, Hash, Serialize, Deserialize)]
pub struct FragmentAvailable {
    chunk: Chunk,
    peer_id: PeerId,
    peer_key: PublicKey,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pull {
    chunk: Chunk,
    peer_id: PeerId,
}

// TODO generalize on address types, lifting up underlying `PeerNet` type
// parameter
pub trait Net:
    SendMessage<Multicast, Invite>
    + SendMessage<PeerId, InviteOk>
    + SendMessage<PeerId, Verifiable<FragmentAvailable>>
    + SendMessage<Multicast, Pull>
{
}
impl<
        T: SendMessage<Multicast, Invite>
            + SendMessage<PeerId, InviteOk>
            + SendMessage<PeerId, Verifiable<FragmentAvailable>>
            + SendMessage<Multicast, Pull>,
    > Net for T
{
}

#[derive(Debug, Clone)]
pub struct Put<K>(pub K, pub Bytes);
#[derive(Debug, Clone)]
pub struct Get<K>(pub K);

#[derive(Debug, Clone)]
pub struct PutOk<K>(pub K);
#[derive(Debug, Clone)]
pub struct GetOk<K>(pub K, pub Payload);

pub trait Upcall<K>: SendEvent<PutOk<K>> + SendEvent<GetOk<K>> {}
impl<T: SendEvent<PutOk<K>> + SendEvent<GetOk<K>>, K> Upcall<K> for T {}

#[derive(Debug)]
pub struct NewEncoder(Chunk, Encoder);
#[derive(Debug, Clone)]
pub struct Encode(Chunk, u32, Payload);
#[derive(Debug)]
pub struct Decode(Chunk, Decoder);
#[derive(Debug, Clone)]
pub struct Recover(Chunk, Payload);
#[derive(Debug, Clone)]
pub struct RecoverEncode(Chunk, Payload);

pub trait SendCodecEvent:
    SendEvent<NewEncoder>
    + SendEvent<Encode>
    + SendEvent<Decode>
    + SendEvent<Recover>
    + SendEvent<RecoverEncode>
{
}
impl<
        T: SendEvent<NewEncoder>
            + SendEvent<Encode>
            + SendEvent<Decode>
            + SendEvent<Recover>
            + SendEvent<RecoverEncode>,
    > SendCodecEvent for T
{
}

pub struct CodecWorker<W, E>(W, std::marker::PhantomData<E>);

impl<W, E> From<W> for CodecWorker<W, E> {
    fn from(value: W) -> Self {
        Self(value, Default::default())
    }
}

impl<W: Submit<(), E>, E: SendCodecEvent + 'static> Submit<(), dyn SendCodecEvent>
    for CodecWorker<W, E>
{
    fn submit(&mut self, work: crate::worker::Work<(), dyn SendCodecEvent>) -> anyhow::Result<()> {
        self.0.submit(Box::new(move |(), emit| work(&(), emit)))
    }
}

pub trait SendFsEvent: SendEvent<fs::Store> + SendEvent<fs::Load> {}
impl<T: SendEvent<fs::Store> + SendEvent<fs::Load>> SendFsEvent for T {}

#[derive(Debug)]
pub struct DownloadOk(pub Chunk, pub u32, pub Payload);

pub trait BulkService: bulk::Service<PeerId, SendFragment, DownloadOk> {}
impl<T: bulk::Service<PeerId, SendFragment, DownloadOk>> BulkService for T {}

pub struct Peer<N, BS, U, CW, F, K, M = (N, BS, U, CW, F, K)> {
    id: PeerId,
    fragment_len: u32,
    chunk_k: NonZeroUsize, // the number of honest peers to recover a chunk
    // the number of peers that, with high probability at least `k` peers of them are honest
    chunk_n: NonZeroUsize,
    chunk_m: NonZeroUsize, // the number of peers that Put peer send Invite to
    // `n` should be derived from `k` and the upper bound of faulty portion
    // `m` should be derived from `n` and the upper bound of faulty portion, because Put can only
    // be concluded when at least `n` peers replies FragmentAvailable, and the worst case is that
    // only honest peers do so
    // alternatively, Put peer can incrementally Invite more and more peers until enough peers
    // replies. that approach would involve timeout and does not fit evaluation purpose
    //
    uploads: HashMap<Chunk, UploadState<K>>,
    downloads: HashMap<Chunk, DownloadState<K>>,
    persists: HashMap<Chunk, PersistState>,
    pending_pulls: HashMap<Chunk, Vec<PeerId>>,

    net: N,
    bulk: BS,
    upcall: U,
    codec_worker: CW,
    fs: F,
    // well, in the context of entropy "computationally intensive" refers to some millisecond-scale
    // computational workload (which is what `codec_worker` for), and the system throughput is
    // tightly bounded on bandwidth (instead of pps) so saving stateful-processing overhead becomes
    // marginal for improving performance
    // in conclusion, do crypto inline, less event and less concurrency to consider :)
    crypto: Crypto,

    _m: std::marker::PhantomData<M>,
}

#[derive(Debug)]
struct UploadState<K> {
    preimage: K,
    encoder: Option<Arc<Encoder>>,
    pending: HashMap<u32, PeerId>,
    available: HashSet<PeerId>,
    cancel: CancellationToken,
}

#[derive(Debug)]
struct DownloadState<K> {
    preimage: K,
    recover: RecoverState,
}

#[derive(Debug)]
struct RecoverState {
    decoder: Option<Decoder>,
    pending: HashMap<u32, Payload>,
    received: HashSet<u32>,
    cancel: CancellationToken,
}

impl RecoverState {
    fn new(fragment_len: u32, chunk_k: NonZeroUsize) -> anyhow::Result<Self> {
        Ok(Self {
            decoder: Some(Decoder::new(
                fragment_len as u64 * chunk_k.get() as u64,
                fragment_len,
            )?),
            pending: Default::default(),
            received: Default::default(),
            cancel: CancellationToken::new(),
        })
    }
}

#[derive(Debug)]
struct PersistState {
    index: u32,
    status: PersistStatus,
    notify: Option<PeerId>,
}

#[derive(Debug)]
enum PersistStatus {
    Recovering(RecoverState),
    Storing,
    Available,
}

impl<N, BS, U, CW, F, K> Debug for Peer<N, BS, U, CW, F, K> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Peer").finish_non_exhaustive()
    }
}

impl<N, BS, U, CW, F, K> Peer<N, BS, U, CW, F, K> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: PeerId,
        crypto: Crypto,
        fragment_len: u32,
        chunk_k: NonZeroUsize,
        chunk_n: NonZeroUsize,
        chunk_m: NonZeroUsize,
        net: N,
        bulk: BS,
        upcall: U,
        codec_worker: CW,
        fs: F,
    ) -> Self {
        Self {
            id,
            crypto,
            fragment_len,
            chunk_k,
            chunk_n,
            chunk_m,
            net,
            bulk,
            upcall,
            codec_worker,
            fs,

            uploads: Default::default(),
            downloads: Default::default(),
            persists: Default::default(),
            pending_pulls: Default::default(),
            _m: Default::default(),
        }
    }
}

pub trait Preimage {
    fn target(&self) -> Target;
}

impl Preimage for H256 {
    fn target(&self) -> Target {
        *self
    }
}

pub trait PeerCommon {
    type N: Net;
    type BS: BulkService;
    type U: Upcall<Self::K>;
    type CW: Submit<(), dyn SendCodecEvent>;
    type F: SendFsEvent;
    type K: Preimage;
}
impl<N, BS, U, CW, F, K> PeerCommon for (N, BS, U, CW, F, K)
where
    N: Net,
    BS: BulkService,
    U: Upcall<K>,
    CW: Submit<(), dyn SendCodecEvent>,
    F: SendFsEvent,
    K: Preimage,
{
    type N = N;
    type BS = BS;
    type U = U;
    type CW = CW;
    type F = F;
    type K = K;
}

impl<M: PeerCommon> OnEvent<Put<M::K>> for Peer<M::N, M::BS, M::U, M::CW, M::F, M::K, M> {
    fn on_event(
        &mut self,
        Put(preimage, buf): Put<M::K>,
        _: &mut impl Timer<Self>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            buf.len() == self.fragment_len as usize * self.chunk_k.get(),
            "expect chunk len {} * {}, actual {}",
            self.fragment_len,
            self.chunk_k,
            buf.len()
        );
        let chunk = preimage.target();
        let replaced = self.uploads.insert(
            chunk,
            UploadState {
                preimage,
                encoder: None,
                pending: Default::default(),
                available: Default::default(),
                cancel: CancellationToken::new(),
            },
        );
        anyhow::ensure!(replaced.is_none(), "duplicated upload chunk {chunk}");
        let fragment_len = self.fragment_len;
        self.codec_worker.submit(Box::new(move |(), sender| {
            let encoder = Encoder::new(buf.into(), fragment_len)?;
            sender.send(NewEncoder(chunk, encoder))
        }))
    }
}

impl<M: PeerCommon> OnEvent<NewEncoder> for Peer<M::N, M::BS, M::U, M::CW, M::F, M::K, M> {
    fn on_event(
        &mut self,
        NewEncoder(chunk, encoder): NewEncoder,
        _: &mut impl Timer<Self>,
    ) -> anyhow::Result<()> {
        let Some(state) = self.uploads.get_mut(&chunk) else {
            return Ok(()); // ok?
        };
        let replaced = state.encoder.replace(encoder.into());
        assert!(replaced.is_none());
        let invite = Invite {
            chunk,
            peer_id: self.id,
        };
        // println!("multicast Invite {}", H256(chunk));
        self.net.send(Multicast(chunk, self.chunk_m), invite)
    }
}

impl<M: PeerCommon> OnEvent<Recv<Invite>> for Peer<M::N, M::BS, M::U, M::CW, M::F, M::K, M> {
    fn on_event(
        &mut self,
        Recv(invite): Recv<Invite>,
        _: &mut impl Timer<Self>,
    ) -> anyhow::Result<()> {
        // technically this is fine, just for more stable evaluation
        // the PUT/GET peers are expected to perform additional object-level codec
        // exclude them from chunk-level workload prevents performance interference
        if invite.peer_id == self.id {
            return Ok(());
        }
        let index = rand::random(); // TODO
        self.persists.entry(invite.chunk).or_insert(PersistState {
            index,
            status: PersistStatus::Recovering(RecoverState::new(self.fragment_len, self.chunk_k)?),
            notify: None,
        });
        // TODO provide a way to suppress unnecessary following `SendFragment`
        let invite_ok = InviteOk {
            chunk: invite.chunk,
            index,
            proof: (),
            peer_id: self.id,
        };
        self.net.send(invite.peer_id, invite_ok)
    }
}

impl<M: PeerCommon> OnEvent<Recv<InviteOk>> for Peer<M::N, M::BS, M::U, M::CW, M::F, M::K, M> {
    fn on_event(
        &mut self,
        Recv(invite_ok): Recv<InviteOk>,
        _: &mut impl Timer<Self>,
    ) -> anyhow::Result<()> {
        // TODO verify
        if let Some(state) = self.uploads.get_mut(&invite_ok.chunk) {
            if state.pending.contains_key(&invite_ok.index) {
                return Ok(());
            }
            state.pending.insert(invite_ok.index, invite_ok.peer_id);
            let encoder = state.encoder.clone().unwrap();
            return self.codec_worker.submit(Box::new(move |(), sender| {
                let fragment = encoder.encode(invite_ok.index)?;
                sender.send(Encode(invite_ok.chunk, invite_ok.index, Payload(fragment)))
            }));
        }
        if let Some(state) = self.persists.get_mut(&invite_ok.chunk) {
            if invite_ok.index == state.index {
                // store duplicated fragment is permitted but against the work's idea
                // should happen very rare anyway
                return Ok(());
            }
            // TODO load fragment and send
        }
        Ok(())
    }
}

impl<M: PeerCommon> OnEvent<Encode> for Peer<M::N, M::BS, M::U, M::CW, M::F, M::K, M> {
    fn on_event(
        &mut self,
        Encode(chunk, index, Payload(fragment)): Encode,
        _: &mut impl Timer<Self>,
    ) -> anyhow::Result<()> {
        let Some(state) = self.uploads.get(&chunk) else {
            return Ok(());
        };
        // TODO code path for serving from persistent state
        let Some(peer_id) = state.pending.get(&index) else {
            // is this ok?
            return Ok(());
        };
        let send_fragment = SendFragment {
            chunk,
            index,
            peer_id: Some(self.id),
        };
        self.bulk
            .offer(*peer_id, send_fragment, fragment, state.cancel.clone())
    }
}

impl<M: PeerCommon> OnEvent<RecvOffer<SendFragment>>
    for Peer<M::N, M::BS, M::U, M::CW, M::F, M::K, M>
{
    fn on_event(
        &mut self,
        mut send_fragment: RecvOffer<SendFragment>,
        _: &mut impl Timer<Self>,
    ) -> anyhow::Result<()> {
        if let Some(state) = self.downloads.get_mut(&send_fragment.chunk) {
            // println!("recv blob {}", H256(send_fragment.chunk));
            let chunk = send_fragment.chunk;
            let index = send_fragment.index;
            return self.bulk.accept(
                &mut send_fragment,
                self.fragment_len as usize,
                move |buf| DownloadOk(chunk, index, Payload(buf)),
                state.recover.cancel.clone(),
            );
        }
        if let Some(state) = self.persists.get_mut(&send_fragment.chunk) {
            let PersistStatus::Recovering(recover) = &mut state.status else {
                if let Some(peer_id) = send_fragment.peer_id {
                    assert_eq!(send_fragment.index, state.index);
                    let fragment_available = FragmentAvailable {
                        chunk: send_fragment.chunk,
                        peer_id: self.id,
                        peer_key: self.crypto.public_key(),
                    };
                    self.net
                        .send(peer_id, self.crypto.sign(fragment_available))?
                }
                return Ok(());
            };
            if send_fragment.index == state.index {
                assert!(state.notify.is_none());
                let peer_id = send_fragment
                    .peer_id
                    .ok_or(anyhow::format_err!("expected peer id in SendFragment"))?;
                state.notify = Some(peer_id);
            }
            let chunk = send_fragment.chunk;
            let index = send_fragment.index;
            return self.bulk.accept(
                &mut send_fragment,
                self.fragment_len as usize,
                move |buf| DownloadOk(chunk, index, Payload(buf)),
                recover.cancel.clone(),
            );
        }
        Ok(())
    }
}

impl<M: PeerCommon> OnEvent<DownloadOk> for Peer<M::N, M::BS, M::U, M::CW, M::F, M::K, M> {
    fn on_event(
        &mut self,
        DownloadOk(chunk, index, fragment): DownloadOk,
        _: &mut impl Timer<Self>,
    ) -> anyhow::Result<()> {
        if let Some(state) = self.downloads.get_mut(&chunk) {
            // println!("download {} index {index}", H256(chunk));
            return state.recover.submit_decode(
                chunk,
                index,
                fragment,
                None,
                &mut self.codec_worker,
            );
        }
        if let Some(state) = self.persists.get_mut(&chunk) {
            let PersistStatus::Recovering(recover) = &mut state.status else {
                return Ok(());
            };
            if index == state.index {
                state.status = PersistStatus::Storing;
                self.fs.send(fs::Store(chunk, state.index, fragment))?
            } else if recover.received.insert(index) {
                recover.submit_decode(
                    chunk,
                    index,
                    fragment,
                    Some(state.index),
                    &mut self.codec_worker,
                )?
            } // otherwise it's either decoded or pending
            return Ok(());
        }
        Ok(())
    }
}

impl RecoverState {
    fn submit_decode(
        &mut self,
        chunk: Chunk,
        index: u32,
        fragment: Payload,
        encode_index: Option<u32>,
        worker: &mut impl Submit<(), dyn SendCodecEvent>,
    ) -> anyhow::Result<()> {
        if let Some(mut decoder) = self.decoder.take() {
            // println!("submit decode {} index {index}", H256(chunk));
            worker.submit(Box::new(move |(), sender| {
                if !decoder.decode(index, &fragment)? {
                    sender.send(Decode(chunk, decoder))
                } else if let Some(index) = encode_index {
                    let fragment = Encoder::try_from(decoder)?.encode(index)?;
                    sender.send(RecoverEncode(chunk, Payload(fragment)))
                } else {
                    // recover does not return error when there's no sufficient block decoded??
                    sender.send(Recover(chunk, Payload(decoder.recover()?)))
                }
            }))
        } else {
            self.pending.insert(index, fragment);
            Ok(())
        }
    }
}

impl<M: PeerCommon> OnEvent<fs::StoreOk> for Peer<M::N, M::BS, M::U, M::CW, M::F, M::K, M> {
    fn on_event(
        &mut self,
        fs::StoreOk(chunk): fs::StoreOk,
        _: &mut impl Timer<Self>,
    ) -> anyhow::Result<()> {
        let Some(state) = self.persists.get_mut(&chunk) else {
            // is this ok?
            return Ok(());
        };
        state.status = PersistStatus::Available;
        if let Some(peer_id) = state.notify.take() {
            let fragment_available = FragmentAvailable {
                chunk,
                peer_id: self.id,
                peer_key: self.crypto.public_key(),
            };
            self.net
                .send(peer_id, self.crypto.sign(fragment_available))?
        }
        // TODO setup rereplicate timer
        Ok(())
    }
}

impl<M: PeerCommon> OnEvent<Recv<Verifiable<FragmentAvailable>>>
    for Peer<M::N, M::BS, M::U, M::CW, M::F, M::K, M>
{
    fn on_event(
        &mut self,
        Recv(fragment_available): Recv<Verifiable<FragmentAvailable>>,
        _: &mut impl Timer<Self>,
    ) -> anyhow::Result<()> {
        let Some(state) = self.uploads.get_mut(&fragment_available.chunk) else {
            return Ok(());
        };
        if fragment_available.peer_id == fragment_available.peer_key.sha256()
            && self
                .crypto
                .verify(&fragment_available.peer_key, &fragment_available)
                .is_err()
        {
            // TODO log
            return Ok(());
        }
        state.available.insert(fragment_available.peer_id);
        if state.available.len() == self.chunk_n.get() {
            let state = self.uploads.remove(&fragment_available.chunk).unwrap();
            state.cancel.cancel();
            self.upcall.send(PutOk(state.preimage))?
        }
        Ok(())
    }
}

impl<M: PeerCommon> OnEvent<Get<M::K>> for Peer<M::N, M::BS, M::U, M::CW, M::F, M::K, M> {
    fn on_event(
        &mut self,
        Get(preimage): Get<M::K>,
        _: &mut impl Timer<Self>,
    ) -> anyhow::Result<()> {
        let chunk = preimage.target();
        let replaced = self.downloads.insert(
            chunk,
            DownloadState {
                preimage,
                recover: RecoverState::new(self.fragment_len, self.chunk_k)?,
            },
        );
        anyhow::ensure!(replaced.is_none(), "duplicated download chunk {chunk}");
        let pull = Pull {
            chunk,
            peer_id: self.id,
        };
        // println!("multicast Pull {}", H256(chunk));
        self.net.send(Multicast(chunk, self.chunk_m), pull)
    }
}

impl<M: PeerCommon> OnEvent<Recv<Pull>> for Peer<M::N, M::BS, M::U, M::CW, M::F, M::K, M> {
    fn on_event(&mut self, Recv(pull): Recv<Pull>, _: &mut impl Timer<Self>) -> anyhow::Result<()> {
        let Some(state) = self.persists.get(&pull.chunk) else {
            // println!("recv Pull {} (unknown)", H256(pull.chunk));
            return Ok(());
        };
        if !matches!(state.status, PersistStatus::Available) {
            // println!("recv Pull {} (unavailable)", H256(pull.chunk));
            return Ok(());
        }
        self.pending_pulls
            .entry(pull.chunk)
            .or_default()
            .push(pull.peer_id);
        // println!("recv Pull {}", H256(pull.chunk));
        self.fs.send(fs::Load(pull.chunk, state.index, true))
    }
}

impl<M: PeerCommon> OnEvent<fs::LoadOk> for Peer<M::N, M::BS, M::U, M::CW, M::F, M::K, M> {
    fn on_event(
        &mut self,
        fs::LoadOk(chunk, index, Payload(fragment)): fs::LoadOk,
        _: &mut impl Timer<Self>,
    ) -> anyhow::Result<()> {
        let Some(pending) = self.pending_pulls.remove(&chunk) else {
            return Ok(());
        };
        let send_fragment = SendFragment {
            chunk,
            index,
            peer_id: None,
        };
        let fragment = Bytes::from(fragment);
        for peer_id in pending {
            let fragment = fragment.clone();
            // println!("blob transfer {}", H256(chunk));
            self.bulk
                .offer(peer_id, send_fragment.clone(), fragment, None)?
        }
        Ok(())
    }
}

impl<M: PeerCommon> OnEvent<Decode> for Peer<M::N, M::BS, M::U, M::CW, M::F, M::K, M> {
    fn on_event(
        &mut self,
        Decode(chunk, decoder): Decode,
        _: &mut impl Timer<Self>,
    ) -> anyhow::Result<()> {
        if let Some(state) = self.downloads.get_mut(&chunk) {
            // println!("decode {}", H256(chunk));
            return state
                .recover
                .on_decode(chunk, decoder, None, &mut self.codec_worker);
        }
        if let Some(state) = self.persists.get_mut(&chunk) {
            let PersistStatus::Recovering(recover) = &mut state.status else {
                unreachable!()
            };
            return recover.on_decode(chunk, decoder, Some(state.index), &mut self.codec_worker);
        }
        Ok(())
    }
}

impl RecoverState {
    fn on_decode(
        &mut self,
        chunk: Chunk,
        decoder: Decoder,
        encode_index: Option<u32>,
        worker: &mut impl Submit<(), dyn SendCodecEvent>,
    ) -> anyhow::Result<()> {
        let replaced = self.decoder.replace(decoder);
        assert!(replaced.is_none());
        if let Some(&index) = self.pending.keys().next() {
            // println!("continue decode {} index {index}", H256(chunk));
            let fragment = self.pending.remove(&index).unwrap();
            self.submit_decode(chunk, index, fragment, encode_index, worker)?
        }
        Ok(())
    }
}

impl<M: PeerCommon> OnEvent<Recover> for Peer<M::N, M::BS, M::U, M::CW, M::F, M::K, M> {
    fn on_event(
        &mut self,
        Recover(chunk, buf): Recover,
        _: &mut impl Timer<Self>,
    ) -> anyhow::Result<()> {
        // println!("recover {}", H256(chunk));
        if let Some(state) = self.downloads.remove(&chunk) {
            state.recover.cancel.cancel();
            self.upcall.send(GetOk(state.preimage, buf))
        } else {
            Ok(())
        }
    }
}

impl<M: PeerCommon> OnEvent<RecoverEncode> for Peer<M::N, M::BS, M::U, M::CW, M::F, M::K, M> {
    fn on_event(
        &mut self,
        RecoverEncode(chunk, fragment): RecoverEncode,
        _: &mut impl Timer<Self>,
    ) -> anyhow::Result<()> {
        let Some(state) = self.persists.get_mut(&chunk) else {
            return Ok(()); // is this ok?
        };
        let replaced_status = replace(&mut state.status, PersistStatus::Storing);
        let PersistStatus::Recovering(recover) = replaced_status else {
            unreachable!()
        };
        recover.cancel.cancel();
        self.fs.send(fs::Store(chunk, state.index, fragment))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, derive_more::From)]
pub enum Message<A> {
    Invite(Invite),
    InviteOk(InviteOk),
    Pull(Pull),
    FragmentAvailable(Verifiable<FragmentAvailable>),

    FindPeer(Verifiable<FindPeer<A>>),
    FindPeerOk(Verifiable<FindPeerOk<A>>),

    BlobServe(bulk::Serve<SendFragment>),
}

pub type MessageNet<T, A> = crate::net::MessageNet<T, Message<A>>;

pub trait SendRecvEvent:
    SendEvent<Recv<Invite>>
    + SendEvent<Recv<InviteOk>>
    + SendEvent<Recv<Pull>>
    + SendEvent<Recv<Verifiable<FragmentAvailable>>>
{
}
impl<
        T: SendEvent<Recv<Invite>>
            + SendEvent<Recv<InviteOk>>
            + SendEvent<Recv<Pull>>
            + SendEvent<Recv<Verifiable<FragmentAvailable>>>,
    > SendRecvEvent for T
{
}

pub fn on_buf<A: Addr>(
    buf: &[u8],
    entropy_sender: &mut impl SendRecvEvent,
    kademlia_sender: &mut impl kademlia::SendRecvEvent<A>,
    blob_sender: &mut impl SendEvent<Recv<bulk::Serve<SendFragment>>>,
) -> anyhow::Result<()> {
    match deserialize(buf)? {
        Message::Invite(message) => entropy_sender.send(Recv(message)),
        Message::InviteOk(message) => entropy_sender.send(Recv(message)),
        Message::Pull(message) => entropy_sender.send(Recv(message)),
        Message::FragmentAvailable(message) => entropy_sender.send(Recv(message)),
        Message::FindPeer(message) => kademlia_sender.send(Recv(message)),
        Message::FindPeerOk(message) => kademlia_sender.send(Recv(message)),
        Message::BlobServe(message) => blob_sender.send(Recv(message)),
    }
}

// TODO generalize and lift into workspace crate
// the problem arise when justify upcall design
// possibly follows blob design if that works well
pub mod fs {
    use std::{fmt::Debug, path::Path};

    use tokio::{
        fs::{create_dir, read, remove_dir_all, write},
        sync::mpsc::UnboundedReceiver,
        task::JoinSet,
    };

    use crate::{event::SendEvent, net::Payload};

    use super::Chunk;

    #[derive(Debug, Clone)]
    pub struct Store(pub Chunk, pub u32, pub Payload);

    // Load(chunk, index, true) will delete fragment file while loading
    // not particular useful in practice, but good for evaluation with bounded storage usage
    #[derive(Debug, Clone)]
    pub struct Load(pub Chunk, pub u32, pub bool);

    #[derive(Debug, Clone)]
    pub struct StoreOk(pub Chunk);

    #[derive(Debug, Clone)]
    pub struct LoadOk(pub Chunk, pub u32, pub Payload);

    #[derive(Debug, derive_more::From)]
    pub enum Event {
        Store(Store),
        Load(Load),
    }

    pub trait Upcall: SendEvent<StoreOk> + SendEvent<LoadOk> {}
    impl<T: SendEvent<StoreOk> + SendEvent<LoadOk>> Upcall for T {}

    pub async fn session(
        path: impl AsRef<Path>,
        mut events: UnboundedReceiver<Event>,
        mut upcall: impl Upcall,
    ) -> anyhow::Result<()> {
        let mut store_tasks = JoinSet::<anyhow::Result<_>>::new();
        let mut load_tasks = JoinSet::<anyhow::Result<_>>::new();
        loop {
            enum Select {
                Recv(Event),
                JoinNextStore(Chunk),
                JoinNextLoad((Chunk, u32, Payload)),
            }
            match tokio::select! {
                event = events.recv() => Select::Recv(event.ok_or(anyhow::format_err!("channel closed"))?),
                Some(result) = store_tasks.join_next() => Select::JoinNextStore(result??),
                Some(result) = load_tasks.join_next() => Select::JoinNextLoad(result??),
            } {
                Select::Recv(Event::Store(Store(chunk, index, fragment))) => {
                    let chunk_path = path.as_ref().join(format!("{chunk:x}"));
                    store_tasks.spawn(async move {
                        create_dir(&chunk_path).await?;
                        write(chunk_path.join(index.to_string()), &*fragment).await?;
                        Ok(chunk)
                    });
                }
                Select::Recv(Event::Load(Load(chunk, index, take))) => {
                    let chunk_path = path.as_ref().join(format!("{chunk:x}"));
                    load_tasks.spawn(async move {
                        let fragment = read(chunk_path.join(index.to_string())).await?;
                        if take {
                            remove_dir_all(chunk_path).await?
                        }
                        Ok((chunk, index, Payload(fragment)))
                    });
                }
                Select::JoinNextStore(chunk) => upcall.send(StoreOk(chunk))?,
                Select::JoinNextLoad((chunk, index, fragment)) => {
                    upcall.send(LoadOk(chunk, index, fragment))?
                }
            }
        }
    }
}

// cSpell:words kademlia upcall preimage rereplicate