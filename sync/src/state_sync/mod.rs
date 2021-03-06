use crate::download::{DownloadActor, SyncEvent};
use crate::helper::{get_accumulator_node_by_node_hash, get_state_node_by_node_hash};
use crate::sync_metrics::{LABEL_ACCUMULATOR, LABEL_STATE, SYNC_METRICS};
use actix::prelude::*;
use actix::{Actor, Addr, Context, Handler};
use anyhow::Result;
use crypto::hash::HashValue;
use forkable_jellyfish_merkle::node_type::Node;
use forkable_jellyfish_merkle::SPARSE_MERKLE_PLACEHOLDER_HASH;
use futures::executor::block_on;
use logger::prelude::*;
use network::NetworkAsyncService;
use network_api::NetworkService;
use parking_lot::Mutex;
use starcoin_accumulator::node::{AccumulatorStoreType, ACCUMULATOR_PLACEHOLDER_HASH};
use starcoin_accumulator::AccumulatorNode;
use starcoin_state_tree::StateNode;
use starcoin_storage::Store;
use starcoin_sync_api::{StateSyncReset, SyncMetadata};
use std::collections::{HashMap, VecDeque};
use std::convert::TryFrom;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use traits::Consensus;
use types::{
    account_state::AccountState,
    peer_info::{PeerId, PeerInfo},
};

struct Roots {
    state: HashValue,
    txn_accumulator: HashValue,
    block_accumulator: HashValue,
}

impl Roots {
    pub fn new(state: HashValue, txn_accumulator: HashValue, block_accumulator: HashValue) -> Self {
        Roots {
            state,
            txn_accumulator,
            block_accumulator,
        }
    }

    fn state_root(&self) -> &HashValue {
        &self.state
    }

    fn txn_accumulator_root(&self) -> &HashValue {
        &self.txn_accumulator
    }

    fn block_accumulator_root(&self) -> &HashValue {
        &self.block_accumulator
    }
}

async fn sync_accumulator_node<C>(
    node_key: HashValue,
    peer_id: PeerId,
    network_service: NetworkAsyncService,
    address: Addr<StateSyncTaskActor<C>>,
    accumulator_type: AccumulatorStoreType,
) where
    C: Consensus + Sync + Send + 'static + Clone,
{
    let accumulator_timer = SYNC_METRICS
        .sync_done_time
        .with_label_values(&[LABEL_ACCUMULATOR])
        .start_timer();
    let accumulator_node = match get_accumulator_node_by_node_hash(
        &network_service,
        peer_id.clone(),
        node_key,
        accumulator_type.clone(),
    )
    .await
    {
        Ok(accumulator_node) => {
            if node_key == accumulator_node.hash() {
                SYNC_METRICS
                    .sync_succ_count
                    .with_label_values(&[LABEL_ACCUMULATOR])
                    .inc();
                Some(accumulator_node)
            } else {
                SYNC_METRICS
                    .sync_verify_fail_count
                    .with_label_values(&[LABEL_ACCUMULATOR])
                    .inc();
                warn!(
                    "accumulator node hash miss match {} :{:?}",
                    node_key,
                    accumulator_node.hash()
                );
                None
            }
        }
        Err(e) => {
            SYNC_METRICS
                .sync_fail_count
                .with_label_values(&[LABEL_ACCUMULATOR])
                .inc();
            debug!("{:?}", e);
            None
        }
    };
    accumulator_timer.observe_duration();

    if let Err(err) = address.try_send(StateSyncTaskEvent::new_accumulator(
        peer_id,
        node_key,
        accumulator_node,
        accumulator_type,
    )) {
        error!("Send accumulator StateSyncTaskEvent failed : {:?}", err);
    };
}

