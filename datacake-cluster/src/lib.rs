#[macro_use]
extern crate tracing;

mod clock;
mod core;
pub mod error;
mod keyspace;
mod node;
mod nodes_selector;
mod poller;
mod rpc;
mod storage;

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Display;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use chitchat::FailureDetectorConfig;
use datacake_crdt::{get_unix_timestamp_ms, Key};
use futures::stream::FuturesUnordered;
use futures::StreamExt;
#[cfg(feature = "test-utils")]
pub use storage::test_suite;
pub use storage::Storage;
use tokio_stream::wrappers::WatchStream;

use crate::clock::Clock;
use crate::core::Document;
use crate::keyspace::{ConsistencySource, KeyspaceGroup};
use crate::node::{ClusterMember, DatacakeNode};
use crate::nodes_selector::{
    Consistency,
    ConsistencyError,
    NodeSelector,
    NodeSelectorHandle,
};
use crate::poller::ShutdownHandle;
use crate::rpc::{
    ConsistencyClient,
    Context,
    DefaultRegistry,
    GrpcTransport,
    RpcNetwork,
    ServiceRegistry,
    TIMEOUT_LIMIT,
};

pub static DEFAULT_DATA_CENTER: &str = "datacake-dc-unknown";
pub static DEFAULT_CLUSTER_ID: &str = "datacake-cluster-unknown";
const POLLING_INTERVAL_DURATION: Duration = Duration::from_secs(1);

/// Non-required configurations for the datacake cluster node.
pub struct ClusterOptions {
    cluster_id: String,
    data_center: Cow<'static, str>,
}

impl Default for ClusterOptions {
    fn default() -> Self {
        Self {
            cluster_id: DEFAULT_CLUSTER_ID.to_string(),
            data_center: Cow::Borrowed(DEFAULT_DATA_CENTER),
        }
    }
}

impl ClusterOptions {
    /// Set the cluster id for the given node.
    pub fn with_cluster_id(mut self, cluster_id: impl Display) -> Self {
        self.cluster_id = cluster_id.to_string();
        self
    }

    /// Set the data center the node belongs to.
    pub fn with_data_center(mut self, dc: impl Display) -> Self {
        self.data_center = Cow::Owned(dc.to_string());
        self
    }
}

#[derive(Debug, Clone)]
/// Configuration for the cluster network.
pub struct ConnectionConfig {
    /// The binding address for the RPC server to bind and listen on.
    ///
    /// This is often `0.0.0.0` + your chosen port.
    pub listen_addr: SocketAddr,

    /// The public address to be broadcast to other cluster members.
    ///
    /// This is normally the machine's public IP address and the port the server is listening on.
    pub public_addr: SocketAddr,

    /// A set of initial seed nodes which the node will attempt to connect to and learn of any
    /// other members in the cluster.
    ///
    /// Normal `2` or `3` seeds is fine when running a multi-node cluster.
    /// Having only `1` seed can be dangerous if both nodes happen to go down but the seed
    /// does not restart before this node, as it will be unable to re-join the cluster.
    pub seed_nodes: Vec<String>,
}

impl ConnectionConfig {
    /// Creates a new connection config.
    pub fn new(
        listen_addr: SocketAddr,
        public_addr: SocketAddr,
        seeds: impl Into<Vec<String>>,
    ) -> Self {
        Self {
            listen_addr,
            public_addr,
            seed_nodes: seeds.into(),
        }
    }
}

/// A fully managed eventually consistent state controller.
///
/// The [DatacakeCluster] manages all RPC and state propagation for
/// a given application, where the only setup required is the
/// RPC based configuration and the required handler traits
/// which wrap the application itself.
///
/// Datacake essentially acts as a frontend wrapper around a datastore
/// to make is distributed.
pub struct DatacakeCluster<S>
where
    S: Storage + Send + Sync + 'static,
{
    node: DatacakeNode,
    network: RpcNetwork,
    group: KeyspaceGroup<S>,
    clock: Clock,
    node_selector: NodeSelectorHandle,
}

