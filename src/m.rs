use libipld::{Cid, Result};

fn main() {
    println!("Hello, world!");
}

#[cfg(test)]
mod tests {

    use super::*;

    use async_std::task;
    use futures::prelude::*;
    use libipld::block::Block;
    use libipld::cbor::DagCborCodec;
    use libipld::ipld;
    use libipld::ipld::Ipld;
    use libipld::multihash::Code;
    use libipld::store::DefaultParams;
    use libp2p::core::muxing::StreamMuxerBox;
    use libp2p::core::transport::Boxed;
    use libp2p::identity;
    use libp2p::noise::{Keypair, NoiseConfig, X25519Spec};
    use libp2p::swarm::SwarmEvent;
    use libp2p::tcp::TcpConfig;
    use libp2p::yamux::YamuxConfig;
    use libp2p::{PeerId, Swarm, Transport};
    use std::sync::Arc;
    use std::time::Duration;
    use cid::Cid as Forest_Cid;
    use db::rocks::RocksDb;
    use db::Store;
    use ipld_blockstore::BlockStore;
    use libp2p::Multiaddr;
    use libp2p_bitswap::{Bitswap, BitswapConfig, BitswapEvent, BitswapStore, QueryId};
    use std::str::FromStr;

    fn tracing_try_init() {
        tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init()
            .ok();
    }

    fn mk_transport() -> (PeerId, Boxed<(PeerId, StreamMuxerBox)>) {
        let id_key = identity::Keypair::generate_ed25519();
        let peer_id = id_key.public().to_peer_id();
        let dh_key = Keypair::<X25519Spec>::new()
            .into_authentic(&id_key)
            .unwrap();
        let noise = NoiseConfig::xx(dh_key).into_authenticated();

        let transport = TcpConfig::new()
            .nodelay(true)
            .upgrade(libp2p::core::upgrade::Version::V1)
            .authenticate(noise)
            .multiplex(YamuxConfig::default())
            .timeout(Duration::from_secs(20))
            .boxed();
        (peer_id, transport)
    }

    fn create_block(ipld: Ipld) -> Block<DefaultParams> {
        Block::encode(DagCborCodec, Code::Blake3_256, &ipld).unwrap()
    }

    pub struct UrsaStore<DB> {
        pub db: Arc<DB>,
    }

    impl<DB> UrsaStore<DB>
    where
        DB: BlockStore + Send + Sync + 'static,
    {
        pub fn new(db: Arc<DB>) -> Self {
            let cs = Self { db };
            cs
        }
    }
    struct BitswapStorage<P: BlockStore + Sync + Send + 'static>(Arc<UrsaStore<P>>);

    impl<P: BlockStore + Sync + Send + 'static> BitswapStore for BitswapStorage<P> {
        type Params = DefaultParams;

        fn contains(&mut self, cid: &Cid) -> Result<bool> {
            // let key = Forest_Cid::from_str(&cid.to_string());
            // println!("{:?}", &cid);
            // println!("{:?}", &key);

            Ok(self.0.db.exists(cid)?)
        }

        fn get(&mut self, cid: &Cid) -> Result<Option<Vec<u8>>> {
            let key = Forest_Cid::from_str(&cid.to_string()).unwrap();
            println!("{:?}", &cid);
            println!("{:?}", &key);

            Ok(self.0.db.read(key.to_bytes()).unwrap())
        }

        fn insert(&mut self, block: &Block<Self::Params>) -> Result<()> {

            let key = Forest_Cid::from_str(&block.cid().to_string()).unwrap();
            println!("{:?}", &block.cid().to_string());
            println!("{:?}", &key);

            self.0
                .db
                .write(key.to_bytes(), block.data().to_vec())
                .unwrap();

            Ok(())
        }

