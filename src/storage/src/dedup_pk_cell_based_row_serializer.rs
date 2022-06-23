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
use std::iter::Iterator;

use risingwave_common::array::Row;
use risingwave_common::catalog::{ColumnDesc, ColumnId};
use risingwave_common::error::Result;

use crate::cell_based_row_serializer::CellBasedRowSerializer;
use crate::cell_serializer::{CellSerializer, KeyBytes, ValueBytes};

/// [`DedupPkCellBasedRowSerializer`] is identical to [`CellBasedRowSerializer`].
/// Difference is that before serializing a row, pk datums are filtered out.
pub struct DedupPkCellBasedRowSerializer {
    /// Contains:
    /// 1. Row indices of datums not in pk,
    /// 2. or datums which have to be stored regardless
    ///    (e.g. if memcomparable not equal to value encoding)
    dedup_datum_indices: HashSet<usize>,

    /// Serializing of row after filtering pk datums
    /// should be same as `CellBasedRowSerializer`.
    /// Hence we reuse its functionality.
    inner: CellBasedRowSerializer,
}

impl DedupPkCellBasedRowSerializer {
    /// Constructs a new [`DedupPkCellBasedRowSerializer`].
    pub fn new(
        pk_indices: &[usize],
        column_descs: &Vec<ColumnDesc>,
        column_ids: &[ColumnId],
    ) -> Self {
        let pk_indices = pk_indices.iter().cloned().collect::<HashSet<_>>();
        let dedup_datum_indices = (0..column_descs.len())
            .filter(|i| {
                !pk_indices.contains(i) || !column_descs[*i].data_type.mem_cmp_eq_value_enc()
            })
            .collect();
        let dedupped_column_ids = Self::remove_dup_pk_column_ids(&dedup_datum_indices, column_ids);
        let inner = CellBasedRowSerializer::new(dedupped_column_ids);
        Self {
            dedup_datum_indices,
            inner,
        }
    }

    /// Used internally to filter through an iterator,
    /// finding items which should be in dedup pk row.
    fn filter_by_dedup_datum_indices<'b, I>(
        dedup_datum_indices: &'b HashSet<usize>,
        iter: impl Iterator<Item = I> + 'b,
    ) -> impl Iterator<Item = I> + 'b {
        iter.enumerate()
            .filter(|(i, _)| dedup_datum_indices.contains(i))
            .map(|(_, d)| d)
    }

    /// Filters out duplicate pk datums by reference.
    fn remove_dup_pk_datums_by_ref(&self, row: &Row) -> Row {
        Row(
            Self::filter_by_dedup_datum_indices(&self.dedup_datum_indices, row.0.iter())
                .cloned()
                .collect(),
        )
    }

    /// Filters out duplicate pk datums.
    fn remove_dup_pk_datums(&self, row: Row) -> Row {
        Row(
            Self::filter_by_dedup_datum_indices(&self.dedup_datum_indices, row.0.into_iter())
                .collect(),
        )
    }

    /// Filters out column ids duplicate
    fn remove_dup_pk_column_ids(
        dedup_datum_indices: &HashSet<usize>,
        column_ids: &[ColumnId],
    ) -> Vec<ColumnId> {
        Self::filter_by_dedup_datum_indices(dedup_datum_indices, column_ids.iter())
            .cloned()
            .collect()
    }
}

impl CellSerializer for DedupPkCellBasedRowSerializer {
    /// Remove dup pk datums + serialize
    fn serialize(&mut self, pk: &[u8], row: Row) -> Result<Vec<(KeyBytes, ValueBytes)>> {
        let row = self.remove_dup_pk_datums(row);
        self.inner.serialize(pk, row)
    }

    /// Remove dup pk datums + serialize_without_filter
    fn serialize_without_filter(
        &mut self,
        pk: &[u8],
        row: Row,
    ) -> Result<Vec<Option<(KeyBytes, ValueBytes)>>> {
        let row = self.remove_dup_pk_datums(row);
        self.inner.serialize_without_filter(pk, row)
    }

    /// Remove dup pk datums + serialize_cell_key
    fn serialize_cell_key(&mut self, pk: &[u8], row: &Row) -> Result<Vec<KeyBytes>> {
        let row = self.remove_dup_pk_datums_by_ref(row);
        self.inner.serialize_cell_key(pk, &row)
    }

    /// Get column ids used by cell serializer to serialize.
    /// TODO: This should probably not be exposed to user.
    fn column_ids(&self) -> &[ColumnId] {
        self.inner.column_ids()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use itertools::Itertools;
    use risingwave_common::array::Row;
    use risingwave_common::catalog::{ColumnDesc, ColumnId};
    use risingwave_common::types::DataType;
    use crate::cell_based_row_deserializer::make_cell_based_row_deserializer;

    #[test]
    fn test_dedup_pk_serialization() {
        let pk_indices = vec![1, 3];
        let column_descs = vec![
            ColumnDesc::unnamed(ColumnId::from(0), DataType::Int32),
            ColumnDesc::unnamed(ColumnId::from(1), DataType::Int32),
            ColumnDesc::unnamed(ColumnId::from(2), DataType::Int32),
            ColumnDesc::unnamed(ColumnId::from(3), DataType::Float64), // test memcmp != value enc.
        ];
        let column_ids = column_descs.iter().map(|c| c.column_id).collect_vec();
        let mut serializer = DedupPkCellBasedRowSerializer::new(&pk_indices, &column_descs, &column_ids);
        let pk = vec![];
        let input = Row(vec![
            Some(1_i32.into()),
            Some(11_i32.into()),
            Some(111_i32.into()),
            Some(1111_f64.into()),
        ]);
        let actual = serializer.serialize(&pk, input).unwrap();
        // datums not in pk (2)
        // + datums whose memcmp not equal to value enc (1)
        // + delimiter cell (1)
        assert!(actual.len() == 4);

        // follows exact layout of serialized cells
        let compact_descs = vec![
            ColumnDesc::unnamed(ColumnId::from(0), DataType::Int32),
            // dedupped pk datum: ColumnDesc::unnamed(ColumnId::from(1), DataType::Int32),
            ColumnDesc::unnamed(ColumnId::from(2), DataType::Int32),
            ColumnDesc::unnamed(ColumnId::from(3), DataType::Float64), // test memcmp != value enc.
        ];
        let mut compact_deserializer = make_cell_based_row_deserializer(compact_descs);
        for (pk_with_cell_id, cell) in &actual {
            compact_deserializer.deserialize(pk_with_cell_id, cell).unwrap();
        }
        let (_k, row) = compact_deserializer.take().unwrap();
        let compact_expected = Row(vec![
            Some(1_i32.into()),
            Some(111_i32.into()),
            Some(1111_f64.into()),
        ]);
        assert_eq!(row, compact_expected);

        let mut normal_deserializer = make_cell_based_row_deserializer(column_descs);
        for (pk_with_cell_id, cell) in actual {
            normal_deserializer.deserialize(pk_with_cell_id, cell).unwrap();
        }
        let (_k, row) = normal_deserializer.take().unwrap();
        let normal_expected = Row(vec![
            Some(1_i32.into()),
            None,
            Some(111_i32.into()),
            Some(1111_f64.into()),
        ]);
        assert_eq!(row, normal_expected);
    }
}
