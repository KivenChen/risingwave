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

use std::collections::HashSet;
use std::sync::Arc;

use risingwave_common::catalog::TableId;
use risingwave_pb::hummock::hummock_manager_service_server::HummockManagerService;
use risingwave_pb::hummock::*;
use tonic::{Request, Response, Status};

use crate::error::meta_error_to_tonic;
use crate::hummock::compaction::ManualCompactionOption;
use crate::hummock::compaction_group::manager::CompactionGroupManagerRef;
use crate::hummock::{CompactorManagerRef, HummockManagerRef, VacuumTrigger};
use crate::manager::FragmentManagerRef;
use crate::rpc::service::RwReceiverStream;
use crate::storage::MetaStore;

pub struct HummockServiceImpl<S>
where
    S: MetaStore,
{
    hummock_manager: HummockManagerRef<S>,
    compactor_manager: CompactorManagerRef,
    vacuum_trigger: Arc<VacuumTrigger<S>>,
    compaction_group_manager: CompactionGroupManagerRef<S>,
    fragment_manager: FragmentManagerRef<S>,
}

impl<S> HummockServiceImpl<S>
where
    S: MetaStore,
{
    pub fn new(
        hummock_manager: HummockManagerRef<S>,
        compactor_manager: CompactorManagerRef,
        vacuum_trigger: Arc<VacuumTrigger<S>>,
        compaction_group_manager: CompactionGroupManagerRef<S>,
        fragment_manager: FragmentManagerRef<S>,
    ) -> Self {
        HummockServiceImpl {
            hummock_manager,
            compactor_manager,
            vacuum_trigger,
            compaction_group_manager,
            fragment_manager,
        }
    }
}