impl<S> DatacakeCluster<S>
where
    S: Storage + Send + Sync + 'static,
{
    /// Starts the Datacake cluster, connecting to the targeted seed nodes.
    ///
    /// When connecting to the cluster, the `node_id` **must be unique** otherwise
    /// the cluster will incorrectly propagate state and not become consistent.
    ///
    /// Typically you will only have one cluster and therefore only have one `cluster_id`
    /// which should be the same for each node in the cluster.
    /// Currently the `cluster_id` is not handled by anything other than
    /// [chitchat](https://docs.rs/chitchat/0.4.1/chitchat/)
    ///
    /// No seed nodes need to be live at the time of connecting for the cluster to start correctly,
    /// but they are required in order for nodes to discover one-another and share
    /// their basic state.
    pub async fn connect<DS>(
        node_id: impl Into<String>,
        connection_cfg: ConnectionConfig,
        datastore: S,
        node_selector: DS,
        options: ClusterOptions,
    ) -> Result<Self, error::DatacakeError<S::Error>>
    where
        DS: NodeSelector + Send + 'static,
    {
        Self::connect_with_registry(
            node_id,
            connection_cfg,
            datastore,
            node_selector,
            DefaultRegistry,
            options,
        )
        .await
    }

    /// Starts the Datacake cluster with a custom service registry, connecting to the targeted seed nodes.
    ///
    /// A custom service registry can be used in order to add additional GRPC services to the
    /// RPC server in order to avoid listening on multiple addresses.
    ///
    /// When connecting to the cluster, the `node_id` **must be unique** otherwise
    /// the cluster will incorrectly propagate state and not become consistent.
    ///
    /// Typically you will only have one cluster and therefore only have one `cluster_id`
    /// which should be the same for each node in the cluster.
    /// Currently the `cluster_id` is not handled by anything other than
    /// [chitchat](https://docs.rs/chitchat/0.4.1/chitchat/)
    ///
    /// No seed nodes need to be live at the time of connecting for the cluster to start correctly,
    /// but they are required in order for nodes to discover one-another and share
    /// their basic state.
    pub async fn connect_with_registry<DS, R>(
        node_id: impl Into<String>,
        connection_cfg: ConnectionConfig,
        datastore: S,
        node_selector: DS,
        service_registry: R,
        options: ClusterOptions,
    ) -> Result<Self, error::DatacakeError<S::Error>>
    where
        DS: NodeSelector + Send + 'static,
        R: ServiceRegistry + Send + Sync + Clone + 'static,
    {
        let node_id = node_id.into();

        let clock = Clock::new(crc32fast::hash(node_id.as_bytes()));
        let storage = Arc::new(datastore);

        let group = KeyspaceGroup::new(storage.clone(), clock.clone()).await;
        let network = RpcNetwork::default();

        // Load the keyspace states.
        group.load_states_from_storage().await?;

        let selector = nodes_selector::start_node_selector(
            connection_cfg.public_addr,
            options.data_center.clone(),
            node_selector,
        )
        .await;

        let cluster_info = ClusterInfo {
            listen_addr: connection_cfg.listen_addr,
            public_addr: connection_cfg.public_addr,
            seed_nodes: connection_cfg.seed_nodes,
            data_center: options.data_center.as_ref(),
        };
        let node = connect_node(
            node_id.clone(),
            options.cluster_id.clone(),
            group.clone(),
            network.clone(),
            cluster_info,
            service_registry,
        )
        .await?;

        setup_poller(group.clone(), network.clone(), &node, selector.clone()).await?;

        info!(
            node_id = %node_id,
            cluster_id = %options.cluster_id,
            listen_addr = %connection_cfg.listen_addr,
            "Datacake cluster connected."
        );

        Ok(Self {
            node,
            network,
            group,
            clock,
            node_selector: selector,
        })
    }

    /// Shuts down the cluster and cleans up any connections.
    pub async fn shutdown(self) {
        self.node.shutdown().await;
    }

    /// Creates a new handle to the underlying storage system.
    ///
    /// Changes applied to the handle are distributed across the cluster.
    pub fn handle(&self) -> DatacakeHandle<S> {
        DatacakeHandle {
            network: self.network.clone(),
            group: self.group.clone(),
            clock: self.clock.clone(),
            node_selector: self.node_selector.clone(),
        }
    }

    /// Creates a new handle to the underlying storage system with a preset keyspace.
    ///
    /// Changes applied to the handle are distributed across the cluster.
    pub fn handle_with_keyspace(
        &self,
        keyspace: impl Into<String>,
    ) -> DatacakeKeyspaceHandle<S> {
        DatacakeKeyspaceHandle {
            inner: self.handle(),
            keyspace: Cow::Owned(keyspace.into()),
        }
    }
}