async fn sync_state_node<C>(
    node_key: HashValue,
    peer_id: PeerId,
    network_service: NetworkAsyncService,
    address: Addr<StateSyncTaskActor<C>>,
) where
    C: Consensus + Sync + Send + 'static + Clone,
{
    let state_timer = SYNC_METRICS
        .sync_done_time
        .with_label_values(&[LABEL_STATE])
        .start_timer();
    let state_node =
        match get_state_node_by_node_hash(&network_service, peer_id.clone(), node_key).await {
            Ok(state_node) => {
                if node_key == state_node.0.hash() {
                    SYNC_METRICS
                        .sync_succ_count
                        .with_label_values(&[LABEL_STATE])
                        .inc();
                    Some(state_node)
                } else {
                    SYNC_METRICS
                        .sync_verify_fail_count
                        .with_label_values(&[LABEL_STATE])
                        .inc();
                    warn!(
                        "state node hash miss match {} :{:?}",
                        node_key,
                        state_node.0.hash()
                    );
                    None
                }
            }
            Err(e) => {
                SYNC_METRICS
                    .sync_fail_count
                    .with_label_values(&[LABEL_STATE])
                    .inc();
                debug!("{:?}", e);
                None
            }
        };
    state_timer.observe_duration();

    if let Err(err) = address.try_send(StateSyncTaskEvent::new_state(peer_id, node_key, state_node))
    {
        error!("Send state StateSyncTaskEvent failed : {:?}", err);
    };
}

#[derive(Clone)]
pub struct StateSyncTaskRef<C>
where
    C: Consensus + Sync + Send + 'static + Clone,
{
    address: Addr<StateSyncTaskActor<C>>,
}