#[async_trait::async_trait]
impl<S> HummockManagerService for HummockServiceImpl<S>
where
    S: MetaStore,
{
    type SubscribeCompactTasksStream = RwReceiverStream<SubscribeCompactTasksResponse>;

    async fn pin_version(
        &self,
        request: Request<PinVersionRequest>,
    ) -> Result<Response<PinVersionResponse>, Status> {
        let req = request.into_inner();
        let (is_delta_response, version_deltas, pinned_version) = self
            .hummock_manager
            .pin_version(req.context_id, req.last_pinned)
            .await
            .map_err(meta_error_to_tonic)?;
        Ok(Response::new(PinVersionResponse {
            status: None,
            is_delta_response,
            version_deltas,
            pinned_version,
        }))
    }

    async fn unpin_version(
        &self,
        request: Request<UnpinVersionRequest>,
    ) -> Result<Response<UnpinVersionResponse>, Status> {
        let req = request.into_inner();
        self.hummock_manager
            .unpin_version(req.context_id)
            .await
            .map_err(meta_error_to_tonic)?;
        Ok(Response::new(UnpinVersionResponse { status: None }))
    }

    async fn unpin_version_before(
        &self,
        request: Request<UnpinVersionBeforeRequest>,
    ) -> Result<Response<UnpinVersionBeforeResponse>, Status> {
        let req = request.into_inner();
        self.hummock_manager
            .unpin_version_before(req.context_id, req.unpin_version_before)
            .await
            .map_err(meta_error_to_tonic)?;
        Ok(Response::new(UnpinVersionBeforeResponse { status: None }))
    }

    async fn report_compaction_tasks(
        &self,
        request: Request<ReportCompactionTasksRequest>,
    ) -> Result<Response<ReportCompactionTasksResponse>, Status> {
        let req = request.into_inner();
        match req.compact_task {
            None => Ok(Response::new(ReportCompactionTasksResponse {
                status: None,
            })),
            Some(compact_task) => {
                self.hummock_manager
                    .report_compact_task(req.context_id, &compact_task)
                    .await
                    .map_err(meta_error_to_tonic)?;
                Ok(Response::new(ReportCompactionTasksResponse {
                    status: None,
                }))
            }
        }
    }

    async fn pin_snapshot(
        &self,
        request: Request<PinSnapshotRequest>,
    ) -> Result<Response<PinSnapshotResponse>, Status> {
        let req = request.into_inner();
        let hummock_snapshot = self
            .hummock_manager
            .pin_snapshot(req.context_id)
            .await
            .map_err(meta_error_to_tonic)?;
        Ok(Response::new(PinSnapshotResponse {
            status: None,
            snapshot: Some(hummock_snapshot),
        }))
    }

    async fn unpin_snapshot(
        &self,
        request: Request<UnpinSnapshotRequest>,
    ) -> Result<Response<UnpinSnapshotResponse>, Status> {
        let req = request.into_inner();
        self.hummock_manager
            .unpin_snapshot(req.context_id)
            .await
            .map_err(meta_error_to_tonic)?;
        Ok(Response::new(UnpinSnapshotResponse { status: None }))
    }

    async fn unpin_snapshot_before(
        &self,
        request: Request<UnpinSnapshotBeforeRequest>,
    ) -> Result<Response<UnpinSnapshotBeforeResponse>, Status> {
        let req = request.into_inner();
        self.hummock_manager
            .unpin_snapshot_before(req.context_id, req.min_snapshot.unwrap())
            .await
            .map_err(meta_error_to_tonic)?;
        Ok(Response::new(UnpinSnapshotBeforeResponse { status: None }))
    }

    async fn get_new_sst_ids(
        &self,
        request: Request<GetNewSstIdsRequest>,
    ) -> Result<Response<GetNewSstIdsResponse>, Status> {
        let sst_id_range = self
            .hummock_manager
            .get_new_sst_ids(request.into_inner().number)
            .await
            .map_err(meta_error_to_tonic)?;
        Ok(Response::new(GetNewSstIdsResponse {
            status: None,
            start_id: sst_id_range.start_id,
            end_id: sst_id_range.end_id,
        }))
    }

    async fn subscribe_compact_tasks(
        &self,
        request: Request<SubscribeCompactTasksRequest>,
    ) -> Result<Response<Self::SubscribeCompactTasksStream>, Status> {
        let context_id = request.into_inner().context_id;
        // check_context and add_compactor as a whole is not atomic, but compactor_manager will
        // remove invalid compactor eventually.
        if !self.hummock_manager.check_context(context_id).await {
            return Err(anyhow::anyhow!("invalid hummock context {}", context_id))
                .map_err(meta_error_to_tonic);
        }
        let rx = self.compactor_manager.add_compactor(context_id);
        Ok(Response::new(RwReceiverStream::new(rx)))
    }

    async fn report_vacuum_task(
        &self,
        request: Request<ReportVacuumTaskRequest>,
    ) -> Result<Response<ReportVacuumTaskResponse>, Status> {
        if let Some(vacuum_task) = request.into_inner().vacuum_task {
            self.vacuum_trigger
                .report_vacuum_task(vacuum_task)
                .await
                .map_err(meta_error_to_tonic)?;
        }
        Ok(Response::new(ReportVacuumTaskResponse { status: None }))
    }

    async fn get_compaction_groups(
        &self,
        _request: Request<GetCompactionGroupsRequest>,
    ) -> Result<Response<GetCompactionGroupsResponse>, Status> {
        let resp = GetCompactionGroupsResponse {
            status: None,
            compaction_groups: self
                .compaction_group_manager
                .compaction_groups()
                .await
                .iter()
                .map(|cg| cg.into())
                .collect(),
        };
        Ok(Response::new(resp))
    }

    async fn trigger_manual_compaction(
        &self,
        request: Request<TriggerManualCompactionRequest>,
    ) -> Result<Response<TriggerManualCompactionResponse>, Status> {
        let request = request.into_inner();
        let compaction_group_id = request.compaction_group_id;
        let mut option = ManualCompactionOption {
            level: request.level as usize,
            sst_ids: request.sst_ids,
            ..Default::default()
        };

        // rewrite the key_range
        match request.key_range {
            Some(key_range) => {
                option.key_range = key_range;
            }

            None => {
                option.key_range = KeyRange {
                    inf: true,
                    ..Default::default()
                }
            }
        }

        // get internal_table_id by fragment_manager
        let table_id = TableId::new(request.table_id);
        if let Ok(table_frgament) = self
            .fragment_manager
            .select_table_fragments_by_table_id(&table_id)
            .await
        {
            option.internal_table_id = HashSet::from_iter(table_frgament.internal_table_ids());
        }
        option.internal_table_id.insert(request.table_id); // need to handle outter table_id (mv)

        tracing::info!(
            "Try trigger_manual_compaction compaction_group_id {} option {:?}",
            compaction_group_id,
            &option
        );

        self.hummock_manager
            .trigger_manual_compaction(compaction_group_id, option)
            .await
            .map_err(meta_error_to_tonic)?;

        Ok(Response::new(TriggerManualCompactionResponse {
            status: None,
        }))
    }

    async fn get_epoch(
        &self,
        _request: Request<GetEpochRequest>,
    ) -> Result<Response<GetEpochResponse>, Status> {
        let hummock_snapshot = self
            .hummock_manager
            .get_last_epoch()
            .map_err(meta_error_to_tonic)?;
        Ok(Response::new(GetEpochResponse {
            status: None,
            snapshot: Some(hummock_snapshot),
        }))
    }

    async fn report_full_scan_task(
        &self,
        request: Request<ReportFullScanTaskRequest>,
    ) -> Result<Response<ReportFullScanTaskResponse>, Status> {
        self.hummock_manager
            .extend_ssts_to_delete_from_scan(&request.into_inner().sst_ids)
            .await;
        Ok(Response::new(ReportFullScanTaskResponse { status: None }))
    }
}
