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

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use futures::Future;
use risingwave_hummock_sdk::key::{Epoch, FullKey};
use risingwave_hummock_sdk::HummockSSTableId;
use tokio::task::JoinHandle;

use super::SstableMeta;
use crate::hummock::sstable_store::SstableStoreRef;
use crate::hummock::value::HummockValue;
use crate::hummock::{CachePolicy, HummockResult, SSTableBuilder, Sstable};

pub struct SealedSstableBuilder {
    pub id: HummockSSTableId,
    pub meta: SstableMeta,
    pub table_ids: Vec<u32>,
    pub upload_join_handle: JoinHandle<HummockResult<()>>,
    pub data_len: usize,
    pub unit_id: u64,
}

/// A wrapper for [`SSTableBuilder`] which automatically split key-value pairs into multiple tables,
/// based on their target capacity set in options.
///
/// When building is finished, one may call `finish` to get the results of zero, one or more tables.
pub struct CapacitySplitTableBuilder<B> {
    /// When creating a new [`SSTableBuilder`], caller use this closure to specify the id and
    /// options.
    get_id_and_builder: B,

    sealed_builders: Vec<SealedSstableBuilder>,

    current_builder: Option<SSTableBuilder>,

    sstable_store: SstableStoreRef,

    uploading_size: Arc<AtomicUsize>,
}

