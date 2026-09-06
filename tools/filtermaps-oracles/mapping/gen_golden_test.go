// Golden-vector generator for reth-filter-maps PR 1.
//
// This is an IN-PACKAGE test for go-ethereum's `core/filtermaps`. The mapping
// functions it pins (rowIndex, columnIndex, maxRowLength, maskedMapIndex,
// addressValue, topicValue) are unexported, so this file must live inside a
// geth checkout at core/filtermaps/ and compile as `package filtermaps`.
//
// It lives on the fork-only filter-maps-oracles reference branch, not in the
// upstream Reth submission. See ../README.md for pinned regeneration.
//
// Usage (see README.md):
//   cp gen_golden_test.go <geth>/core/filtermaps/
//   cd <geth>/core/filtermaps && go test -run TestGenGolden -v
// Output: filtermaps_golden.rs (override path with GOLDEN_OUT=/abs/path.rs).
// Paste the emitted `const` tables into the crate's test module and record the
// geth commit printed in the header comment.

package filtermaps

import (
	"encoding/hex"
	"fmt"
	"os"
	"strings"
	"testing"

	"github.com/ethereum/go-ethereum/common"
)

func hx(b []byte) string { return "0x" + hex.EncodeToString(b) }

func TestGenGolden(t *testing.T) {
	if len(os.Getenv("ORACLE_REV")) != 40 {
		t.Fatal("ORACLE_REV must contain the exact generator commit; use regenerate.sh")
	}
	// DefaultParams / RangeTestParams carry only source fields until sanitize()
	// runs deriveFields(); the mapping functions read the derived fields, so
	// this call is mandatory before using either param set.
	p := DefaultParams
	if err := p.sanitize(); err != nil {
		t.Fatal(err)
	}
	rt := RangeTestParams
	if err := rt.sanitize(); err != nil {
		t.Fatal(err)
	}

	var b strings.Builder
	w := func(format string, a ...any) { fmt.Fprintf(&b, format, a...) }

	// The header must let someone with only this file and a network connection
	// reproduce the tables; it cannot assume access to the generator's own repo.
	w("// GENERATED from go-ethereum core/filtermaps. DO NOT EDIT.\n")
	w("// Geth commit: af7c0fd8ee09de71b1034dbe6d1112556b49b59f\n")
	w("//\n")
	w("// Regenerate: the mapping functions are unexported, so this is produced by an\n")
	w("// in-package Go test placed at core/filtermaps/ in a go-ethereum checkout at the\n")
	w("// commit above. It calls sanitize() on DefaultParams and RangeTestParams (which\n")
	w("// runs deriveFields; the mapping functions read those derived fields), then walks\n")
	w("// addressValue, topicValue, rowIndex, columnIndex, maxRowLength, maskedMapIndex,\n")
	w("// mapEpoch, firstEpochMap, lastEpochMap, mapGroupIndex and mapGroupOffset over the\n")
	w("// inputs recorded in each table below, and prints them as Rust consts:\n")
	w("// Generator: https://github.com/0xAysh/reth/blob/%s/tools/filtermaps-oracles/mapping/gen_golden_test.go\n", os.Getenv("ORACLE_REV"))
	w("// Instructions: https://github.com/0xAysh/reth/blob/%s/tools/filtermaps-oracles/README.md\n", os.Getenv("ORACLE_REV"))
	w("// Run tools/filtermaps-oracles/regenerate.sh <geth-checkout> <reth-checkout>.\n")
	w("// Pins selected representative and boundary inputs, not every public input.\n")
	w("//\n")
	w("// Params are DEFAULT unless a table name says RANGE_TEST.\n\n")

	// --- fixed inputs (arbitrary but stable; only the exact bytes matter) ---
	addrs := []common.Address{
		common.HexToAddress("0x0000000000000000000000000000000000000000"),
		common.HexToAddress("0x0000000000000000000000000000000000000001"),
		common.HexToAddress("0xdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef"),
		common.HexToAddress("0xffffffffffffffffffffffffffffffffffffffff"),
	}
	topics := []common.Hash{
		common.HexToHash("0x0000000000000000000000000000000000000000000000000000000000000000"),
		common.HexToHash("0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef"),
		common.HexToHash("0xffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"),
	}

	// address_value / topic_value: input hex -> 32-byte sha256 output hex.
	w("pub const ADDRESS_VALUES: &[(&str, &str)] = &[\n")
	for _, a := range addrs {
		v := addressValue(a)
		w("    (%q, %q),\n", hx(a.Bytes()), hx(v.Bytes()))
	}
	w("];\n\n")

	w("pub const TOPIC_VALUES: &[(&str, &str)] = &[\n")
	for _, tp := range topics {
		v := topicValue(tp)
		w("    (%q, %q),\n", hx(tp.Bytes()), hx(v.Bytes()))
	}
	w("];\n\n")

	// Two value hashes reused for the row/column vectors, derived the real way.
	values := []common.Hash{addressValue(addrs[2]), topicValue(topics[1])}

	// row_index: (value_hash, map_index, layer_index) -> row.
	// map_index set spans an epoch boundary (maps_per_epoch = 1024): indices in
	// {0,5,1023} share an epoch (layer-0 row must be equal); 1024,1025 next epoch.
	w("// (value_hash, map_index, layer_index) -> row_index\n")
	w("pub const ROW_INDEX: &[(&str, u32, u32, u32)] = &[\n")
	for _, v := range values {
		for _, mi := range []uint32{0, 5, 1023, 1024, 1025, 2048} {
			for _, li := range []uint32{0, 1, 2, 3, 4, 5, 6} {
				w("    (%q, %d, %d, %d),\n", hx(v.Bytes()), mi, li, p.rowIndex(mi, li, v))
			}
		}
	}
	w("];\n\n")

	// column_index: (value_hash, log_value_index) -> column. The index set straddles a
	// map boundary (values_per_map = 65536): 65535 -> high bits 65535, 65536 -> 0.
	w("// (value_hash, log_value_index) -> column_index\n")
	w("pub const COLUMN_INDEX: &[(&str, u64, u32)] = &[\n")
	for _, v := range values {
		vv := v
		for _, lv := range []uint64{0, 1, 65535, 65536, 65537, 131072, 1234567} {
			w("    (%q, %d, %d),\n", hx(vv.Bytes()), lv, p.columnIndex(lv, &vv))
		}
	}
	w("];\n\n")

	// max_row_length: expect DEFAULT [8,128,2048,8192,8192,...]; RANGE_TEST all 1.
	w("// layer -> max_row_length (DEFAULT)\n")
	w("pub const MAX_ROW_LENGTH_DEFAULT: &[(u32, u32)] = &[\n")
	for li := uint32(0); li <= 6; li++ {
		w("    (%d, %d),\n", li, p.maxRowLength(li))
	}
	w("];\n\n")

	w("// layer -> max_row_length (RANGE_TEST)\n")
	w("pub const MAX_ROW_LENGTH_RANGE_TEST: &[(u32, u32)] = &[\n")
	for li := uint32(0); li <= 6; li++ {
		w("    (%d, %d),\n", li, rt.maxRowLength(li))
	}
	w("];\n\n")

	// masked_map_index: (map_index, layer) -> masked. Layer 0 clears low
	// log_maps_per_epoch bits (epoch-stable); higher layers clear fewer.
	w("// (map_index, layer) -> masked_map_index (DEFAULT)\n")
	w("pub const MASKED_MAP_INDEX_DEFAULT: &[(u32, u32, u32)] = &[\n")
	for _, mi := range []uint32{0, 5, 1023, 1024, 1025, 2048} {
		for _, li := range []uint32{0, 1, 2, 3, 4, 5, 6} {
			w("    (%d, %d, %d),\n", mi, li, p.maskedMapIndex(mi, li))
		}
	}
	w("];\n\n")

	w("// (map_index, layer) -> masked_map_index (RANGE_TEST)\n")
	w("pub const MASKED_MAP_INDEX_RANGE_TEST: &[(u32, u32, u32)] = &[\n")
	for _, mi := range []uint32{0, 1, 2, 7} {
		for _, li := range []uint32{0, 1, 2, 3, 4, 5, 6} {
			w("    (%d, %d, %d),\n", mi, li, rt.maskedMapIndex(mi, li))
		}
	}
	w("];\n\n")

	// Epoch helpers: map_index -> epoch, and epoch -> its first/last map. Inputs
	// straddle the epoch boundary (maps_per_epoch = 1024) so an off-by-one in the
	// shift shows up as a boundary that lands on the wrong map.
	w("// map_index -> (map_epoch, first_epoch_map(that epoch), last_epoch_map(that epoch)) (DEFAULT)\n")
	w("pub const EPOCH_HELPERS_DEFAULT: &[(u32, u32, u32, u32)] = &[\n")
	for _, mi := range []uint32{0, 1, 1023, 1024, 1025, 2047, 2048, 1048575, 1048576, ^uint32(0) - p.mapsPerEpoch, ^uint32(0)} {
		e := p.mapEpoch(mi)
		w("    (%d, %d, %d, %d),\n", mi, e, p.firstEpochMap(e), p.lastEpochMap(e))
	}
	w("];\n\n")

	// RangeTestParams puts one map per epoch, so every map is its own epoch: the
	// degenerate case where first and last epoch map coincide with the index.
	w("// map_index -> (map_epoch, first_epoch_map(that epoch), last_epoch_map(that epoch)) (RANGE_TEST)\n")
	w("pub const EPOCH_HELPERS_RANGE_TEST: &[(u32, u32, u32, u32)] = &[\n")
	for _, mi := range []uint32{0, 1, 2, 7, 1024, ^uint32(0) - 1, ^uint32(0)} {
		e := rt.mapEpoch(mi)
		w("    (%d, %d, %d, %d),\n", mi, e, rt.firstEpochMap(e), rt.lastEpochMap(e))
	}
	w("];\n\n")

	// Base row grouping: map_index -> (group start, offset within group).
	// baseRowGroupSize is 32 for both param sets, so inputs straddle 32 and 64.
	w("// map_index -> (map_group_index, map_group_offset) (DEFAULT)\n")
	w("pub const MAP_GROUP_DEFAULT: &[(u32, u32, u32)] = &[\n")
	for _, mi := range []uint32{0, 1, 31, 32, 33, 63, 64, 1000, 4294967295} {
		w("    (%d, %d, %d),\n", mi, p.mapGroupIndex(mi), p.mapGroupOffset(mi))
	}
	w("];\n\n")

	// Derived-field sanity (assert these in Rust against the const fn accessors).
	w("// sanity DEFAULT:    base_row_length=%d map_height=%d values_per_map=%d maps_per_epoch=%d\n",
		p.baseRowLength, p.mapHeight, p.valuesPerMap, p.mapsPerEpoch)
	w("// sanity RANGE_TEST: base_row_length=%d map_height=%d values_per_map=%d maps_per_epoch=%d\n",
		rt.baseRowLength, rt.mapHeight, rt.valuesPerMap, rt.mapsPerEpoch)

	out := "filtermaps_golden.rs"
	if env := os.Getenv("GOLDEN_OUT"); env != "" {
		out = env
	}
	if err := os.WriteFile(out, []byte(b.String()), 0o644); err != nil {
		t.Fatal(err)
	}
	t.Logf("wrote %s (%d bytes)", out, b.Len())
}