/// A cheaply cloneable handle to control the data store.
pub struct DatacakeHandle<S>
where
    S: Storage + Send + Sync + 'static,
{
    network: RpcNetwork,
    group: KeyspaceGroup<S>,
    clock: Clock,
    node_selector: NodeSelectorHandle,
}

impl<S> Clone for DatacakeHandle<S>
where
    S: Storage + Send + Sync + 'static,
{
    fn clone(&self) -> Self {
        Self {
            network: self.network.clone(),
            group: self.group.clone(),
            clock: self.clock.clone(),
            node_selector: self.node_selector.clone(),
        }
    }
}

impl<S> DatacakeHandle<S>
where
    S: Storage + Send + Sync + 'static,
{
    /// Creates a new handle to the underlying storage system with a preset keyspace.
    ///
    /// Changes applied to the handle are distributed across the cluster.
    pub fn with_keyspace(
        &self,
        keyspace: impl Into<String>,
    ) -> DatacakeKeyspaceHandle<S> {
        DatacakeKeyspaceHandle {
            inner: self.clone(),
            keyspace: Cow::Owned(keyspace.into()),
        }
    }

    /// Retrieves a document from the underlying storage.
    pub async fn get(
        &self,
        keyspace: &str,
        doc_id: Key,
    ) -> Result<Option<Document>, S::Error> {
        let storage = self.group.storage();
        storage.get(keyspace, doc_id).await
    }

    /// Retrieves a set of documents from the underlying storage.
    ///
    /// If a document does not exist with the given ID, it is simply not part
    /// of the returned iterator.
    pub async fn get_many<I, T>(
        &self,
        keyspace: &str,
        doc_ids: I,
    ) -> Result<S::DocsIter, S::Error>
    where
        T: Iterator<Item = Key> + Send,
        I: IntoIterator<IntoIter = T> + Send,
    {
        let storage = self.group.storage();
        storage.multi_get(keyspace, doc_ids.into_iter()).await
    }

    /// Insert or update a single document into the datastore.
    pub async fn put<D>(
        &self,
        keyspace: &str,
        doc_id: Key,
        data: D,
        consistency: Consistency,
    ) -> Result<(), error::DatacakeError<S::Error>>
    where
        D: Into<Bytes>,
    {
        let nodes = self
            .node_selector
            .get_nodes(consistency)
            .await
            .map_err(error::DatacakeError::ConsistencyError)?;

        let last_updated = self.clock.get_time().await;
        let document = Document::new(doc_id, last_updated, data);

        core::put_data::<ConsistencySource, _>(keyspace, document.clone(), &self.group)
            .await?;

        let factory = |node| {
            let keyspace = keyspace.to_string();
            let document = document.clone();
            async move {
                let channel = self
                    .network
                    .get_or_connect(node)
                    .await
                    .map_err(|e| error::DatacakeError::TransportError(node, e))?;

                let mut client = ConsistencyClient::from(channel);

                client
                    .put(keyspace, document)
                    .await
                    .map_err(|e| error::DatacakeError::RpcError(node, e))?;

                Ok::<_, error::DatacakeError<S::Error>>(())
            }
        };

        handle_consistency_distribution::<S, _, _>(nodes, factory).await
    }

    /// Insert or update multiple documents into the datastore at once.
    pub async fn put_many<I, T, D>(
        &self,
        keyspace: &str,
        documents: I,
        consistency: Consistency,
    ) -> Result<(), error::DatacakeError<S::Error>>
    where
        D: Into<Bytes>,
        T: Iterator<Item = (Key, D)> + Send,
        I: IntoIterator<IntoIter = T> + Send,
    {
        let nodes = self
            .node_selector
            .get_nodes(consistency)
            .await
            .map_err(error::DatacakeError::ConsistencyError)?;

        let last_updated = self.clock.get_time().await;
        let docs = documents
            .into_iter()
            .map(|(id, data)| Document::new(id, last_updated, data))
            .collect::<Vec<_>>();

        core::put_many_data::<ConsistencySource, _>(
            keyspace,
            docs.clone().into_iter(),
            &self.group,
        )
        .await?;

        let factory = |node| {
            let keyspace = keyspace.to_string();
            let documents = docs.clone();
            async move {
                let channel = self
                    .network
                    .get_or_connect(node)
                    .await
                    .map_err(|e| error::DatacakeError::TransportError(node, e))?;

                let mut client = ConsistencyClient::from(channel);

                client
                    .multi_put(keyspace, documents.into_iter())
                    .await
                    .map_err(|e| error::DatacakeError::RpcError(node, e))?;

                Ok::<_, error::DatacakeError<S::Error>>(())
            }
        };

        handle_consistency_distribution::<S, _, _>(nodes, factory).await
    }

    /// Delete a document from the datastore with a given doc ID.
    pub async fn del(
        &self,
        keyspace: &str,
        doc_id: Key,
        consistency: Consistency,
    ) -> Result<(), error::DatacakeError<S::Error>> {
        let nodes = self
            .node_selector
            .get_nodes(consistency)
            .await
            .map_err(error::DatacakeError::ConsistencyError)?;

        let last_updated = self.clock.get_time().await;

        core::del_data::<ConsistencySource, _>(
            keyspace,
            doc_id,
            last_updated,
            &self.group,
        )
        .await?;

        let factory = |node| {
            let keyspace = keyspace.to_string();
            async move {
                let channel = self
                    .network
                    .get_or_connect(node)
                    .await
                    .map_err(|e| error::DatacakeError::TransportError(node, e))?;

                let mut client = ConsistencyClient::from(channel);

                client
                    .del(keyspace, doc_id, last_updated)
                    .await
                    .map_err(|e| error::DatacakeError::RpcError(node, e))?;

                Ok::<_, error::DatacakeError<S::Error>>(())
            }
        };

        handle_consistency_distribution::<S, _, _>(nodes, factory).await
    }

    /// Delete multiple documents from the datastore from the set of doc IDs.
    pub async fn del_many<I, T>(
        &self,
        keyspace: &str,
        doc_ids: I,
        consistency: Consistency,
    ) -> Result<(), error::DatacakeError<S::Error>>
    where
        T: Iterator<Item = Key> + Send,
        I: IntoIterator<IntoIter = T> + Send,
    {
        let nodes = self
            .node_selector
            .get_nodes(consistency)
            .await
            .map_err(error::DatacakeError::ConsistencyError)?;

        let last_updated = self.clock.get_time().await;
        let docs = doc_ids
            .into_iter()
            .map(|id| (id, last_updated))
            .collect::<Vec<_>>();

        core::del_many_data::<ConsistencySource, _>(
            keyspace,
            docs.clone().into_iter(),
            &self.group,
        )
        .await?;

        let factory = |node| {
            let keyspace = keyspace.to_string();
            let docs = docs.clone();
            async move {
                let channel = self
                    .network
                    .get_or_connect(node)
                    .await
                    .map_err(|e| error::DatacakeError::TransportError(node, e))?;

                let mut client = ConsistencyClient::from(channel);

                client
                    .multi_del(keyspace, docs.into_iter())
                    .await
                    .map_err(|e| error::DatacakeError::RpcError(node, e))?;

                Ok::<_, error::DatacakeError<S::Error>>(())
            }
        };

        handle_consistency_distribution::<S, _, _>(nodes, factory).await
    }
}