impl<B, F> CapacitySplitTableBuilder<B>
where
    B: Clone + Fn() -> F,
    F: Future<Output = HummockResult<SSTableBuilder>>,
{
    /// Creates a new [`CapacitySplitTableBuilder`] using given configuration generator.
    pub fn new(get_id_and_builder: B, sstable_store: SstableStoreRef) -> Self {
        Self {
            get_id_and_builder,
            sealed_builders: Vec::new(),
            current_builder: None,
            sstable_store,
            uploading_size: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Returns the number of [`SSTableBuilder`]s.
    pub fn len(&self) -> usize {
        self.sealed_builders.len() + if self.current_builder.is_some() { 1 } else { 0 }
    }

    /// Returns true if no builder is created.
    pub fn is_empty(&self) -> bool {
        self.sealed_builders.is_empty() && self.current_builder.is_none()
    }

    /// Adds a user key-value pair to the underlying builders, with given `epoch`.
    ///
    /// If the current builder reaches its capacity, this function will create a new one with the
    /// configuration generated by the closure provided earlier.
    pub async fn add_user_key(
        &mut self,
        user_key: Vec<u8>,
        value: HummockValue<&[u8]>,
        epoch: Epoch,
    ) -> HummockResult<()> {
        assert!(!user_key.is_empty());
        let full_key = FullKey::from_user_key(user_key, epoch);
        self.add_full_key(full_key.as_slice(), value, true).await?;
        Ok(())
    }

    /// Adds a key-value pair to the underlying builders.
    ///
    /// If `allow_split` and the current builder reaches its capacity, this function will create a
    /// new one with the configuration generated by the closure provided earlier.
    ///
    /// Note that in some cases like compaction of the same user key, automatic splitting is not
    /// allowed, where `allow_split` should be `false`.
    pub async fn add_full_key(
        &mut self,
        full_key: FullKey<&[u8]>,
        value: HummockValue<&[u8]>,
        allow_split: bool,
    ) -> HummockResult<()> {
        if let Some(builder) = self.current_builder.as_ref() {
            if allow_split && builder.reach_capacity() {
                self.seal_current();
            }
        }

        if self.current_builder.is_none() {
            let _ = self
                .current_builder
                .insert((self.get_id_and_builder)().await?);
        }

        let builder = self.current_builder.as_mut().unwrap();
        builder.add(full_key.into_inner(), value);
        Ok(())
    }

    /// Marks the current builder as sealed. Next call of `add` will always create a new table.
    ///
    /// If there's no builder created, or current one is already sealed before, then this function
    /// will be no-op.
    pub fn seal_current(&mut self) {
        if let Some(builder) = self.current_builder.take() {
            let (table_id, data, meta, table_ids) = builder.finish();
            let len = data.len();
            self.uploading_size.fetch_add(len, Ordering::Relaxed);
            let sstable_store = self.sstable_store.clone();
            let meta_clone = meta.clone();
            let uploading_size = self.uploading_size.clone();
            let upload_join_handle = tokio::spawn(async move {
                let ret = sstable_store
                    .put(
                        Sstable {
                            id: table_id,
                            meta: meta_clone,
                        },
                        data,
                        CachePolicy::Fill,
                    )
                    .await;
                uploading_size.fetch_sub(len, Ordering::Relaxed);
                ret
            });
            self.sealed_builders.push(SealedSstableBuilder {
                id: table_id,
                meta,
                table_ids,
                upload_join_handle,
                data_len: len,
                unit_id: 0,
            })
        }
    }

    /// Finalizes all the tables to be ids, blocks and metadata.
    pub fn finish(mut self) -> Vec<SealedSstableBuilder> {
        self.seal_current();
        self.sealed_builders
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering::SeqCst;

    use itertools::Itertools;

    use super::*;
    use crate::hummock::iterator::test_utils::mock_sstable_store;
    use crate::hummock::sstable::utils::CompressionAlgorithm;
    use crate::hummock::test_utils::default_builder_opt_for_test;
    use crate::hummock::{SSTableBuilderOptions, DEFAULT_RESTART_INTERVAL};

    #[tokio::test]
    async fn test_empty() {
        let next_id = AtomicU64::new(1001);
        let block_size = 1 << 10;
        let table_capacity = 4 * block_size;
        let get_id_and_builder = || async {
            Ok(SSTableBuilder::new(
                next_id.fetch_add(1, SeqCst),
                SSTableBuilderOptions {
                    capacity: table_capacity,
                    block_capacity: block_size,
                    restart_interval: DEFAULT_RESTART_INTERVAL,
                    bloom_false_positive: 0.1,
                    compression_algorithm: CompressionAlgorithm::None,
                },
            ))
        };
        let builder = CapacitySplitTableBuilder::new(get_id_and_builder, mock_sstable_store());
        let results = builder.finish();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn test_lots_of_tables() {
        let next_id = AtomicU64::new(1001);

        let block_size = 1 << 10;
        let table_capacity = 4 * block_size;
        let get_id_and_builder = || async {
            Ok(SSTableBuilder::new(
                next_id.fetch_add(1, SeqCst),
                SSTableBuilderOptions {
                    capacity: table_capacity,
                    block_capacity: block_size,
                    restart_interval: DEFAULT_RESTART_INTERVAL,
                    bloom_false_positive: 0.1,
                    compression_algorithm: CompressionAlgorithm::None,
                },
            ))
        };
        let mut builder = CapacitySplitTableBuilder::new(get_id_and_builder, mock_sstable_store());

        for i in 0..table_capacity {
            builder
                .add_user_key(
                    b"key".to_vec(),
                    HummockValue::put(b"value"),
                    (table_capacity - i) as u64,
                )
                .await
                .unwrap();
        }

        let results = builder.finish();
        assert!(results.len() > 1);
        assert_eq!(results.iter().map(|p| p.id).duplicates().count(), 0);
    }

    #[tokio::test]
    async fn test_table_seal() {
        let next_id = AtomicU64::new(1001);
        let mut builder = CapacitySplitTableBuilder::new(
            || async {
                Ok(SSTableBuilder::new(
                    next_id.fetch_add(1, SeqCst),
                    default_builder_opt_for_test(),
                ))
            },
            mock_sstable_store(),
        );
        let mut epoch = 100;

        macro_rules! add {
            () => {
                epoch -= 1;
                builder
                    .add_user_key(b"k".to_vec(), HummockValue::put(b"v"), epoch)
                    .await
                    .unwrap();
            };
        }

        assert_eq!(builder.len(), 0);
        builder.seal_current();
        assert_eq!(builder.len(), 0);
        add!();
        assert_eq!(builder.len(), 1);
        add!();
        assert_eq!(builder.len(), 1);
        builder.seal_current();
        assert_eq!(builder.len(), 1);
        add!();
        assert_eq!(builder.len(), 2);
        builder.seal_current();
        assert_eq!(builder.len(), 2);
        builder.seal_current();
        assert_eq!(builder.len(), 2);

        let results = builder.finish();
        assert_eq!(results.len(), 2);
    }

    #[tokio::test]
    async fn test_initial_not_allowed_split() {
        let next_id = AtomicU64::new(1001);
        let mut builder = CapacitySplitTableBuilder::new(
            || async {
                Ok(SSTableBuilder::new(
                    next_id.fetch_add(1, SeqCst),
                    default_builder_opt_for_test(),
                ))
            },
            mock_sstable_store(),
        );

        builder
            .add_full_key(
                FullKey::from_user_key_slice(b"k", 233).as_slice(),
                HummockValue::put(b"v"),
                false,
            )
            .await
            .unwrap();
    }
}
