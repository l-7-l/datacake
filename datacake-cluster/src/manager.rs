use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use chitchat::transport::UdpTransport;
use chitchat::FailureDetectorConfig;
use datacake_crdt::get_unix_timestamp_ms;
use futures::channel::oneshot;
use futures::StreamExt;
use tokio::sync::Semaphore;
use tokio::time::{Instant, interval, MissedTickBehavior};
use tokio_stream::wrappers::WatchStream;

use crate::node::{ClusterMember, DatacakeNode};
use crate::NUMBER_OF_SHARDS;
use crate::rpc::{server, Client, ClientCluster, DataHandler};
use crate::shard::{self, ShardGroupHandle, StateChangeTs};
use crate::shard::state::StateWatcherHandle;

const CHANGES_POLLING_DURATION: Duration = Duration::from_millis(500);

/// All network related configs for both gossip and RPC.
pub struct ConnectionCfg {
    /// The address that other nodes can use to connect
    /// to the node's gossip communication.
    pub gossip_public_addr: SocketAddr,

    /// The listen address the node should use for the gossip address.
    ///
    /// Normally this is something along the lines of: `0.0.0.0:9999` or `127.0.0.1:9999`.
    pub gossip_listen_addr: SocketAddr,

    /// The address that other nodes can use to connect
    /// to the node's RPC communication.
    pub rpc_public_addr: SocketAddr,

    /// The listen address the node should use for the gossip address.
    ///
    /// Normally this is something along the lines of: `0.0.0.0:9999` or `127.0.0.1:9999`.
    ///
    /// NOTE:
    ///  This cannot be the same address as the gossip listen address.
    pub rpc_listen_addr: SocketAddr,
}

pub struct DatacakeClusterManager {
    rpc_server_shutdown: oneshot::Sender<()>,
    rpc_clients: ClientCluster,
    node: DatacakeNode,
}

impl DatacakeClusterManager {
    pub async fn connect(
        node_id: String,
        connection_cfg: ConnectionCfg,
        cluster_id: String,
        seed_nodes: Vec<String>,
        data_handler: Arc<dyn DataHandler>,
        shard_group: ShardGroupHandle,
        shard_changes_watcher: StateWatcherHandle,
    ) -> Result<Self> {
        info!(
            cluster_id = %cluster_id,
            node_id = %node_id,
            peer_seed_addrs = %seed_nodes.join(", "),
            num_shards = %shard::NUMBER_OF_SHARDS,
            "Starting Datacake cluster."
        );

        let rpc_clients = ClientCluster::default();
        let rpc_server_shutdown = server::start_rpc_server(
            shard_group.clone(),
            shard_changes_watcher,
            data_handler.clone(),
            connection_cfg.rpc_listen_addr,
        )
        .await;

        let me = ClusterMember::new(
            node_id,
            get_unix_timestamp_ms(),
            connection_cfg.rpc_public_addr,
            connection_cfg.gossip_public_addr,
        );

        let node_id = me.node_id.clone();
        let node = DatacakeNode::connect(
            me,
            connection_cfg.gossip_listen_addr,
            cluster_id,
            seed_nodes,
            FailureDetectorConfig::default(),
            &UdpTransport,
        )
        .await?;

        let watcher = node.member_change_watcher();
        tokio::spawn(watch_for_remote_state_changes(
            node_id,
            watcher,
            rpc_clients.clone(),
            data_handler,
            shard_group,
        ));

        Ok(Self {
            rpc_server_shutdown,
            rpc_clients,
            node,
        })
    }

    #[inline]
    pub fn rpc_nodes(&self) -> &ClientCluster {
        &self.rpc_clients
    }

    pub async fn shutdown(self) -> Result<()> {
        self.node.shutdown().await;
        let _ = self.rpc_server_shutdown.send(());
        Ok(())
    }
}