#[async_trait::async_trait]
impl<C> StateSyncReset for StateSyncTaskRef<C>
where
    C: Consensus + Sync + Send + 'static + Clone,
{
    async fn reset(
        &self,
        state_root: HashValue,
        txn_accumulator_root: HashValue,
        block_accumulator_root: HashValue,
    ) {
        if let Err(e) = self
            .address
            .send(StateSyncEvent::RESET(RestRoots {
                state_root,
                txn_accumulator_root,
                block_accumulator_root,
            }))
            .await
        {
            error!("Send RESET StateSyncEvent failed : {:?}", e);
        }
    }

    async fn act(&self) {
        if let Err(e) = self.address.send(StateSyncEvent::ACT {}).await {
            error!("Send ACT StateSyncEvent failed : {:?}", e);
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum TaskType {
    STATE,
    TxnAccumulator,
    BlockAccumulator,
}

#[derive(Debug, Message)]
#[rtype(result = "Result<()>")]
struct StateSyncTaskEvent {
    peer_id: PeerId,
    node_key: HashValue,
    state_node: Option<StateNode>,
    accumulator_node: Option<AccumulatorNode>,
    task_type: TaskType,
}

impl StateSyncTaskEvent {
    pub fn new_state(peer_id: PeerId, node_key: HashValue, state_node: Option<StateNode>) -> Self {
        StateSyncTaskEvent {
            peer_id,
            node_key,
            state_node,
            accumulator_node: None,
            task_type: TaskType::STATE,
        }
    }

    pub fn new_accumulator(
        peer_id: PeerId,
        node_key: HashValue,
        accumulator_node: Option<AccumulatorNode>,
        accumulator_type: AccumulatorStoreType,
    ) -> Self {
        StateSyncTaskEvent {
            peer_id,
            node_key,
            state_node: None,
            accumulator_node,
            task_type: match accumulator_type {
                AccumulatorStoreType::Block => TaskType::BlockAccumulator,
                AccumulatorStoreType::Transaction => TaskType::TxnAccumulator,
            },
        }
    }
}

pub struct StateSyncTaskActor<C>
where
    C: Consensus + Sync + Send + 'static + Clone,
{
    self_peer_id: PeerId,
    roots: Roots,
    storage: Arc<dyn Store>,
    network_service: NetworkAsyncService,
    sync_metadata: SyncMetadata,
    state_sync_task: Arc<Mutex<SyncTask<(HashValue, bool)>>>,
    txn_accumulator_sync_task: Arc<Mutex<SyncTask<HashValue>>>,
    block_accumulator_sync_task: Arc<Mutex<SyncTask<HashValue>>>,
    connect_address: Addr<DownloadActor<C>>,
}

pub struct SyncTask<T> {
    wait_2_sync: VecDeque<T>,
    syncing_nodes: HashMap<PeerId, T>,
    done_tasks: AtomicU64,
}

impl<T> SyncTask<T> {
    fn new() -> Self {
        Self {
            wait_2_sync: VecDeque::new(),
            syncing_nodes: HashMap::new(),
            done_tasks: AtomicU64::new(0),
        }
    }

    fn do_one_task(&self) {
        self.done_tasks.fetch_add(1, Ordering::Relaxed);
    }

    fn is_empty(&mut self) -> bool {
        self.wait_2_sync.is_empty() && self.syncing_nodes.is_empty()
    }

    fn task_info(&self) -> (usize, usize, u64) {
        (
            self.wait_2_sync.len(),
            self.syncing_nodes.len(),
            self.done_tasks.load(Ordering::Relaxed),
        )
    }

    pub fn push_back(&mut self, value: T) {
        self.wait_2_sync.push_back(value)
    }

    pub fn pop_front(&mut self) -> Option<T> {
        self.wait_2_sync.pop_front()
    }

    pub fn clear(&mut self) {
        self.wait_2_sync.clear();
        self.syncing_nodes.clear();
        self.done_tasks = AtomicU64::new(0);
    }

    pub fn insert(&mut self, peer_id: PeerId, value: T) -> Option<T> {
        self.syncing_nodes.insert(peer_id, value)
    }

    pub fn get(&self, peer_id: &PeerId) -> Option<&T> {
        self.syncing_nodes.get(peer_id)
    }

    pub fn remove(&mut self, peer_id: &PeerId) -> Option<T> {
        self.syncing_nodes.remove(peer_id)
    }
}

impl<C> StateSyncTaskActor<C>
where
    C: Consensus + Sync + Send + 'static + Clone,
{
    pub fn launch(
        self_peer_id: PeerId,
        root: (HashValue, HashValue, HashValue),
        storage: Arc<dyn Store>,
        network_service: NetworkAsyncService,
        sync_metadata: SyncMetadata,
        address: Addr<DownloadActor<C>>,
    ) -> StateSyncTaskRef<C> {
        let roots = Roots::new(root.0, root.1, root.2);
        let mut state_sync_task = SyncTask::new();
        state_sync_task.push_back((*roots.state_root(), true));
        let mut txn_accumulator_sync_task = SyncTask::new();
        txn_accumulator_sync_task.push_back(*roots.txn_accumulator_root());
        let mut block_accumulator_sync_task = SyncTask::new();
        block_accumulator_sync_task.push_back(*roots.block_accumulator_root());
        let address = StateSyncTaskActor::create(move |_ctx| Self {
            self_peer_id,
            roots,
            storage,
            network_service,
            sync_metadata,
            state_sync_task: Arc::new(Mutex::new(state_sync_task)),
            txn_accumulator_sync_task: Arc::new(Mutex::new(txn_accumulator_sync_task)),
            block_accumulator_sync_task: Arc::new(Mutex::new(block_accumulator_sync_task)),
            connect_address: address,
        });
        StateSyncTaskRef { address }
    }

    fn sync_end(&self) -> bool {
        info!(
            "state sync task info : {:?},\
             txn accumulator sync task info : {:?},\
             block accumulator sync task info : {:?}.",
            self.state_sync_task.lock().task_info(),
            self.txn_accumulator_sync_task.lock().task_info(),
            self.block_accumulator_sync_task.lock().task_info(),
        );
        self.state_sync_task.lock().is_empty()
            && self.txn_accumulator_sync_task.lock().is_empty()
            && self.block_accumulator_sync_task.lock().is_empty()
    }

    fn exe_state_sync_task(&mut self, address: Addr<StateSyncTaskActor<C>>) {
        let mut lock = self.state_sync_task.lock();
        let value = lock.pop_front();
        if let Some((node_key, is_global)) = value {
            SYNC_METRICS
                .sync_total_count
                .with_label_values(&[LABEL_STATE])
                .inc();
            if let Ok(Some(state_node)) = self.storage.get(&node_key) {
                debug!("find state_node {:?} in db.", node_key);
                lock.insert(self.self_peer_id.clone(), (node_key, is_global));
                if let Err(err) = address.try_send(StateSyncTaskEvent::new_state(
                    self.self_peer_id.clone(),
                    node_key,
                    Some(state_node),
                )) {
                    error!("Send state StateSyncTaskEvent failed : {:?}", err);
                };
            } else {
                let best_peer_info = get_best_peer_info(self.network_service.clone());
                debug!(
                    "sync state node {:?} from peer {:?}.",
                    node_key, best_peer_info
                );
                if let Some(best_peer) = best_peer_info {
                    if self.self_peer_id != best_peer.get_peer_id() {
                        let network_service = self.network_service.clone();
                        lock.insert(best_peer.get_peer_id(), (node_key, is_global));
                        Arbiter::spawn(async move {
                            sync_state_node(
                                node_key,
                                best_peer.get_peer_id(),
                                network_service,
                                address,
                            )
                            .await;
                        });
                    }
                } else {
                    warn!("{:?}", "best peer is none, state sync may be failed.");
                    self.sync_metadata.update_failed(true);
                }
            }
        }
    }

    fn handle_state_sync(&mut self, task_event: StateSyncTaskEvent) {
        let mut lock = self.state_sync_task.lock();
        if let Some((state_node_hash, is_global)) = lock.get(&task_event.peer_id) {
            let is_global = *is_global;
            //1. push back
            let current_node_key = task_event.node_key;
            if state_node_hash != &current_node_key {
                debug!(
                    "hash miss match {:} : {:?}",
                    state_node_hash, current_node_key
                );
                return;
            }
            let _ = lock.remove(&task_event.peer_id);
            if let Some(state_node) = task_event.state_node {
                if let Err(e) = self.storage.put(current_node_key, state_node.clone()) {
                    debug!("{:?}, retry {:?}.", e, current_node_key);
                    lock.push_back((current_node_key, is_global));
                } else {
                    lock.do_one_task();
                    match state_node.inner() {
                        Node::Leaf(leaf) => {
                            if !is_global {
                                return;
                            }
                            match AccountState::try_from(leaf.blob().as_ref()) {
                                Err(e) => {
                                    error!("AccountState decode from blob failed : {:?}", e);
                                }
                                Ok(account_state) => {
                                    account_state.storage_roots().iter().for_each(|key| {
                                        if let Some(hash) = key {
                                            if *hash != *SPARSE_MERKLE_PLACEHOLDER_HASH {
                                                lock.push_back((*hash, false));
                                            }
                                        }
                                    });
                                }
                            }
                        }
                        Node::Internal(n) => {
                            for child in n.all_child() {
                                lock.push_back((child, is_global));
                            }
                        }
                        _ => {
                            debug!("node {:?} is null.", current_node_key);
                        }
                    }
                }
            } else {
                lock.push_back((current_node_key, is_global));
            }
        } else {
            debug!("discard state event : {:?}", task_event);
        }
    }

    fn exe_accumulator_sync_task(
        &self,
        address: Addr<StateSyncTaskActor<C>>,
        accumulator_type: AccumulatorStoreType,
    ) {
        Self::exe_accumulator_sync_task_inner(
            self.self_peer_id.clone(),
            self.storage.clone(),
            self.network_service.clone(),
            self.sync_metadata.clone(),
            match accumulator_type {
                AccumulatorStoreType::Transaction => self.txn_accumulator_sync_task.clone(),
                AccumulatorStoreType::Block => self.block_accumulator_sync_task.clone(),
            },
            address,
            accumulator_type,
        );
    }

    fn exe_accumulator_sync_task_inner(
        self_peer_id: PeerId,
        storage: Arc<dyn Store>,
        network_service: NetworkAsyncService,
        sync_metadata: SyncMetadata,
        accumulator_sync_task: Arc<Mutex<SyncTask<HashValue>>>,
        address: Addr<StateSyncTaskActor<C>>,
        accumulator_type: AccumulatorStoreType,
    ) {
        let mut lock = accumulator_sync_task.lock();
        let value = lock.pop_front();
        if let Some(node_key) = value {
            SYNC_METRICS
                .sync_total_count
                .with_label_values(&[LABEL_ACCUMULATOR])
                .inc();
            if let Ok(Some(accumulator_node)) = storage.get_node(accumulator_type.clone(), node_key)
            {
                debug!("find accumulator_node {:?} in db.", node_key);
                lock.insert(self_peer_id.clone(), node_key);
                if let Err(err) = address.try_send(StateSyncTaskEvent::new_accumulator(
                    self_peer_id,
                    node_key,
                    Some(accumulator_node),
                    accumulator_type,
                )) {
                    error!("Send accumulator StateSyncTaskEvent failed : {:?}", err);
                };
            } else {
                let best_peer_info = get_best_peer_info(network_service.clone());
                debug!(
                    "sync accumulator node {:?} from peer {:?}.",
                    node_key, best_peer_info
                );
                if let Some(best_peer) = best_peer_info {
                    if self_peer_id != best_peer.get_peer_id() {
                        lock.insert(best_peer.get_peer_id(), node_key);
                        Arbiter::spawn(async move {
                            sync_accumulator_node(
                                node_key,
                                best_peer.get_peer_id(),
                                network_service,
                                address,
                                accumulator_type,
                            )
                            .await;
                        });
                    }
                } else {
                    warn!("{:?}", "best peer is none.");
                    sync_metadata.update_failed(true);
                }
            }
        }
    }

    fn handle_accumulator_sync(&mut self, task_event: StateSyncTaskEvent) {
        Self::handle_accumulator_sync_inner(
            self.storage.clone(),
            match task_event.task_type {
                TaskType::TxnAccumulator => self.txn_accumulator_sync_task.clone(),
                _ => self.block_accumulator_sync_task.clone(),
            },
            task_event,
        );
    }
    fn handle_accumulator_sync_inner(
        storage: Arc<dyn Store>,
        accumulator_sync_task: Arc<Mutex<SyncTask<HashValue>>>,
        task_event: StateSyncTaskEvent,
    ) {
        let mut lock = accumulator_sync_task.lock();
        if let Some(accumulator_node_hash) = lock.get(&task_event.peer_id) {
            //1. push back
            let current_node_key = task_event.node_key;
            if accumulator_node_hash != &current_node_key {
                warn!(
                    "hash miss match {:} : {:?}",
                    accumulator_node_hash, current_node_key
                );
                return;
            }
            let _ = lock.remove(&task_event.peer_id);
            if let Some(accumulator_node) = task_event.accumulator_node {
                if let Err(e) = storage.save_node(
                    match task_event.task_type {
                        TaskType::TxnAccumulator => AccumulatorStoreType::Transaction,
                        _ => AccumulatorStoreType::Block,
                    },
                    accumulator_node.clone(),
                ) {
                    debug!("{:?}", e);
                    lock.push_back(current_node_key);
                } else {
                    debug!("receive accumulator_node: {:?}", accumulator_node);
                    lock.do_one_task();
                    match accumulator_node {
                        AccumulatorNode::Leaf(_leaf) => {}
                        AccumulatorNode::Internal(n) => {
                            if n.left() != *ACCUMULATOR_PLACEHOLDER_HASH {
                                lock.push_back(n.left());
                            }
                            if n.right() != *ACCUMULATOR_PLACEHOLDER_HASH {
                                lock.push_back(n.right());
                            }
                        }
                        _ => {
                            debug!("node {:?} is null.", current_node_key);
                        }
                    }
                }
            } else {
                lock.push_back(current_node_key);
            }
        } else {
            debug!("discard state event : {:?}", task_event);
        }
    }

    pub fn reset(
        &mut self,
        state_root: &HashValue,
        txn_accumulator_root: &HashValue,
        block_accumulator_root: &HashValue,
        address: Addr<StateSyncTaskActor<C>>,
    ) {
        debug!("reset state sync task with state root : {:?}, txn accumulator root : {:?}, block accumulator root : {:?}.",
               state_root, txn_accumulator_root, block_accumulator_root);
        self.roots = Roots::new(*state_root, *txn_accumulator_root, *block_accumulator_root);

        let mut state_lock = self.state_sync_task.lock();
        let old_state_is_empty = state_lock.is_empty();
        state_lock.clear();
        state_lock.push_back((*self.roots.state_root(), true));
        drop(state_lock);
        let mut txn_accumulator_lock = self.txn_accumulator_sync_task.lock();
        let old_txn_accumulator_is_empty = txn_accumulator_lock.is_empty();
        txn_accumulator_lock.clear();
        txn_accumulator_lock.push_back(*self.roots.txn_accumulator_root());
        drop(txn_accumulator_lock);
        let mut block_accumulator_lock = self.block_accumulator_sync_task.lock();
        let old_block_accumulator_is_empty = block_accumulator_lock.is_empty();
        block_accumulator_lock.clear();
        block_accumulator_lock.push_back(*self.roots.block_accumulator_root());
        drop(block_accumulator_lock);
        if self.sync_metadata.is_failed() {
            self.activation_task(address);
        } else {
            if old_state_is_empty {
                self.exe_state_sync_task(address.clone());
            }
            if old_txn_accumulator_is_empty {
                self.exe_accumulator_sync_task(address.clone(), AccumulatorStoreType::Transaction);
            }
            if old_block_accumulator_is_empty {
                self.exe_accumulator_sync_task(address, AccumulatorStoreType::Block);
            }
        }
    }

    fn activation_task(&mut self, address: Addr<StateSyncTaskActor<C>>) {
        debug!("activation state sync task.");
        if self.sync_metadata.is_failed() {
            self.sync_metadata.update_failed(false);
            self.exe_state_sync_task(address.clone());
            self.exe_accumulator_sync_task(address.clone(), AccumulatorStoreType::Transaction);
            self.exe_accumulator_sync_task(address, AccumulatorStoreType::Block);
        }
    }
}

impl<C> Actor for StateSyncTaskActor<C>
where
    C: Consensus + Sync + Send + 'static + Clone,
{
    type Context = Context<Self>;

    fn started(&mut self, ctx: &mut Self::Context) {
        self.exe_state_sync_task(ctx.address());
        self.exe_accumulator_sync_task(ctx.address(), AccumulatorStoreType::Transaction);
        self.exe_accumulator_sync_task(ctx.address(), AccumulatorStoreType::Block);
    }
}

impl<C> Handler<StateSyncTaskEvent> for StateSyncTaskActor<C>
where
    C: Consensus + Sync + Send + 'static + Clone,
{
    type Result = Result<()>;

    fn handle(&mut self, task_event: StateSyncTaskEvent, ctx: &mut Self::Context) -> Self::Result {
        let task_type = task_event.task_type.clone();
        match task_type.clone() {
            TaskType::STATE => self.handle_state_sync(task_event),
            _ => self.handle_accumulator_sync(task_event),
        }

        if self.sync_end() {
            if let Some((block, block_info)) = self.sync_metadata.get_pivot_block() {
                self.connect_address
                    .do_send(SyncEvent::DoPivot(Box::new(block), Box::new(block_info)));
            }

            if let Err(e) = self.sync_metadata.state_sync_done() {
                error!("update state_sync_done in sync_metadata failed : {:?}", e);
            } else {
                ctx.stop();
            }
        } else {
            match task_type {
                TaskType::STATE => self.exe_state_sync_task(ctx.address()),
                TaskType::TxnAccumulator => {
                    self.exe_accumulator_sync_task(ctx.address(), AccumulatorStoreType::Transaction)
                }
                TaskType::BlockAccumulator => {
                    self.exe_accumulator_sync_task(ctx.address(), AccumulatorStoreType::Block)
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Message)]
#[rtype(result = "Result<()>")]
enum StateSyncEvent {
    RESET(RestRoots),
    ACT,
}

#[derive(Debug, Clone)]
struct RestRoots {
    state_root: HashValue,
    txn_accumulator_root: HashValue,
    block_accumulator_root: HashValue,
}

impl<C> Handler<StateSyncEvent> for StateSyncTaskActor<C>
where
    C: Consensus + Sync + Send + 'static + Clone,
{
    type Result = Result<()>;

    /// This method is called for every message received by this actor.
    fn handle(&mut self, msg: StateSyncEvent, ctx: &mut Self::Context) -> Self::Result {
        match msg {
            StateSyncEvent::ACT => self.activation_task(ctx.address()),
            StateSyncEvent::RESET(roots) => {
                self.reset(
                    &roots.state_root,
                    &roots.txn_accumulator_root,
                    &roots.block_accumulator_root,
                    ctx.address(),
                );
            }
        }
        Ok(())
    }
}

fn get_best_peer_info(network_service: NetworkAsyncService) -> Option<PeerInfo> {
    block_on(async move {
        if let Ok(peer_info) = network_service.best_peer().await {
            peer_info
        } else {
            None
        }
    })
}
