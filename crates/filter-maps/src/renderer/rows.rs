use super::{RenderedRow, RendererError};
use crate::Params;
use alloy_primitives::B256;
use std::collections::HashMap;

#[derive(Debug)]
pub(super) struct ActiveRows {
    rows: HashMap<u32, Vec<u32>>,
}

impl ActiveRows {
    pub(super) fn new() -> Self {
        Self { rows: HashMap::new() }
    }

    pub(super) fn place(
        &mut self,
        params: Params,
        map_index: u32,
        index: u64,
        value: B256,
    ) -> Result<(), RendererError> {
        self.place_from_layer(params, map_index, index, value, 0)
    }

    fn place_from_layer(
        &mut self,
        params: Params,
        map_index: u32,
        index: u64,
        value: B256,
        mut layer: u32,
    ) -> Result<(), RendererError> {
        loop {
            let row_index = params.row_index(map_index, layer, value);
            let row_len = self.rows.get(&row_index).map_or(0, Vec::len);
            let capacity = params.max_row_length(layer) as usize;
            if row_len < capacity {
                let column = params.column_index(index, value);
                self.rows.entry(row_index).or_default().push(column);
                return Ok(())
            }
            layer = layer
                .checked_add(1)
                .ok_or(RendererError::MappingLayerExhausted { map_index, value })?;
        }
    }

    pub(super) fn finish(self) -> Vec<RenderedRow> {
        let mut rows = self
            .rows
            .into_iter()
            .filter(|(_, columns)| !columns.is_empty())
            .map(|(row_index, columns)| RenderedRow { row_index, columns })
            .collect::<Vec<_>>();
        rows.sort_unstable_by_key(|row| row.row_index);
        rows
    }

    #[cfg(test)]
    pub(super) fn mark_count(&self) -> usize {
        self.rows.values().map(Vec::len).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{LogValueKind, LogValueSlot, ParamsId, RANGE_TEST_PARAMS};

    #[test]
    fn searchable_values_are_placed_and_unsearchable_slots_are_not() {
        let params = RANGE_TEST_PARAMS;
        let value = B256::ZERO;
        let mut rows = ActiveRows::new();
        rows.place(params, 0, 0, value).unwrap();
        let expected_row = params.row_index(0, 0, value);
        let expected_column = params.column_index(0, value);
        assert_eq!(
            rows.finish(),
            [RenderedRow { row_index: expected_row, columns: vec![expected_column] }]
        );

        // Slot kinds are intentionally consumed by the renderer state machine rather than rows.
        let _ = LogValueSlot::Value { index: 0, value, kind: LogValueKind::Address };
        let _ = ParamsId::RangeTest;
    }

    #[test]
    fn duplicate_values_preserve_duplicate_columns_and_overflow() {
        let params = RANGE_TEST_PARAMS;
        let value = B256::repeat_byte(7);
        let mut rows = ActiveRows::new();
        rows.place(params, 0, 0, value).unwrap();
        rows.place(params, 0, 0, value).unwrap();
        let rendered = rows.finish();
        assert_eq!(rendered.iter().map(|row| row.columns.len()).sum::<usize>(), 2);
        assert_eq!(rendered[0].columns, vec![params.column_index(0, value)]);
        assert_eq!(rendered[1].columns, vec![params.column_index(0, value)]);
    }

    #[test]
    fn exact_capacity_spills_to_the_first_available_layer() {
        let params = RANGE_TEST_PARAMS;
        let value = (0u8..=u8::MAX)
            .map(B256::repeat_byte)
            .find(|value| params.row_index(0, 0, *value) != params.row_index(0, 1, *value))
            .expect("a value remaps on layer one");
        let base_row = params.row_index(0, 0, value);
        let overflow_row = params.row_index(0, 1, value);
        let mut rows = ActiveRows::new();
        rows.rows.insert(base_row, vec![1]);
        rows.place(params, 0, 0, value).unwrap();
        assert_eq!(rows.rows.get(&base_row).unwrap().len(), 1);
        assert_eq!(
            rows.rows.get(&overflow_row).unwrap().last(),
            Some(&params.column_index(0, value))
        );
    }

    #[test]
    fn several_full_layers_are_skipped_without_an_arbitrary_limit() {
        let params = RANGE_TEST_PARAMS;
        let value = (0u8..=u8::MAX)
            .map(B256::repeat_byte)
            .find(|value| {
                let rows =
                    (0..=3).map(|layer| params.row_index(0, layer, *value)).collect::<Vec<_>>();
                rows.iter().collect::<std::collections::HashSet<_>>().len() == rows.len()
            })
            .expect("a value maps to four distinct test rows");
        let mut rows = ActiveRows::new();
        for layer in 0..3 {
            rows.rows.insert(params.row_index(0, layer, value), vec![layer]);
        }
        rows.place(params, 0, 0, value).unwrap();
        assert_eq!(rows.rows.get(&params.row_index(0, 3, value)).unwrap().len(), 1);
    }

    #[test]
    fn layer_growth_is_checked() {
        let params = RANGE_TEST_PARAMS;
        let value = B256::repeat_byte(9);
        let row = params.row_index(0, u32::MAX, value);
        let capacity = params.max_row_length(u32::MAX) as usize;
        let mut rows = ActiveRows { rows: HashMap::from([(row, vec![0; capacity])]) };
        assert!(matches!(
            rows.place_from_layer(params, 0, 0, value, u32::MAX),
            Err(RendererError::MappingLayerExhausted { map_index: 0, value: actual }) if actual == value
        ));
    }
}