/// Watches any member state changes from the ChitChat cluster.
///
/// * The system first checks for any new member joins and disconnects
///   and attempts to establish the RPC connection.
///
/// * The previous known state of the shard is checked to see if any of it's
///   shards have changed. If they have, the synchronisation process is triggered,
///   otherwise the member is ignored.
async fn watch_for_remote_state_changes(
    self_node_id: String,
    mut changes: WatchStream<Vec<ClusterMember>>,
    rpc_clients: ClientCluster,
    data_handler: Arc<dyn DataHandler>,
    shard_group: ShardGroupHandle,
) {
    let mut shard_states = HashMap::<String, SocketAddr>::new();
    while let Some(members) = changes.next().await {
        info!(
            node_id = %self_node_id,
            num_members = %members.len(),
            "Member states have changed! Checking for new and dead members.",
        );

        // Make sure our remote nodes are handled.
        let iterator = members
            .iter()
            .filter(|member| member.node_id != self_node_id)
            .map(|member| (member.node_id.clone(), member.public_rpc_addr));


        let errors = rpc_clients.adjust_connected_clients(iterator).await;
        for (node_id, error) in errors {
            error!(
                node_id = %self_node_id,
                target_node_id = %node_id,
                error = ?error,
                "Failed to connect to remote node member.",
            );
        }

        for member in members.into_iter().filter(|member| member.node_id != self_node_id) {
            if let Some(previous_addr) = shard_states.get(&member.node_id) {
                if previous_addr == &member.public_rpc_addr {
                    trace!(
                        node_id = %self_node_id,
                        target_node_id = %member.node_id,
                        rpc_addr = %member.public_rpc_addr,
                        "Ignoring member setup, node already online.",
                    );
                    continue;
                }
            }

            let client = match rpc_clients.get_client(&member.node_id) {
                None => {
                    warn!(
                        node_id = %self_node_id,
                        target_node_id = %member.node_id,
                        "Potential logical error, node RPC clients should be connected before \
                        reaching this point unless a node has failed to establish a connection."
                    );
                    continue;
                },
                Some(client) => client,
            };

            shard_states.insert(member.node_id.clone(), member.public_rpc_addr);

            info!(
                node_id = %self_node_id,
                target_node_id = %member.node_id,
                rpc_addr = %member.public_rpc_addr,
                "Starting changes poller for node.",
            );

            tokio::spawn(spawn_shard_state_poller(
                member,
                client,
                data_handler.clone(),
                shard_group.clone(),
            ));

        }
    }
}


/// A polling task that check the remote node's shard changes
/// every given period of time.
async fn spawn_shard_state_poller(
    member: ClusterMember,
    node_rpc: Client,
    data_handler: Arc<dyn DataHandler>,
    shard_group: ShardGroupHandle
) {
    let mut interval = interval(CHANGES_POLLING_DURATION);
    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);

    let mut previous_state = vec![0; NUMBER_OF_SHARDS];
    loop {
        interval.tick().await;

        let state = match node_rpc.sync.get_shard_changes().await {
            Err(e) => {
                error!(target_node_id = %member.node_id, error = %e, "Poller shutting down for node.");
                break;
            },
            Ok(state) => state,
        };

        let result = handle_node_state_change(
            &member,
            state,
            &mut previous_state,
            &node_rpc,
            &data_handler,
            &shard_group,
        )
        .await;

        if let Err(e) = result {
            error!(
                target_node_id = %member.node_id,
                error = ?e,
                "Failed to process remote node state changes.",
            );
        }
    }
}