        fn missing_blocks(&mut self, cid: &Cid) -> Result<Vec<Cid>> {

            let mut stack = vec![*cid];
            let mut missing = vec![];
            
            while let Some(cid) = stack.pop() {
                if let Some(data) = self.get(&cid)? {
                    let block = Block::<Self::Params>::new_unchecked(cid, data);
                    block.references(&mut stack)?;
                } else {
                    missing.push(cid);
                }
            }

            Ok(missing)
        }
    }

    struct Peer<P> {
        peer_id: PeerId,
        addr: Multiaddr,
        store: Arc<UrsaStore<P>>,
        swarm: Swarm<Bitswap<DefaultParams>>,
    }

    impl<P> Peer<P>
    where
        P: BlockStore + Sync + Send + 'static,
    {
        fn new(chain_store: Arc<UrsaStore<P>>) -> Self {
            let (peer_id, trans) = mk_transport();

            let bitswap = BitswapStorage(chain_store.clone());

            let mut swarm = Swarm::new(trans, Bitswap::new(BitswapConfig::new(), bitswap), peer_id);
            Swarm::listen_on(&mut swarm, "/ip4/127.0.0.1/tcp/0".parse().unwrap()).unwrap();
            while swarm.next().now_or_never().is_some() {}
            let addr = Swarm::listeners(&swarm).next().unwrap().clone();
            Self {
                peer_id,
                addr,
                store: chain_store,
                swarm,
            }
        }

        fn add_address(&mut self, peer: &Peer<P>) {
            self.swarm
                .behaviour_mut()
                .add_address(&peer.peer_id, peer.addr.clone());
        }

        fn swarm(&mut self) -> &mut Swarm<Bitswap<DefaultParams>> {
            &mut self.swarm
        }

        fn spawn(mut self, name: &'static str) -> PeerId {
            let peer_id = self.peer_id;
            task::spawn(async move {
                loop {
                    let event = self.swarm.next().await;
                    tracing::debug!("{}: {:?}", name, event);
                }
            });
            peer_id
        }

        async fn next(&mut self) -> Option<BitswapEvent> {
            loop {
                let ev = self.swarm.next().await?;
                if let SwarmEvent::Behaviour(event) = ev {
                    return Some(event);
                }
            }
        }
    }

    fn assert_progress(event: Option<BitswapEvent>, id: QueryId, missing: usize) {
        if let Some(BitswapEvent::Progress(id2, missing2)) = event {
            assert_eq!(id2, id);
            assert_eq!(missing2, missing);
        } else {
            panic!("{:?} is not a progress event", event);
        }
    }

    fn assert_complete_ok(event: Option<BitswapEvent>, id: QueryId) {
        if let Some(BitswapEvent::Complete(id2, Ok(()))) = event {
            assert_eq!(id2, id);
        } else {
            panic!("{:?} is not a complete event", event);
        }
    }

    #[async_std::test]
    async fn test_bitswap_get() {
        tracing_try_init();
        #[cfg(feature = "rocksdb")]
        let db = RocksDb::open("test_db").expect("Opening RocksDB must succeed");
        let db = Arc::new(db);

        let chain_store = Arc::new(UrsaStore::new(Arc::clone(&db)));

        let peer1 = Peer::new(Arc::clone(&chain_store));
        let mut peer2 = Peer::new(Arc::clone(&chain_store));
        peer2.add_address(&peer1);

        let block = create_block(ipld!(&b"hello world"[..]));

        let key = Forest_Cid::from_str(&block.cid().to_string()).unwrap();
        peer1
            .store
            .db
            .write(key.to_bytes(), block.data().to_vec())
            .unwrap();

        let peer1 = peer1.spawn("peer1");

        let id = peer2
            .swarm()
            .behaviour_mut()
            .get(*block.cid(), std::iter::once(peer1));

        assert_complete_ok(peer2.next().await, id);
    }

    #[async_std::test]
    async fn test_bitswap_cancel_get() {
        tracing_try_init();
        #[cfg(feature = "rocksdb")]
        let db = RocksDb::open("test_db").expect("Opening RocksDB must succeed");
        let db = Arc::new(db);

        let chain_store = Arc::new(UrsaStore::new(Arc::clone(&db)));

        let peer1 = Peer::new(Arc::clone(&chain_store));
        let mut peer2 = Peer::new(Arc::clone(&chain_store));
        peer2.add_address(&peer1);

        let block = create_block(ipld!(&b"hello world"[..]));
        let key = Forest_Cid::from_str(&block.cid().to_string()).unwrap();
        peer1
            .store
            .db
            .write(key.to_bytes(), block.data().to_vec())
            .unwrap();

        let peer1 = peer1.spawn("peer1");

        let id = peer2
            .swarm()
            .behaviour_mut()
            .get(*block.cid(), std::iter::once(peer1));
        peer2.swarm().behaviour_mut().cancel(id);

        let res = peer2.next().now_or_never();
        println!("{:?}", res);
        assert!(res.is_none());
    }

    #[async_std::test]
    async fn test_bitswap_sync() {
        tracing_try_init();
        #[cfg(feature = "rocksdb")]
        let db = RocksDb::open("test_db").expect("Opening RocksDB must succeed");
        let db = Arc::new(db);

        let chain_store = Arc::new(UrsaStore::new(Arc::clone(&db)));

        let peer1 = Peer::new(Arc::clone(&chain_store));
        let mut peer2 = Peer::new(Arc::clone(&chain_store));
        peer2.add_address(&peer1);

        let b0 = create_block(ipld!({
            "n": 0,
        }));

        let b1 = create_block(ipld!({
            "prev": b0.cid(),
            "n": 1,
        }));

        let b2 = create_block(ipld!({
            "prev": b1.cid(),
            "n": 2,
        }));

        let key0 = Forest_Cid::from_str(&b0.cid().to_string()).unwrap();
        let key1 = Forest_Cid::from_str(&b1.cid().to_string()).unwrap();
        let key2 = Forest_Cid::from_str(&b2.cid().to_string()).unwrap();

        peer1
            .store
            .db
            .write(key0.to_bytes(), b0.data().to_vec())
            .unwrap();
        peer1
            .store
            .db
            .write(key1.to_bytes(), b1.data().to_vec())
            .unwrap();
        peer1
            .store
            .db
            .write(key2.to_bytes(), b2.data().to_vec())
            .unwrap();

        let peer1 = peer1.spawn("peer1");

        let id =
            peer2
                .swarm()
                .behaviour_mut()
                .sync(*b2.cid(), vec![peer1], std::iter::once(*b2.cid()));

        assert_progress(peer2.next().await, id, 1);
        assert_progress(peer2.next().await, id, 1);

        assert_complete_ok(peer2.next().await, id);
    }

    #[async_std::test]
    async fn test_bitswap_cancel_sync() {
        tracing_try_init();
        #[cfg(feature = "rocksdb")]
        let db = RocksDb::open("test_db").expect("Opening RocksDB must succeed");
        let db = Arc::new(db);

        let chain_store = Arc::new(UrsaStore::new(Arc::clone(&db)));

        let peer1 = Peer::new(Arc::clone(&chain_store));
        let mut peer2 = Peer::new(Arc::clone(&chain_store));
        peer2.add_address(&peer1);

        let block = create_block(ipld!(&b"hello world"[..]));
        peer1
            .store
            .db
            .put_raw(block.data().to_vec(), cid::Code::Blake2b256)
            .unwrap();
        let peer1 = peer1.spawn("peer1");

        let id = peer2.swarm().behaviour_mut().sync(
            *block.cid(),
            vec![peer1],
            std::iter::once(*block.cid()),
        );
        peer2.swarm().behaviour_mut().cancel(id);
        let res = peer2.next().now_or_never();
        println!("{:?}", res);
        assert!(res.is_none());
    }

    // #[cfg(feature = "compat")]
    #[async_std::test]
    async fn compat_test() {
        tracing_try_init();
        let cid: Cid = "QmXQsqVRpp2W7fbYZHi4aB2Xkqfd3DpwWskZoLVEYigMKC"
            .parse()
            .unwrap();
        let peer_id: PeerId = "QmRSGx67Kq8w7xSBDia7hQfbfuvauMQGgxcwSWw976x4BS"
            .parse()
            .unwrap();
        let multiaddr: Multiaddr = "/ip4/54.173.33.96/tcp/4001".parse().unwrap();

        #[cfg(feature = "rocksdb")]
        let db = RocksDb::open("test_db").expect("Opening RocksDB must succeed");
        let db = Arc::new(db);

        let chain_store = Arc::new(UrsaStore::new(Arc::clone(&db)));

        let mut peer = Peer::new(Arc::clone(&chain_store));
        peer.swarm()
            .behaviour_mut()
            .add_address(&peer_id, multiaddr);

        let id = peer
            .swarm()
            .behaviour_mut()
            .get(cid, std::iter::once(peer_id));
        assert_complete_ok(peer.next().await, id);
    }
}
