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

use std::collections::BTreeMap;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;

use futures::{FutureExt, StreamExt};
use risingwave_common::config::StorageConfig;
use risingwave_hummock_sdk::{get_local_sst_id, HummockEpoch};
use risingwave_pb::hummock::SstableInfo;
use risingwave_rpc_client::HummockMetaClient;
use tokio::sync::{mpsc, oneshot};
use tracing::error;

use crate::hummock::compaction_executor::CompactionExecutor;
use crate::hummock::compactor::{get_remote_sstable_id_generator, Compactor, CompactorContext};
use crate::hummock::conflict_detector::ConflictDetector;
use crate::hummock::shared_buffer::{OrderIndex, OrderSortedUncommittedData};
use crate::hummock::{HummockError, HummockResult, SstableStoreRef};
use crate::monitor::StateStoreMetrics;

pub(crate) type UploadTaskPayload = OrderSortedUncommittedData;
pub(crate) type UploadTaskResult =
    BTreeMap<(HummockEpoch, OrderIndex), HummockResult<Vec<SstableInfo>>>;

#[derive(Debug)]
pub struct UploadTask {
    pub(crate) order_index: OrderIndex,
    pub(crate) epoch: HummockEpoch,
    pub(crate) payload: UploadTaskPayload,
    pub(crate) is_local: bool,
}

impl UploadTask {
    pub fn new(
        order_index: OrderIndex,
        epoch: HummockEpoch,
        payload: UploadTaskPayload,
        is_local: bool,
    ) -> Self {
        Self {
            order_index,
            epoch,
            payload,
            is_local,
        }
    }
}

pub struct UploadItem {
    pub(crate) tasks: Vec<UploadTask>,
    pub(crate) notifier: oneshot::Sender<UploadTaskResult>,
}

impl UploadItem {
    pub fn new(tasks: Vec<UploadTask>, notifier: oneshot::Sender<UploadTaskResult>) -> Self {
        Self { tasks, notifier }
    }
}

pub struct SharedBufferUploader {
    options: Arc<StorageConfig>,
    write_conflict_detector: Option<Arc<ConflictDetector>>,

    uploader_rx: Option<mpsc::UnboundedReceiver<UploadItem>>,

    sstable_store: SstableStoreRef,
    hummock_meta_client: Arc<dyn HummockMetaClient>,
    next_local_sstable_id: Arc<AtomicU64>,
    stats: Arc<StateStoreMetrics>,
    compaction_executor: Option<Arc<CompactionExecutor>>,
}

impl SharedBufferUploader {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        options: Arc<StorageConfig>,
        sstable_store: SstableStoreRef,
        hummock_meta_client: Arc<dyn HummockMetaClient>,
        uploader_rx: mpsc::UnboundedReceiver<UploadItem>,
        stats: Arc<StateStoreMetrics>,
        write_conflict_detector: Option<Arc<ConflictDetector>>,
    ) -> Self {
        let compaction_executor = if options.share_buffer_compaction_worker_threads_number == 0 {
            None
        } else {
            Some(Arc::new(CompactionExecutor::new(Some(
                options.share_buffer_compaction_worker_threads_number as usize,
            ))))
        };
        Self {
            options,
            write_conflict_detector,
            uploader_rx: Some(uploader_rx),
            sstable_store,
            hummock_meta_client,
            next_local_sstable_id: Arc::new(AtomicU64::new(0)),
            stats,
            compaction_executor,
        }
    }

    pub async fn run(mut self) -> HummockResult<()> {
        let rx = self.uploader_rx.take().unwrap();
        let this = Arc::new(self);
        let rx = tokio_stream::wrappers::UnboundedReceiverStream::new(rx);
        let mut stream = rx
            .map(|item| {
                let this = this.clone();
                tokio::spawn(this.run_inner(item))
            })
            .buffer_unordered(this.options.share_buffer_upload_concurrency);
        while let Some(res) = stream.next().await {
            res.unwrap()?;
        }
        Ok(())
    }
}

impl SharedBufferUploader {
    async fn run_inner(self: Arc<Self>, item: UploadItem) -> HummockResult<()> {
        let mut task_results = BTreeMap::new();
        let mut failed = false;
        for UploadTask {
            order_index,
            epoch,
            payload,
            is_local,
        } in item.tasks
        {
            // If a previous task failed, this task will also fail
            let result = if failed {
                Err(HummockError::shared_buffer_error(
                    "failed due to previous failure",
                ))
            } else {
                self.flush(epoch, is_local, &payload)
                    .await
                    .inspect_err(|e| {
                        error!("Failed to flush shared buffer: {:?}", e);
                        failed = true;
                    })
            };
            assert!(
                task_results.insert((epoch, order_index), result).is_none(),
                "Upload task duplicate. epoch: {:?}, order_index: {:?}",
                epoch,
                order_index,
            );
        }
        item.notifier
            .send(task_results)
            .map_err(|_| HummockError::shared_buffer_error("failed to send result"))?;
        Ok(())
    }

    async fn flush(
        &self,
        _epoch: HummockEpoch,
        is_local: bool,
        payload: &UploadTaskPayload,
    ) -> HummockResult<Vec<SstableInfo>> {
        if payload.is_empty() {
            return Ok(vec![]);
        }

        // Compact buffers into SSTs
        let mem_compactor_ctx = CompactorContext {
            options: self.options.clone(),
            hummock_meta_client: self.hummock_meta_client.clone(),
            sstable_store: self.sstable_store.clone(),
            stats: self.stats.clone(),
            is_share_buffer_compact: true,
            sstable_id_generator: if is_local {
                let atomic = self.next_local_sstable_id.clone();
                Arc::new(move || {
                    {
                        let atomic = atomic.clone();
                        async move { Ok(get_local_sst_id(atomic.fetch_add(1, Relaxed))) }
                    }
                    .boxed()
                })
            } else {
                get_remote_sstable_id_generator(self.hummock_meta_client.clone())
            },
            compaction_executor: self.compaction_executor.as_ref().cloned(),
        };

        let tables = Compactor::compact_shared_buffer(Arc::new(mem_compactor_ctx), payload).await?;

        let uploaded_sst_info: Vec<SstableInfo> = tables
            .into_iter()
            .map(|(sst, vnode_bitmaps)| SstableInfo {
                id: sst.id,
                key_range: Some(risingwave_pb::hummock::KeyRange {
                    left: sst.meta.smallest_key.clone(),
                    right: sst.meta.largest_key.clone(),
                    inf: false,
                }),
                file_size: sst.meta.estimated_size as u64,
                vnode_bitmaps,
            })
            .collect();

        // TODO: re-enable conflict detector after we have a better way to determine which actor
        // writes the batch. if let Some(detector) = &self.write_conflict_detector {
        //     for data_list in payload {
        //         for data in data_list {
        //             if let UncommittedData::Batch(batch) = data {
        //                 detector.check_conflict_and_track_write_batch(batch.get_payload(),
        // epoch);             }
        //         }
        //     }
        // }

        Ok(uploaded_sst_info)
    }
}