/// Calculated what shards have changed for a given node's state
/// and spawns a handler task for each shard that has changed.
///
/// In the case that the shard's change timestamp is `0` (Initial startup state)
/// then the shard is always marked as changed and follows the synchronisation
/// process, regardless of if the local node's state is aligned already.
async fn handle_node_state_change(
    member: &ClusterMember,
    new_state: Vec<StateChangeTs>,
    previous_state: &mut [StateChangeTs],
    node_rpc: &Client,
    data_handler: &Arc<dyn DataHandler>,
    shard_group: &ShardGroupHandle,
) -> Result<()> {
    let shard_changes = new_state.iter()
        .zip(previous_state.iter())
        .enumerate();

    let (tx, rx) = flume::bounded(2);
    let concurrency_limiter = Arc::new(Semaphore::new(1));
    for (shard_id, (&new, &old)) in shard_changes {
        // If the shard state hasn't changed don't bother trying to sync it.
        // `0` is reserved just for initial states. If a state is `0` then we must
        // request a re-sync to make sure we haven't missed updates.
        if new == old && !(new == 0 || old == 0) {
            continue;
        }

        debug!(
            target_node_id = %member.node_id,
            shard_id = shard_id,
            "Shard is behind for remote node. Fetching updates.",
        );

        let tx = tx.clone();
        let concurrency_limiter = concurrency_limiter.clone();
        let node_id = member.node_id.clone();
        let rpc = node_rpc.clone();
        let data_handler = data_handler.clone();
        let shard_group = shard_group.clone();
        tokio::spawn(async move {
            let _permit = concurrency_limiter.acquire().await;

            let fut =
                handle_shard_change(&node_id, shard_id, rpc, data_handler, shard_group);

            if let Err(e) = fut.await {
                error!(
                    node_id = %node_id,
                    target_shard_id = %shard_id,
                    error = ?e,
                    "Failed to handle shard state change due to error.",
                );
            } else {
                let _ = tx.send_async((shard_id, new)).await;
            };
        });
    }

    while let Ok((shard_id, aligned_ts)) = rx.recv_async().await {
        previous_state[shard_id] = aligned_ts;
    }

    Ok(())
}

/// Handles a given node's state shard changing.
///
/// * This works by first getting the shard's doc set which can then have the
///   deterministic difference calculated between the two sets.
///
/// * The node spawns a task to mark the deleted documents as tombstones.
///
/// * The node fetches and streams the updated/inserted documents from the remote node,
///   feeding them into the local node's data handler.
///
/// * The remote set is merged into the current set and any observed deletes are purged
///   from the set.
///
/// * Purged deletes are then cleared completely including removing the tombstone markers
///   for that given document.
async fn handle_shard_change(
    node_id: &str,
    shard_id: usize,
    rpc: Client,
    data_handler: Arc<dyn DataHandler>,
    shard_group: ShardGroupHandle,
) -> Result<()> {
    let state = rpc.sync.get_doc_set(shard_id).await?;
    let (updated, removed) = shard_group.diff(shard_id, state.clone()).await?;

    if updated.is_empty() && removed.is_empty() {
        return Ok(())
    }

    let num_updates = updated.len();
    let num_removed = removed.len();

    let start = Instant::now();
    let handler = data_handler.clone();
    let delete_task = tokio::spawn(async move {
            if removed.is_empty() {
                Ok(())
            } else {
                handler.mark_tombstone_documents(removed).await
            }
        });

    if !updated.is_empty() {
        let mut stream = rpc
            .sync
            .fetch_docs(updated.iter().map(|v| v.0).collect())
            .await?;

        while let Some(docs) = stream.next().await {
            data_handler.upsert_documents(Vec::from_iter(docs?)).await?;
        }
    }

    delete_task.await??;
    debug!(
        target_node_id = %node_id,
        target_shard_id = %shard_id,
        num_updated = num_updates,
        num_removed = num_removed,
        processing_time = ?start.elapsed(),
        "Deleted documents and updates synchronised successfully.",
    );

    let purged_keys = shard_group.merge(shard_id, state).await?;
    let num_purged = purged_keys.len();
    data_handler.clear_tombstone_documents(purged_keys).await?;

    debug!(
        target_node_id = %node_id,
        target_shard_id = %shard_id,
        num_purged = num_purged,
        "Purged observed deletes from set.",
    );

    Ok(())
}