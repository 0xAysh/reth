// In-package adapter around the pinned Geth logIterator, not a second layout implementation.
package filtermaps

import (
	"crypto/sha256"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/ethereum/go-ethereum/common"
	"github.com/ethereum/go-ethereum/core/types"
)

type oracleLog struct {
	count   int
	address common.Address
	topics  []common.Hash
}
type oracleBlock struct {
	number   uint64
	receipts [][]oracleLog
}
type oracleCase struct {
	name                      string
	start                     uint64
	continuation, rangeParams bool
	blocks                    []oracleBlock
	next                      *oracleBlock
}
type oracleChain struct{ receipts map[uint64]types.Receipts }

func (*oracleChain) GetHeader(common.Hash, uint64) *types.Header { return nil }
func (*oracleChain) GetCanonicalHash(n uint64) common.Hash       { return oracleHash(n) }
func (*oracleChain) GetReceiptsByHash(common.Hash) types.Receipts {
	panic("iterator must use RawReceipts")
}
func (c *oracleChain) GetRawReceipts(_ common.Hash, n uint64) types.Receipts { return c.receipts[n] }
func oracleHash(n uint64) common.Hash {
	return sha256.Sum256([]byte(fmt.Sprintf("canonical-block-%d", n)))
}
func oracleLogs(count, topics int) oracleLog {
	log := oracleLog{count: count, address: common.HexToAddress("0x1234567890abcdef1234567890abcdef12345678")}
	for i := 0; i < topics; i++ {
		log.topics = append(log.topics, sha256.Sum256([]byte(fmt.Sprintf("topic-%d", i))))
	}
	return log
}
func oracleBlocks(number uint64, logs ...oracleLog) oracleBlock {
	return oracleBlock{number, [][]oracleLog{{}, logs, {}}}
}
func oracleReceipts(b oracleBlock) types.Receipts {
	r := make(types.Receipts, 0, len(b.receipts))
	for _, specs := range b.receipts {
		receipt := &types.Receipt{}
		for _, spec := range specs {
			for i := 0; i < spec.count; i++ {
				receipt.Logs = append(receipt.Logs, &types.Log{Address: spec.address, Topics: spec.topics})
			}
		}
		r = append(r, receipt)
	}
	return r
}

// Only consecutive, identical searchable values are run-length encoded. Boundaries,
// pointers, kinds and hash changes always break a run; Rust expands every recorded index.
type oracleOutput struct {
	strings.Builder
	first, last uint64
	hash        common.Hash
	kind        string
	pending     bool
}

