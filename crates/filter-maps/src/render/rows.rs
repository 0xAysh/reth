use crate::Params;
use alloy_primitives::B256;
use schnellru::{ByLength, LruMap};
use std::collections::HashMap;

/// The number of row mappings cached per map, as in Geth (`cachedRowMappings`).
const CACHED_ROW_MAPPINGS: u32 = 10_000;

/// The rows of the filter map that is being rendered.
#[derive(Debug)]
pub(crate) struct ActiveRows {
    rows: HashMap<u32, Vec<u32>>,
    /// Geth's row-mapping cache: the row and mapping layer where a value was last marked. Rows
    /// only grow while a map is rendered, so the layers below the cached one stay full.
    cache: LruMap<B256, (u32, u32)>,
    /// The number of placements that started from a cached row mapping.
    #[cfg(test)]
    cache_hits: usize,
}

impl ActiveRows {
    /// Creates the rows of an empty map.
    pub(crate) fn new() -> Self {
        Self {
            rows: HashMap::new(),
            cache: LruMap::new(ByLength::new(CACHED_ROW_MAPPINGS)),
            #[cfg(test)]
            cache_hits: 0,
        }
    }

    /// Marks `value` at log value index `index` on the lowest mapping layer whose row has room.
    ///
    /// The layer always stays small: a map holds at most `values_per_map` marks, so only a handful
    /// of rows can be full at the clamped maximum row length.
    pub(crate) fn place(&mut self, params: Params, map_index: u32, index: u64, value: B256) {
        let cached = self.cache.get(&value).copied();
        #[cfg(test)]
        if cached.is_some() {
            self.cache_hits += 1;
        }
        let (mut row_index, mut layer) =
            cached.unwrap_or_else(|| (params.row_index(map_index, 0, value), 0));
        let mut moved = cached.is_none();
        loop {
            let row = self.rows.entry(row_index).or_default();
            if row.len() < params.max_row_length(layer) as usize {
                row.push(params.column_index(index, value));
                break
            }
            layer += 1;
            row_index = params.row_index(map_index, layer, value);
            moved = true;
        }
        if moved {
            self.cache.insert(value, (row_index, layer));
        }
    }

    /// Returns the nonempty rows in ascending row order, with columns in insertion order.
    pub(crate) fn finish(self) -> Vec<(u32, Vec<u32>)> {
        let mut rows = self.rows.into_iter().filter(|(_, row)| !row.is_empty()).collect::<Vec<_>>();
        rows.sort_unstable_by_key(|(row_index, _)| *row_index);
        rows
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DEFAULT_PARAMS;

    #[test]
    fn a_value_is_marked_at_its_row_and_column() {
        let params = DEFAULT_PARAMS;
        let value = B256::ZERO;
        let mut rows = ActiveRows::new();
        rows.place(params, 0, 0, value);
        assert_eq!(
            rows.finish(),
            [(params.row_index(0, 0, value), vec![params.column_index(0, value)])]
        );
    }

    #[test]
    fn a_full_base_row_spills_to_the_next_layer() {
        let params = DEFAULT_PARAMS;
        let value = B256::repeat_byte(7);
        let mut rows = ActiveRows::new();
        let base_row_length = params.base_row_length() as u64;
        for index in 0..=base_row_length {
            rows.place(params, 0, index, value);
        }

        let base_row = params.row_index(0, 0, value);
        let overflow_row = params.row_index(0, 1, value);
        assert_ne!(base_row, overflow_row);
        let rows = rows.finish().into_iter().collect::<HashMap<_, _>>();
        let base = (0..base_row_length).map(|index| params.column_index(index, value));
        assert_eq!(rows[&base_row], base.collect::<Vec<_>>());
        assert_eq!(rows[&overflow_row], [params.column_index(base_row_length, value)]);
    }

    /// Places a small pool of values many times, so rows fill up and spill to higher layers, and
    /// compares the rows with a placement that starts every value at layer 0.
    #[test]
    fn repeated_values_hit_the_row_mapping_cache() {
        let params = DEFAULT_PARAMS;
        let map_index = 1500;
        let pool = [B256::repeat_byte(1), B256::repeat_byte(2), B256::repeat_byte(3)];
        let mut cached = ActiveRows::new();
        let mut cold = HashMap::<u32, Vec<u32>>::new();
        for index in 0..600u64 {
            let value = pool[(index % 3) as usize];
            cached.place(params, map_index, index, value);

            let mut layer = 0;
            loop {
                let row = cold.entry(params.row_index(map_index, layer, value)).or_default();
                if row.len() < params.max_row_length(layer) as usize {
                    row.push(params.column_index(index, value));
                    break
                }
                layer += 1;
            }
        }

        assert_eq!(cached.cache_hits, 597, "every placement after a value's first one is a hit");
        let mut cold = cold.into_iter().collect::<Vec<_>>();
        cold.sort_unstable();
        assert_eq!(cached.finish(), cold);
    }

    #[test]
    fn several_full_layers_are_skipped() {
        let params = DEFAULT_PARAMS;
        let value = B256::repeat_byte(9);
        let layer_rows = (0..=3).map(|layer| params.row_index(0, layer, value)).collect::<Vec<_>>();
        assert_eq!(
            layer_rows.iter().collect::<std::collections::HashSet<_>>().len(),
            layer_rows.len(),
            "the value maps to four distinct rows"
        );
        let mut rows = ActiveRows::new();
        for (layer, row) in (0..3).zip(&layer_rows) {
            rows.rows.insert(*row, vec![0; params.max_row_length(layer) as usize]);
        }

        rows.place(params, 0, 5, value);
        assert_eq!(rows.rows[&layer_rows[3]], [params.column_index(5, value)]);
    }
}
