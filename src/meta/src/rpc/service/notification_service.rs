// Copyright 2022 Singularity Data
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use risingwave_pb::common::worker_node::State::Running;
use risingwave_pb::common::WorkerType;
use risingwave_pb::meta::notification_service_server::NotificationService;
use risingwave_pb::meta::subscribe_response::{Info, Operation};
use risingwave_pb::meta::{MetaSnapshot, SubscribeRequest, SubscribeResponse};
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tonic::{Request, Response, Status};

use crate::error::meta_error_to_tonic;
use crate::hummock::HummockManagerRef;
use crate::manager::{CatalogManagerRef, ClusterManagerRef, MetaSrvEnv, Notification, WorkerKey};
use crate::storage::MetaStore;
use crate::stream::GlobalStreamManagerRef;

pub struct NotificationServiceImpl<S: MetaStore> {
    env: MetaSrvEnv<S>,

    catalog_manager: CatalogManagerRef<S>,
    cluster_manager: ClusterManagerRef<S>,
    hummock_manager: HummockManagerRef<S>,
    stream_manager: GlobalStreamManagerRef<S>,
}

impl<S> NotificationServiceImpl<S>
where
    S: MetaStore,
{
    pub fn new(
        env: MetaSrvEnv<S>,
        catalog_manager: CatalogManagerRef<S>,
        cluster_manager: ClusterManagerRef<S>,
        hummock_manager: HummockManagerRef<S>,
        stream_manager: GlobalStreamManagerRef<S>,
    ) -> Self {
        Self {
            env,
            catalog_manager,
            cluster_manager,
            hummock_manager,
            stream_manager,
        }
    }
}

#[async_trait::async_trait]
impl<S> NotificationService for NotificationServiceImpl<S>
where
    S: MetaStore,
{
    type SubscribeStream = UnboundedReceiverStream<Notification>;

    #[cfg_attr(coverage, no_coverage)]
    async fn subscribe(
        &self,
        request: Request<SubscribeRequest>,
    ) -> Result<Response<Self::SubscribeStream>, Status> {
        let req = request.into_inner();
        let worker_type = req.get_worker_type().map_err(meta_error_to_tonic)?;
        let host_address = req.get_host().map_err(meta_error_to_tonic)?.clone();

        let (tx, rx) = mpsc::unbounded_channel();

        // let meta_snapshot = self.build_snapshot_by_type(worker_type).await?;

        let catalog_guard = self.catalog_manager.get_catalog_core_guard().await;

        let (database, schema, mut table, source, sink, index) =
            catalog_guard.database.get_catalog().await?;

        let users = catalog_guard.user.list_users();

        let cluster_guard = self.cluster_manager.get_cluster_core_guard().await;
        let nodes = cluster_guard.list_worker_node(WorkerType::ComputeNode, Some(Running));

        let hummock_version = Some(self.hummock_manager.get_current_version().await);

        let processing_table_guard = self.stream_manager.get_processing_table_guard().await;

        // Send the snapshot on subscription. After that we will send only updates.
        let meta_snapshot = match worker_type {
            WorkerType::Frontend => MetaSnapshot {
                nodes,
                database,
                schema,
                source,
                sink,
                table,
                users,
                hummock_version: None,
                index,
            },

            WorkerType::Compactor => {
                table.extend(processing_table_guard.values().cloned());

                MetaSnapshot {
                    table,
                    ..Default::default()
                }
            }

            WorkerType::ComputeNode => {
                table.extend(processing_table_guard.values().cloned());

                MetaSnapshot {
                    table,
                    hummock_version,
                    ..Default::default()
                }
            }

            _ => unreachable!(),
        };

        tx.send(Ok(SubscribeResponse {
            status: None,
            operation: Operation::Snapshot as i32,
            info: Some(Info::Snapshot(meta_snapshot)),
            version: self.env.notification_manager().current_version().await,
        }))
        .unwrap();

        self.env
            .notification_manager()
            .insert_sender(worker_type, WorkerKey(host_address), tx)
            .await;

        Ok(Response::new(UnboundedReceiverStream::new(rx)))
    }
}