func (w *oracleOutput) flush() {
	if w.pending {
		fmt.Fprintf(&w.Builder, "V %d %d %s %s\n", w.first, w.last, w.hash.Hex(), w.kind)
		w.pending = false
	}
}
func (w *oracleOutput) event(format string, args ...any) {
	w.flush()
	fmt.Fprintf(&w.Builder, format+"\n", args...)
}
func (w *oracleOutput) value(index uint64, hash common.Hash, kind string) {
	if w.pending && index == w.last+1 && hash == w.hash && kind == w.kind {
		w.last = index
		return
	}
	w.flush()
	w.first, w.last, w.hash, w.kind, w.pending = index, index, hash, kind, true
}
func oracleWriteBlock(w *strings.Builder, b oracleBlock, prefix string) {
	fmt.Fprintf(w, "%sBLOCK %d %s\n", prefix, b.number, oracleHash(b.number).Hex())
	for _, receipt := range b.receipts {
		fmt.Fprintf(w, "%sRECEIPT\n", prefix)
		for _, log := range receipt {
			fmt.Fprintf(w, "%sLOG %d %s", prefix, log.count, log.address.Hex())
			for _, topic := range log.topics {
				fmt.Fprintf(w, " %s", topic.Hex())
			}
			fmt.Fprintln(w)
		}
	}
}
func TestGenStream(t *testing.T) {
	if len(os.Getenv("ORACLE_REV")) != 40 {
		t.Fatal("ORACLE_REV must contain the exact generator commit; use regenerate.sh")
	}
	const m = uint64(65536)
	emptySuccessor := oracleBlocks(11)
	fullSuccessor := oracleBlocks(11, oracleLogs(1, 1))
	cases := []oracleCase{
		{name: "genesis", blocks: []oracleBlock{oracleBlocks(0)}},
		{name: "empty-blocks-receipts", blocks: []oracleBlock{{0, nil}, oracleBlocks(1), oracleBlocks(2)}},
		{name: "nonzero-topics", start: 17, blocks: []oracleBlock{oracleBlocks(10, oracleLogs(1, 0), oracleLogs(1, 1), oracleLogs(1, 2), oracleLogs(1, 3), oracleLogs(1, 4))}},
		{name: "exact-fit", start: m - 5, blocks: []oracleBlock{oracleBlocks(10, oracleLogs(1, 4))}},
		{name: "first-log-padding", start: m - 2, blocks: []oracleBlock{oracleBlocks(10), oracleBlocks(11, oracleLogs(1, 2))}},
		{name: "later-log-padding", start: m - 3, blocks: []oracleBlock{oracleBlocks(10, oracleLogs(1, 0), oracleLogs(1, 2))}},
		{name: "multi-slot-padding", start: m - 4, blocks: []oracleBlock{oracleBlocks(10, oracleLogs(1, 0), oracleLogs(1, 4))}},
		{name: "multiple-maps", blocks: []oracleBlock{oracleBlocks(10, oracleLogs(int(2*m+1), 0))}},
		{name: "delimiter-empty-successor", start: m - 2, blocks: []oracleBlock{oracleBlocks(10, oracleLogs(1, 0)), emptySuccessor}},
		{name: "delimiter-full-successor", start: m - 2, blocks: []oracleBlock{oracleBlocks(10, oracleLogs(1, 0)), fullSuccessor}},
		{name: "pending-head-boundary", start: m - 1, blocks: []oracleBlock{oracleBlocks(10)}},
		{name: "absolute-map", start: 3*m - 1, blocks: []oracleBlock{oracleBlocks(10, oracleLogs(1, 0))}},
		{name: "batch-delimiter-empty", start: m - 2, blocks: []oracleBlock{oracleBlocks(10, oracleLogs(1, 0))}, next: &emptySuccessor},
		{name: "batch-delimiter-full", start: m - 2, blocks: []oracleBlock{oracleBlocks(10, oracleLogs(1, 0))}, next: &fullSuccessor},
		{name: "batch-before-padding", start: m - 3, blocks: []oracleBlock{oracleBlocks(10, oracleLogs(1, 0))}, next: &fullSuccessor},
		{name: "continuation-padding", start: m - 1, continuation: true, blocks: []oracleBlock{fullSuccessor}},
		{name: "continuation-empty", start: m, continuation: true, blocks: []oracleBlock{emptySuccessor}},
		{name: "range-maps", rangeParams: true, blocks: []oracleBlock{oracleBlocks(10, oracleLogs(3, 0)), oracleBlocks(11)}},
	}
	out := os.Getenv("STREAM_OUT")
	if out == "" {
		t.Fatal("STREAM_OUT must name the fixture output directory")
	}
	if err := os.MkdirAll(out, 0755); err != nil {
		t.Fatal(err)
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) { generateOracleStream(t, c, out) })
	}
}
func generateOracleStream(t *testing.T, c oracleCase, out string) {
	p, paramName := DefaultParams, "DEFAULT"
	if c.rangeParams {
		p, paramName = RangeTestParams, "RANGE"
	}
	if err := p.sanitize(); err != nil {
		t.Fatal(err)
	}
	chain := &oracleChain{make(map[uint64]types.Receipts)}
	var input strings.Builder
	fmt.Fprintln(&input, "# Geth af7c0fd8ee09de71b1034dbe6d1112556b49b59f")
	fmt.Fprintf(&input, "# Generator https://github.com/0xAysh/reth/blob/%s/tools/filtermaps-oracles/stream/gen_stream_test.go\n", os.Getenv("ORACLE_REV"))
	fmt.Fprintln(&input, "FORMAT 1")
	fmt.Fprintf(&input, "PARAMS %s\n", paramName)
	startMode := "ANCHOR"
	if c.continuation {
		startMode = "CONTINUATION"
	}
	fmt.Fprintf(&input, "START %s %d\n", startMode, c.start)
	head := c.blocks[len(c.blocks)-1].number
	if c.next == nil {
		fmt.Fprintln(&input, "END HEAD")
	} else {
		head = c.next.number
		fmt.Fprintf(&input, "END BATCH %d %s\n", head, oracleHash(head).Hex())
		oracleWriteBlock(&input, *c.next, "NEXT_")
		chain.receipts[head] = oracleReceipts(*c.next)
	}
	for _, block := range c.blocks {
		chain.receipts[block.number] = oracleReceipts(block)
		oracleWriteBlock(&input, block, "")
	}
	first := c.blocks[0].number
	l := &logIterator{params: &p, chainView: NewChainView(chain, head, oracleHash(head)), blockNumber: first, receipts: chain.receipts[first], blockStart: true, lvIndex: c.start}
	l.enforceValidState()
	if !c.continuation && l.skipToBoundary {
		t.Fatal("anchor must already be a non-padding pointer")
	}
	var output oracleOutput
	pointers := make(map[uint64]uint64)
	for {
		if _, seen := pointers[l.blockNumber]; !seen && !l.skipToBoundary {
			pointers[l.blockNumber] = l.lvIndex
			output.event("P %d %s %d", l.blockNumber, l.chainView.BlockId(l.blockNumber).Hex(), l.lvIndex)
		}
		if l.finished {
			// H/B encode Rust completion using observed Geth state; they are adapter
			// assertions, not event types emitted by Geth itself.
			output.event("H %d %s %d %d", l.blockNumber, l.chainView.BlockId(l.blockNumber).Hex(), pointers[l.blockNumber], l.lvIndex)
			break
		}
		index := l.lvIndex
		if l.skipToBoundary {
			output.event("X %d", index)
		} else if l.delimiter {
			output.event("D %d %d %s", index, l.blockNumber, l.chainView.BlockId(l.blockNumber).Hex())
		} else {
			kind := "A"
			if l.topicIndex > 0 {
				kind = fmt.Sprintf("T%d", l.topicIndex-1)
			}
			output.value(index, l.getValueHash(), kind)
		}
		if err := l.next(); err != nil {
			t.Fatal(err)
		}
		if l.lvIndex%p.valuesPerMap == 0 {
			output.event("M %d %d %s", index/p.valuesPerMap, l.blockNumber, l.chainView.BlockId(l.blockNumber).Hex())
		}
		if c.next != nil && l.blockNumber == c.next.number {
			last := c.blocks[len(c.blocks)-1].number
			output.event("B %d %s %d %d %s %d", last, oracleHash(last).Hex(), pointers[last], l.blockNumber, l.chainView.BlockId(l.blockNumber).Hex(), l.lvIndex)
			break
		}
	}
	fmt.Fprintln(&input, "EVENTS")
	input.WriteString(output.String())
	if err := os.WriteFile(filepath.Join(out, c.name+".txt"), []byte(input.String()), 0644); err != nil {
		t.Fatal(err)
	}
}