/// A convenience wrapper which creates a new handle with a preset keyspace.
pub struct DatacakeKeyspaceHandle<S>
where
    S: Storage + Send + Sync + 'static,
{
    inner: DatacakeHandle<S>,
    keyspace: Cow<'static, str>,
}

impl<S> Clone for DatacakeKeyspaceHandle<S>
where
    S: Storage + Send + Sync + 'static,
{
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            keyspace: self.keyspace.clone(),
        }
    }
}

impl<S> DatacakeKeyspaceHandle<S>
where
    S: Storage + Send + Sync + 'static,
{
    /// Retrieves a document from the underlying storage.
    pub async fn get(&self, doc_id: Key) -> Result<Option<Document>, S::Error> {
        self.inner.get(self.keyspace.as_ref(), doc_id).await
    }

    /// Retrieves a set of documents from the underlying storage.
    ///
    /// If a document does not exist with the given ID, it is simply not part
    /// of the returned iterator.
    pub async fn get_many<I, T>(&self, doc_ids: I) -> Result<S::DocsIter, S::Error>
    where
        T: Iterator<Item = Key> + Send,
        I: IntoIterator<IntoIter = T> + Send,
    {
        self.inner.get_many(self.keyspace.as_ref(), doc_ids).await
    }

    /// Insert or update a single document into the datastore.
    pub async fn put(
        &self,
        doc_id: Key,
        data: Vec<u8>,
        consistency: Consistency,
    ) -> Result<(), error::DatacakeError<S::Error>> {
        self.inner
            .put(self.keyspace.as_ref(), doc_id, data, consistency)
            .await
    }

    /// Insert or update multiple documents into the datastore at once.
    pub async fn put_many(
        &self,
        documents: Vec<(Key, Vec<u8>)>,
        consistency: Consistency,
    ) -> Result<(), error::DatacakeError<S::Error>> {
        self.inner
            .put_many(self.keyspace.as_ref(), documents, consistency)
            .await
    }

    /// Delete a document from the datastore with a given doc ID.
    pub async fn del(
        &self,
        doc_id: Key,
        consistency: Consistency,
    ) -> Result<(), error::DatacakeError<S::Error>> {
        self.inner
            .del(self.keyspace.as_ref(), doc_id, consistency)
            .await
    }

    /// Delete multiple documents from the datastore from the set of doc IDs.
    pub async fn del_many(
        &self,
        doc_ids: Vec<Key>,
        consistency: Consistency,
    ) -> Result<(), error::DatacakeError<S::Error>> {
        self.inner
            .del_many(self.keyspace.as_ref(), doc_ids, consistency)
            .await
    }
}

