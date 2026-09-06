# FilterMaps oracle generators

Fork-only reference material for `reth-filter-maps`. Keep this folder on the
public `filter-maps-oracles` branch of `0xAysh/reth`; do not merge the reference
branch into `filter-maps` or include Go tooling in the upstream submission.
Reth's implementation branch contains generated outputs and ordinary Rust tests.
Private project specs and agent notes do not belong here.

## Pins

- Geth: `af7c0fd8ee09de71b1034dbe6d1112556b49b59f`.
- Generator: the exact commit containing this folder, recorded in every generated
  stream fixture by `regenerate.sh`.
- Tested tools: Go `1.26.5`; rustfmt `1.9.0-nightly (37d85e592f 2026-04-28)`.

Publish the exact oracle commit before relying on its links in a public PR. Rust
CI consumes committed outputs and requires neither Go nor a Geth checkout.

## Regeneration

Start with the exact oracle commit, a clean checkout of the pinned Geth commit,
and a Reth implementation checkout containing `crates/filter-maps`:

```sh
REFERENCE=/absolute/path/to/oracle-reference-checkout
RETH=/absolute/path/to/reth-implementation-checkout

git clone https://github.com/ethereum/go-ethereum geth-oracle
cd geth-oracle
git checkout --detach af7c0fd8ee09de71b1034dbe6d1112556b49b59f
GETH="$PWD"

# Select one output family, or use all. Omitting the mode also means all.
bash "$REFERENCE/tools/filtermaps-oracles/regenerate.sh" stream "$GETH" "$RETH"
bash "$REFERENCE/tools/filtermaps-oracles/regenerate.sh" mapping "$GETH" "$RETH"
bash "$REFERENCE/tools/filtermaps-oracles/regenerate.sh" all "$GETH" "$RETH"

cd "$RETH"
cargo test -p reth-filter-maps --test it
```

The script verifies the clean Geth checkout and pinned revision, requires a
committed oracle source tree, refuses to overwrite generator files, removes only
its copied files on exit, and runs nightly formatting for the Rust crate.

Outputs are written to:

```text
mapping → crates/filter-maps/tests/it/golden/vectors.rs
stream  → crates/filter-maps/tests/it/golden_stream/fixtures/*.txt
```

To verify determinism, regenerate from clean inputs twice and compare output
hashes byte-for-byte. Generated outputs are never hand-edited. Neither generator
reimplements the layout rules; both execute actual unexported Geth functions.

## Mapping oracle

`mapping/gen_golden_test.go` calls `sanitize` on both shipped parameter sets and
records address/topic hashes, row and column indices, row limits, map masks,
epoch endpoints, and map groups. It pins selected representative and boundary
inputs, not the entire input domain:

- Layers 0–6 and the default layer-3 growth clamp.
- Default map boundaries 65535/65536/65537.
- Default epoch boundaries 1023/1024/1025 and later ordinary epochs.
- Final and preceding representable epochs for both parameter sets.
- Group boundaries 31/32/33 and 63/64, and the final `u32` map index.

Earlier PR1 vectors used Geth `ca1f2e4d38f4e94676981bb9251239a5d490b004`.
Its `math.go` and `map_renderer.go` are byte-identical to the pin above.

## Whole-stream oracle

`stream/gen_stream_test.go` initializes Geth's real `logIterator` over a
deterministic in-memory `ChainView`, then observes `getValueHash`, `next`, and
iterator state. It supplies trusted synthetic entry indices, numbers, full
block hashes, and receipt/log content. It does not prove checkpoint provenance,
receipt completeness against a chain, renderer rows, storage, matching, or reorg
behavior.

Eighteen cases cover genesis, nonzero entry indices and block numbers, empty
blocks and receipts, zero through four topics, exact fit, first/later-log
padding, multiple maps in one block, delimiter boundaries with empty/non-empty
successors, pending head delimiters, absolute map indices, bounded batches,
continuation padding, and one-slot range-test maps.

Every fixture starts with uniform provenance and a format version:

```text
# Geth <exact-commit>
# Generator https://github.com/0xAysh/reth/blob/<exact-commit>/tools/filtermaps-oracles/stream/gen_stream_test.go
FORMAT 1
```

Fixture grammar:

| Record | Meaning |
| --- | --- |
| `FORMAT 1` | Fixture grammar version |
| `PARAMS DEFAULT/RANGE` | Shipped parameter set |
| `START ANCHOR/CONTINUATION index` | Trusted pointer or raw batch cursor |
| `END HEAD` / `END BATCH number hash` | Explicit termination |
| `BLOCK number hash`, `RECEIPT`, `LOG count address [topics...]` | Receipt-ordered input; repeated identical logs keep large cases readable |
| `NEXT_*` | Successor receipts needed by Geth, not supplied as Rust batch input |
| `P number hash index` | Block pointer normalized from observed Geth state |
| `V first last hash A/T0/T1/T2/T3` | Inclusive run of identical searchable values |
| `X index` | Observed padding slot |
| `D index number hash` | Observed materialized delimiter |
| `M map number hash` | Boundary normalized from observed post-advance state |
| `H number hash pointer pending` | Rust head completion adapted from observed state |
| `B last-number hash pointer next-number hash cursor` | Rust batch completion adapted from observed state |

Compression merges only consecutive identical `V` records. Every pointer,
boundary, kind/hash change, delimiter, and padding slot breaks the run. Rust
expands and compares every index, keeping the multiple-map fixture small without
skipping its 131,073 values.

Geth does not emit Rust enums. `P`, `M`, value kinds, and `H`/`B` completion are
explicit normalizations of observed iterator state. Rust's independent
uninterrupted-versus-batched tests establish its continuation contract.