struct ClusterInfo<'a> {
    listen_addr: SocketAddr,
    public_addr: SocketAddr,
    seed_nodes: Vec<String>,
    data_center: &'a str,
}

/// Connects to the chitchat cluster.
///
/// The node will attempt to establish connections to the seed nodes and
/// will broadcast the node's public address to communicate.
async fn connect_node<S, R>(
    node_id: String,
    cluster_id: String,
    group: KeyspaceGroup<S>,
    network: RpcNetwork,
    cluster_info: ClusterInfo<'_>,
    service_registry: R,
) -> Result<DatacakeNode, error::DatacakeError<S::Error>>
where
    S: Storage + Send + Sync + 'static,
    R: ServiceRegistry + Send + Sync + Clone + 'static,
{
    let (chitchat_tx, chitchat_rx) = flume::bounded(1000);
    let context = Context {
        chitchat_messages: chitchat_tx,
        keyspace_group: group,
        service_registry,
    };
    let transport = GrpcTransport::new(network.clone(), context, chitchat_rx);

    let me = ClusterMember::new(
        node_id,
        get_unix_timestamp_ms(),
        cluster_info.public_addr,
        cluster_info.data_center,
    );
    let node = DatacakeNode::connect(
        me,
        cluster_info.listen_addr,
        cluster_id,
        cluster_info.seed_nodes,
        FailureDetectorConfig::default(),
        &transport,
    )
    .await?;

    Ok(node)
}

/// Starts the background task which watches for membership changes
/// intern starting and stopping polling services for each member.
async fn setup_poller<S>(
    keyspace_group: KeyspaceGroup<S>,
    network: RpcNetwork,
    node: &DatacakeNode,
    node_selector: NodeSelectorHandle,
) -> Result<(), error::DatacakeError<S::Error>>
where
    S: Storage + Send + Sync + 'static,
{
    let changes = node.member_change_watcher();
    tokio::spawn(watch_membership_changes(
        keyspace_group,
        network,
        node_selector,
        changes,
    ));
    Ok(())
}

/// Watches for changes in the cluster membership.
///
/// When nodes leave and join, pollers are stopped and started as required.
async fn watch_membership_changes<S>(
    keyspace_group: KeyspaceGroup<S>,
    network: RpcNetwork,
    node_selector: NodeSelectorHandle,
    mut changes: WatchStream<Vec<ClusterMember>>,
) where
    S: Storage + Send + Sync + 'static,
{
    let mut poller_handles = HashMap::<SocketAddr, ShutdownHandle>::new();
    let mut last_network_set = HashSet::new();
    while let Some(members) = changes.next().await {
        let new_network_set = members
            .iter()
            .map(|member| (member.node_id.clone(), member.public_addr))
            .collect::<HashSet<_>>();

        {
            let mut data_centers = BTreeMap::<Cow<'static, str>, Vec<SocketAddr>>::new();
            for member in members.iter() {
                let dc = Cow::Owned(member.data_center.clone());
                data_centers.entry(dc).or_default().push(member.public_addr);
            }

            node_selector.set_nodes(data_centers).await;
        }

        // Remove client no longer apart of the network.
        for (node_id, addr) in last_network_set.difference(&new_network_set) {
            info!(
                target_node_id = %node_id,
                target_addr = %addr,
                "Node is no longer part of cluster."
            );

            network.disconnect(*addr);

            if let Some(handle) = poller_handles.remove(addr) {
                handle.kill();
            }
        }

        // Add new clients for each new node.
        for (node_id, addr) in new_network_set.difference(&last_network_set) {
            info!(
                target_node_id = %node_id,
                target_addr = %addr,
                "Node has connected to the cluster."
            );

            let channel = match network.get_or_connect(*addr).await {
                Ok(channel) => channel,
                Err(e) => {
                    error!(
                        error = ?e,
                        target_node_id = %node_id,
                        target_addr = %addr,
                        "Failed to establish network connection to node despite membership just changing. Is the system configured correctly?"
                    );
                    warn!(
                        target_node_id = %node_id,
                        target_addr = %addr,
                        "Node poller is starting with lazy connection, this may continue to error if a connection cannot be re-established.",
                    );

                    network.connect_lazy(*addr)
                },
            };

            let state = poller::NodePollerState::new(
                Cow::Owned(node_id.clone()),
                *addr,
                keyspace_group.clone(),
                channel,
                POLLING_INTERVAL_DURATION,
            );
            let handle = state.shutdown_handle();
            tokio::spawn(poller::node_poller(state));

            if let Some(handle) = poller_handles.insert(*addr, handle) {
                handle.kill();
            };
        }

        last_network_set = new_network_set;
    }
}

async fn handle_consistency_distribution<S, CB, F>(
    nodes: Vec<SocketAddr>,
    factory: CB,
) -> Result<(), error::DatacakeError<S::Error>>
where
    S: Storage,
    CB: FnMut(SocketAddr) -> F,
    F: Future<Output = Result<(), error::DatacakeError<S::Error>>>,
{
    let mut num_success = 0;
    let num_required = nodes.len();

    let mut requests = nodes
        .into_iter()
        .map(factory)
        .collect::<FuturesUnordered<_>>();

    while let Some(res) = requests.next().await {
        match res {
            Ok(()) => {
                num_success += 1;
            },
            Err(error::DatacakeError::RpcError(node, error)) => {
                error!(
                    error = ?error,
                    target_node = %node,
                    "Replica failed to acknowledge change to meet consistency level requirement."
                );
            },
            Err(error::DatacakeError::TransportError(node, error)) => {
                error!(
                    error = ?error,
                    target_node = %node,
                    "Replica failed to acknowledge change to meet consistency level requirement."
                );
            },
            Err(other) => {
                error!(
                    error = ?other,
                    "Failed to send action to replica due to unknown error.",
                );
            },
        }
    }

    if num_success != num_required {
        Err(error::DatacakeError::ConsistencyError(
            ConsistencyError::ConsistencyFailure {
                responses: num_success,
                required: num_required,
                timeout: TIMEOUT_LIMIT,
            },
        ))
    } else {
        Ok(())
    }
}
