// In-package oracle for Geth's logical FilterMaps renderer and public matcher.
// This file is copied temporarily into core/filtermaps by regenerate.sh.
package filtermaps

import (
	"context"
	"crypto/sha256"
	"encoding/hex"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"slices"
	"sort"
	"strings"
	"sync"
	"testing"

	"github.com/ethereum/go-ethereum/common"
	"github.com/ethereum/go-ethereum/core/types"
)

const (
	pipelineGethRevision = "af7c0fd8ee09de71b1034dbe6d1112556b49b59f"
	pipelineStressSeed   = uint64(0xaf7c0fd8ee09de71)
)

type pipelineClass string

const (
	classFocused  pipelineClass = "FOCUSED"
	classEndToEnd pipelineClass = "END_TO_END"
	classStress   pipelineClass = "STRESS"
)

type pipelineLog struct {
	address common.Address
	topics  []common.Hash
	repeat  uint64
}
type pipelineReceipt struct{ logs []pipelineLog }
type pipelineBlock struct {
	number   uint64
	receipts []pipelineReceipt
}
type pipelineOrigin struct {
	kind, reason                    string
	cursor                          uint64
	previousNumber, previousPointer uint64
}
type pipelineQuery struct {
	id                    string
	firstBlock, lastBlock uint64
	addresses             []common.Address
	topics                [][]common.Hash
}
type pipelineScenario struct {
	name        string
	class       pipelineClass
	rangeParams bool
	seedOrdinal *uint64
	origin      pipelineOrigin
	blocks      []pipelineBlock
	queries     []pipelineQuery
}

type pipelineChain struct{ receipts map[uint64]types.Receipts }

func (*pipelineChain) GetHeader(common.Hash, uint64) *types.Header { return nil }
func (*pipelineChain) GetCanonicalHash(n uint64) common.Hash       { return pipelineBlockHash(n) }
func (*pipelineChain) GetReceiptsByHash(common.Hash) types.Receipts {
	panic("pipeline oracle must use RawReceipts")
}
func (c *pipelineChain) GetRawReceipts(_ common.Hash, n uint64) types.Receipts {
	return c.receipts[n]
}

func pipelineBlockHash(n uint64) common.Hash {
	return sha256.Sum256([]byte(fmt.Sprintf("canonical-block-%d", n)))
}
func pipelineAddress(label string) common.Address {
	h := sha256.Sum256([]byte("pipeline-address-" + label))
	return common.BytesToAddress(h[12:])
}
func pipelineTopic(label string) common.Hash {
	return sha256.Sum256([]byte("pipeline-topic-" + label))
}
func plog(label string, topicCount int) pipelineLog {
	l := pipelineLog{address: pipelineAddress(label), repeat: 1}
	for i := range topicCount {
		l.topics = append(l.topics, pipelineTopic(fmt.Sprintf("%s-%d", label, i)))
	}
	return l
}
func repeatedLog(label string, count uint64, topicCount int) pipelineLog {
	l := plog(label, topicCount)
	l.repeat = count
	return l
}
func pblock(number uint64, logs ...pipelineLog) pipelineBlock {
	return pipelineBlock{number: number, receipts: []pipelineReceipt{{logs: logs}}}
}
func pempty(number uint64) pipelineBlock { return pipelineBlock{number: number} }

func pipelineReceipts(block pipelineBlock) types.Receipts {
	receipts := make(types.Receipts, len(block.receipts))
	for ri, receiptSpec := range block.receipts {
		receipt := &types.Receipt{}
		for _, logSpec := range receiptSpec.logs {
			for range logSpec.repeat {
				receipt.Logs = append(receipt.Logs, &types.Log{
					Address: logSpec.address,
					Topics:  slices.Clone(logSpec.topics),
				})
			}
		}
		receipts[ri] = receipt
	}
	return receipts
}

type observedPointer struct {
	block uint64
	hash  common.Hash
	index uint64
}
type observedBoundary struct {
	mapIndex   uint32
	resume     uint64
	resumeHash common.Hash
	ending     string
}
type slotEvidence struct {
	class string
	logID string
	log   *types.Log
}
type streamObservation struct {
	pointers   []observedPointer
	boundaries []observedBoundary
	slots      map[uint64]slotEvidence
	pending    uint64
}

func newPipelineIterator(p *Params, view *ChainView, first uint64, cursor uint64) *logIterator {
	l := &logIterator{
		params:      p,
		chainView:   view,
		blockNumber: first,
		receipts:    view.RawReceipts(first),
		blockStart:  true,
		lvIndex:     cursor,
	}
	l.enforceValidState()
	return l
}

func observePipelineStream(t *testing.T, p *Params, view *ChainView, first, cursor uint64) streamObservation {
	t.Helper()
	l := newPipelineIterator(p, view, first, cursor)
	obs := streamObservation{slots: make(map[uint64]slotEvidence)}
	seenPointers := make(map[uint64]bool)
	for {
		if !l.skipToBoundary && !seenPointers[l.blockNumber] {
			seenPointers[l.blockNumber] = true
			obs.pointers = append(obs.pointers, observedPointer{l.blockNumber, view.BlockId(l.blockNumber), l.lvIndex})
		}
		if l.finished {
			obs.pending = l.lvIndex
			break
		}
		index := l.lvIndex
		ending, class := "", ""
		switch {
		case l.skipToBoundary:
			ending, class = "PADDING", "PADDING"
		case l.delimiter:
			ending, class = "DELIMITER", "DELIMITER"
		default:
			class = "A"
			if l.topicIndex > 0 {
				class = fmt.Sprintf("T%d", l.topicIndex-1)
			}
			ending = "VALUE"
		}
		evidence := slotEvidence{class: class}
		if !l.skipToBoundary && !l.delimiter {
			log := l.receipts[l.txIndex].Logs[l.logIndex]
			evidence.log = log
			evidence.logID = fmt.Sprintf("%d:%d:%d", l.blockNumber, l.txIndex, l.logIndex)
		}
		obs.slots[index] = evidence
		if err := l.next(); err != nil {
			t.Fatalf("advance stream at %d: %v", index, err)
		}
		if l.lvIndex%p.valuesPerMap == 0 {
			obs.boundaries = append(obs.boundaries, observedBoundary{
				mapIndex: uint32(index / p.valuesPerMap), resume: l.blockNumber,
				resumeHash: view.BlockId(l.blockNumber), ending: ending,
			})
		}
	}
	return obs
}

type observedMap struct {
	mapIndex         uint32
	epoch            uint32
	lastBlock        uint64
	lastBlockHash    common.Hash
	rows             filterMap
	pointerBlocks    []uint64
	boundary         *observedPointer
	pendingDelimiter uint64
	finished         bool
}
type renderObservation struct {
	completed []observedMap
	partials  []observedMap
}

func pointerByIndex(t *testing.T, pointers []observedPointer, index uint64) observedPointer {
	t.Helper()
	for _, pointer := range pointers {
		if pointer.index == index {
			return pointer
		}
	}
	t.Fatalf("no block pointer at index %d", index)
	return observedPointer{}
}
func pointerByBlock(t *testing.T, pointers []observedPointer, block uint64) observedPointer {
	t.Helper()
	for _, pointer := range pointers {
		if pointer.block == block {
			return pointer
		}
	}
	t.Fatalf("no pointer for block %d", block)
	return observedPointer{}
}

func observePipelineRenderer(t *testing.T, p Params, view *ChainView, first, cursor uint64, stream streamObservation) renderObservation {
	t.Helper()
	f := &FilterMaps{Params: p, targetView: view, indexedView: view, testDisableSnapshots: true}
	iterator := newPipelineIterator(&f.Params, view, first, cursor)
	mapIndex := uint32(cursor >> p.logValuesPerMap)
	var result renderObservation
	for {
		r := &mapRenderer{
			f:          f,
			currentMap: &renderedMap{filterMap: f.emptyFilterMap(), mapIndex: mapIndex, lastBlock: iterator.blockNumber},
			iterator:   iterator,
		}
		done, err := r.renderCurrentMap(func() bool { return false })
		if err != nil || !done {
			t.Fatalf("render map %d: done=%v err=%v", mapIndex, done, err)
		}
		rm := r.currentMap
		observed := observedMap{
			mapIndex: mapIndex, epoch: p.mapEpoch(mapIndex), lastBlock: rm.lastBlock,
			lastBlockHash: rm.lastBlockId, rows: rm.filterMap.fullCopy(), finished: rm.finished,
			pendingDelimiter: rm.headDelimiter,
		}
		for _, lvPointer := range rm.blockLvPtrs {
			observed.pointerBlocks = append(observed.pointerBlocks, pointerByIndex(t, stream.pointers, lvPointer).block)
		}
		mapEnd := uint64(mapIndex+1) << p.logValuesPerMap
		if iterator.lvIndex >= mapEnd {
			resume := pointerByBlock(t, stream.pointers, iterator.blockNumber)
			observed.boundary = &resume
			result.completed = append(result.completed, observed)
		} else {
			result.partials = append(result.partials, observed)
		}
		if iterator.finished {
			break
		}
		mapIndex++
	}
	return result
}

type syntheticMatcherBackend struct {
	params       *Params
	maps         map[uint32]filterMap
	pointers     map[uint64]uint64
	addressSlots map[uint64]*types.Log
	logIDs       map[*types.Log]string
	mu           sync.Mutex
	lookups      []uint64
}

func (b *syntheticMatcherBackend) GetParams() *Params { return b.params }
func (b *syntheticMatcherBackend) GetBlockLvPointer(_ context.Context, block uint64) (uint64, error) {
	pointer, ok := b.pointers[block]
	if !ok {
		return 0, fmt.Errorf("missing synthetic pointer for block %d", block)
	}
	return pointer, nil
}
func (b *syntheticMatcherBackend) GetFilterMapRows(_ context.Context, maps []uint32, row uint32, baseOnly bool) ([]FilterRow, error) {
	rows := make([]FilterRow, len(maps))
	for i, mapIndex := range maps {
		fm, ok := b.maps[mapIndex]
		if !ok {
			return nil, fmt.Errorf("missing completed synthetic map %d", mapIndex)
		}
		if row >= uint32(len(fm)) {
			return nil, fmt.Errorf("row %d outside map height", row)
		}
		rows[i] = fm[row]
		if baseOnly && uint32(len(rows[i])) > b.params.baseRowLength {
			rows[i] = rows[i][:b.params.baseRowLength]
		}
		if rows[i] == nil {
			rows[i] = FilterRow{}
		}
	}
	return rows, nil
}
func (b *syntheticMatcherBackend) GetLogByLvIndex(_ context.Context, index uint64) (*types.Log, error) {
	b.mu.Lock()
	b.lookups = append(b.lookups, index)
	b.mu.Unlock()
	return b.addressSlots[index], nil
}
func (b *syntheticMatcherBackend) SyncLogIndex(context.Context) (SyncRange, error) {
	return SyncRange{}, nil
}
func (*syntheticMatcherBackend) Close() {}

type queryObservation struct {
	input                 pipelineQuery
	addressValues         []common.Hash
	topicValues           [][]common.Hash
	firstIndex, lastIndex uint64
	firstMap, lastMap     uint32
	result                string
	potentialIndices      []uint64
	slotClasses           []string
	candidateBlocks       []uint64
	potentialLogs         []string
	exactLogs             []string
	exactBlocks           []uint64
	planner               string
}

func uniqueSorted(values []uint64) []uint64 {
	slices.Sort(values)
	return slices.Compact(values)
}
func containsU64(values []uint64, wanted uint64) bool {
	_, ok := slices.BinarySearch(values, wanted)
	return ok
}
func resolveCandidateBlock(t *testing.T, pointers []observedPointer, index uint64) uint64 {
	t.Helper()
	position := sort.Search(len(pointers), func(i int) bool { return pointers[i].index > index })
	if position == 0 {
		t.Fatalf("candidate index %d precedes first pointer", index)
	}
	return pointers[position-1].block
}
func exactPipelineMatch(log *types.Log, q pipelineQuery) bool {
	if len(q.addresses) > 0 && !slices.Contains(q.addresses, log.Address) {
		return false
	}
	for position, alternatives := range q.topics {
		if position >= len(log.Topics) {
			return false
		}
		if len(alternatives) > 0 && !slices.Contains(alternatives, log.Topics[position]) {
			return false
		}
	}
	return true
}

func observePipelineQuery(t *testing.T, p *Params, rendered renderObservation, stream streamObservation, receipts map[uint64]types.Receipts, q pipelineQuery) queryObservation {
	t.Helper()
	first := pointerByBlock(t, stream.pointers, q.firstBlock)
	after := pointerByBlock(t, stream.pointers, q.lastBlock+1)
	qo := queryObservation{input: q, firstIndex: first.index, lastIndex: after.index - 1, result: "OK", planner: "MATCHER"}
	qo.firstMap = uint32(qo.firstIndex >> p.logValuesPerMap)
	qo.lastMap = uint32(qo.lastIndex >> p.logValuesPerMap)
	for _, address := range q.addresses {
		qo.addressValues = append(qo.addressValues, addressValue(address))
	}
	for _, alternatives := range q.topics {
		values := make([]common.Hash, len(alternatives))
		for i, topic := range alternatives {
			values[i] = topicValue(topic)
		}
		qo.topicValues = append(qo.topicValues, values)
	}
	maps := make(map[uint32]filterMap)
	for _, observed := range rendered.completed {
		maps[observed.mapIndex] = observed.rows
	}
	pointerMap := make(map[uint64]uint64)
	for _, pointer := range stream.pointers {
		pointerMap[pointer.block] = pointer.index
	}
	addressSlots, logIDs := make(map[uint64]*types.Log), make(map[*types.Log]string)
	for index, evidence := range stream.slots {
		if evidence.class == "A" {
			addressSlots[index] = evidence.log
			logIDs[evidence.log] = evidence.logID
		}
	}
	backend := &syntheticMatcherBackend{params: p, maps: maps, pointers: pointerMap, addressSlots: addressSlots, logIDs: logIDs}
	logs, err := GetPotentialMatches(context.Background(), backend, q.firstBlock, q.lastBlock, q.addresses, q.topics)
	if errors.Is(err, ErrMatchAll) {
		qo.result, qo.planner = "ERR_MATCH_ALL", "EVERY_BLOCK"
		for block := q.firstBlock; block <= q.lastBlock; block++ {
			qo.candidateBlocks = append(qo.candidateBlocks, block)
		}
	} else if err != nil {
		t.Fatalf("query %s: %v", q.id, err)
	} else {
		backend.mu.Lock()
		qo.potentialIndices = uniqueSorted(slices.Clone(backend.lookups))
		backend.mu.Unlock()
		for _, index := range qo.potentialIndices {
			if index < qo.firstIndex || index > qo.lastIndex {
				t.Fatalf("query %s candidate %d outside [%d,%d]", q.id, index, qo.firstIndex, qo.lastIndex)
			}
			evidence, ok := stream.slots[index]
			if !ok {
				t.Fatalf("query %s candidate %d has no slot evidence", q.id, index)
			}
			qo.slotClasses = append(qo.slotClasses, evidence.class)
			qo.candidateBlocks = append(qo.candidateBlocks, resolveCandidateBlock(t, stream.pointers, index))
		}
		qo.candidateBlocks = uniqueSorted(qo.candidateBlocks)
		for _, log := range logs {
			id, ok := logIDs[log]
			if !ok {
				t.Fatalf("query %s returned unknown synthetic log", q.id)
			}
			qo.potentialLogs = append(qo.potentialLogs, id)
		}
	}
	blockSet := make(map[uint64]bool)
	for block := q.firstBlock; block <= q.lastBlock; block++ {
		for ri, receipt := range receipts[block] {
			for li, log := range receipt.Logs {
				if exactPipelineMatch(log, q) {
					qo.exactLogs = append(qo.exactLogs, fmt.Sprintf("%d:%d:%d", block, ri, li))
					blockSet[block] = true
				}
			}
		}
	}
	for block := range blockSet {
		qo.exactBlocks = append(qo.exactBlocks, block)
	}
	qo.exactBlocks = uniqueSorted(qo.exactBlocks)
	for _, block := range qo.exactBlocks {
		if !containsU64(qo.candidateBlocks, block) {
			t.Fatalf("query %s omitted exact block %d from candidates %v", q.id, block, qo.candidateBlocks)
		}
	}
	return qo
}

type generatedPipelineScenario struct {
	scenario pipelineScenario
	params   Params
	stream   streamObservation
	rendered renderObservation
	queries  []queryObservation
	text     string
}

func validatePipelineScenario(t *testing.T, s pipelineScenario) {
	t.Helper()
	if s.name == "" || len(s.blocks) < 2 {
		t.Fatalf("scenario %q needs a name and at least two blocks", s.name)
	}
	for i, block := range s.blocks {
		if i > 0 && block.number != s.blocks[i-1].number+1 {
			t.Fatalf("scenario %s has non-contiguous blocks", s.name)
		}
		for _, receipt := range block.receipts {
			for _, log := range receipt.logs {
				if log.repeat == 0 || len(log.topics) > 4 {
					t.Fatalf("scenario %s has invalid log", s.name)
				}
			}
		}
	}
	if s.class == classStress && s.seedOrdinal == nil {
		t.Fatalf("stress scenario %s has no seed ordinal", s.name)
	}
	if s.class != classStress && s.seedOrdinal != nil {
		t.Fatalf("curated scenario %s has stress seed", s.name)
	}
}

func validateGeneratedPipelineScenario(t *testing.T, g generatedPipelineScenario) {
	t.Helper()
	lastMap := uint32(0)
	seenMap := false
	checkMap := func(observed observedMap) {
		if seenMap && observed.mapIndex <= lastMap {
			t.Fatalf("scenario %s has non-ascending map %d", g.scenario.name, observed.mapIndex)
		}
		seenMap, lastMap = true, observed.mapIndex
		rows, marks := rowCounts(observed.rows)
		var streamMarks uint64
		for index, slot := range g.stream.slots {
			if uint32(index>>g.params.logValuesPerMap) == observed.mapIndex && (slot.class == "A" || strings.HasPrefix(slot.class, "T")) {
				streamMarks++
			}
		}
		if marks != streamMarks {
			t.Fatalf("scenario %s map %d renderer marks %d != observed searchable slots %d", g.scenario.name, observed.mapIndex, marks, streamMarks)
		}
		var countedRows uint64
		for rowIndex, row := range observed.rows {
			if len(row) == 0 {
				continue
			}
			countedRows++
			if uint32(rowIndex) >= g.params.mapHeight {
				t.Fatalf("scenario %s map %d row %d outside height", g.scenario.name, observed.mapIndex, rowIndex)
			}
			for _, column := range row {
				if column >= uint32(1)<<g.params.logMapWidth {
					t.Fatalf("scenario %s map %d column %d outside width", g.scenario.name, observed.mapIndex, column)
				}
			}
		}
		if countedRows != rows {
			t.Fatalf("scenario %s map %d row count mismatch", g.scenario.name, observed.mapIndex)
		}
	}
	for _, observed := range g.rendered.completed {
		if observed.boundary == nil {
			t.Fatalf("scenario %s completed map %d lacks boundary", g.scenario.name, observed.mapIndex)
		}
		checkMap(observed)
	}
	for _, observed := range g.rendered.partials {
		if observed.boundary != nil || !observed.finished {
			t.Fatalf("scenario %s private map %d has invalid completion state", g.scenario.name, observed.mapIndex)
		}
		checkMap(observed)
	}
	if len(g.stream.pointers) == 0 {
		t.Fatalf("scenario %s has no authoritative pointers", g.scenario.name)
	}
	for i, pointer := range g.stream.pointers {
		if pointer.hash != pipelineBlockHash(pointer.block) ||
			(i > 0 && (pointer.block != g.stream.pointers[i-1].block+1 || pointer.index <= g.stream.pointers[i-1].index)) {
			t.Fatalf("scenario %s has invalid pointer table at block %d", g.scenario.name, pointer.block)
		}
	}
	expectEnding := map[string]string{
		"boundary-by-value": "VALUE", "boundary-by-delimiter": "DELIMITER", "boundary-by-padding": "PADDING",
	}
	if ending := expectEnding[g.scenario.name]; ending != "" {
		if len(g.stream.boundaries) == 0 || g.stream.boundaries[0].ending != ending {
			t.Fatalf("scenario %s did not end its first map by %s", g.scenario.name, ending)
		}
	}
	if g.scenario.name == "default-overflow" {
		value := addressValue(pipelineAddress("overflow"))
		fm := g.rendered.completed[0].rows
		if len(fm[g.params.rowIndex(0, 0, value)]) != int(g.params.maxRowLength(0)) ||
			len(fm[g.params.rowIndex(0, 1, value)]) != int(g.params.maxRowLength(1)) ||
			len(fm[g.params.rowIndex(0, 2, value)]) == 0 {
			t.Fatal("default-overflow did not fill base and first overflow rows before using layer two")
		}
	}
	if g.scenario.name == "epoch-boundary" {
		if len(g.rendered.completed) == 0 || g.rendered.completed[0].mapIndex != 1023 || len(g.rendered.partials) == 0 || g.rendered.partials[0].mapIndex != 1024 {
			t.Fatal("epoch-boundary did not observe maps 1023 and 1024")
		}
	}
}

func generatePipelineScenario(t *testing.T, s pipelineScenario) generatedPipelineScenario {
	t.Helper()
	validatePipelineScenario(t, s)
	p := DefaultParams
	if s.rangeParams {
		p = RangeTestParams
	}
	if err := p.sanitize(); err != nil {
		t.Fatal(err)
	}
	receipts := make(map[uint64]types.Receipts)
	for _, block := range s.blocks {
		receipts[block.number] = pipelineReceipts(block)
	}
	head := s.blocks[len(s.blocks)-1].number
	chain := &pipelineChain{receipts: receipts}
	view := NewChainView(chain, head, pipelineBlockHash(head))
	stream := observePipelineStream(t, &p, view, s.blocks[0].number, s.origin.cursor)
	rendered := observePipelineRenderer(t, p, view, s.blocks[0].number, s.origin.cursor, stream)
	generated := generatedPipelineScenario{scenario: s, params: p, stream: stream, rendered: rendered}
	for _, query := range s.queries {
		generated.queries = append(generated.queries, observePipelineQuery(t, &generated.params, rendered, stream, receipts, query))
	}
	if s.name == "matcher-false-positives" {
		q := generated.queries[0]
		if len(q.candidateBlocks) <= len(q.exactBlocks) {
			t.Fatalf("fixed collision did not produce a false-positive candidate: candidates=%v exact=%v", q.candidateBlocks, q.exactBlocks)
		}
	}
	validateGeneratedPipelineScenario(t, generated)
	generated.text = serializePipelineScenario(t, generated)
	return generated
}

func rowCounts(fm filterMap) (rows, marks uint64) {
	for _, row := range fm {
		if len(row) > 0 {
			rows++
			marks += uint64(len(row))
		}
	}
	return
}
func writeNumberList[T ~uint32 | ~uint64](w *strings.Builder, label string, values []T) {
	fmt.Fprintf(w, "%s %d", label, len(values))
	for _, value := range values {
		fmt.Fprintf(w, " %d", value)
	}
	fmt.Fprintln(w)
}
func writeStringList(w *strings.Builder, label string, values []string) {
	fmt.Fprintf(w, "%s %d", label, len(values))
	for _, value := range values {
		fmt.Fprintf(w, " %s", value)
	}
	fmt.Fprintln(w)
}
func writeAddressList(w *strings.Builder, label string, values []common.Address) {
	fmt.Fprintf(w, "%s %d", label, len(values))
	for _, value := range values {
		fmt.Fprintf(w, " %s", strings.ToLower(value.Hex()))
	}
	fmt.Fprintln(w)
}
func writeHashList(w *strings.Builder, label string, values []common.Hash) {
	fmt.Fprintf(w, "%s %d", label, len(values))
	for _, value := range values {
		fmt.Fprintf(w, " %s", value.Hex())
	}
	fmt.Fprintln(w)
}
func writeRows(w *strings.Builder, fm filterMap) {
	for rowIndex, row := range fm {
		if len(row) == 0 {
			continue
		}
		fmt.Fprintf(w, "ROW %d %d", rowIndex, len(row))
		for _, column := range row {
			fmt.Fprintf(w, " %d", column)
		}
		fmt.Fprintln(w)
	}
}

func serializePipelineScenario(t *testing.T, g generatedPipelineScenario) string {
	t.Helper()
	s := g.scenario
	var w strings.Builder
	fmt.Fprintf(&w, "# Geth %s\n", pipelineGethRevision)
	fmt.Fprintf(&w, "# Generator https://github.com/0xAysh/reth/blob/%s/tools/filtermaps-oracles/pipeline/gen_pipeline_test.go\n", os.Getenv("ORACLE_REV"))
	fmt.Fprintln(&w, "FORMAT 2")
	fmt.Fprintf(&w, "SCENARIO %s\n", s.name)
	fmt.Fprintf(&w, "CLASS %s\n", s.class)
	if s.rangeParams {
		fmt.Fprintln(&w, "PARAMS RANGE")
	} else {
		fmt.Fprintln(&w, "PARAMS DEFAULT")
	}
	fmt.Fprintln(&w, "VALUE_SPACE GETH_V1")
	if s.seedOrdinal == nil {
		fmt.Fprintln(&w, "SEED NONE")
	} else {
		fmt.Fprintf(&w, "SEED SPLITMIX64 0x%016x %d\n", pipelineStressSeed, *s.seedOrdinal)
	}
	first := s.blocks[0]
	switch s.origin.kind {
	case "GENESIS":
		fmt.Fprintf(&w, "ORIGIN GENESIS %d %s %d\n", first.number, pipelineBlockHash(first.number).Hex(), s.origin.cursor)
	case "BATCH_CONTINUATION":
		fmt.Fprintf(&w, "ORIGIN BATCH_CONTINUATION %d %s %d %d %s %d\n", first.number, pipelineBlockHash(first.number).Hex(), s.origin.cursor, s.origin.previousNumber, pipelineBlockHash(s.origin.previousNumber).Hex(), s.origin.previousPointer)
	default:
		fmt.Fprintf(&w, "ORIGIN SYNTHETIC_CHECKPOINT %d %s %d %s\n", first.number, pipelineBlockHash(first.number).Hex(), s.origin.cursor, s.origin.reason)
	}
	fmt.Fprintln(&w, "TERMINATION HEAD")
	fmt.Fprintf(&w, "BLOCKS %d\n", len(s.blocks))
	for _, block := range s.blocks {
		fmt.Fprintf(&w, "BLOCK %d %s %d\n", block.number, pipelineBlockHash(block.number).Hex(), len(block.receipts))
		for _, receipt := range block.receipts {
			expanded := uint64(0)
			for _, log := range receipt.logs {
				expanded += log.repeat
			}
			fmt.Fprintf(&w, "RECEIPT %d\n", expanded)
			for _, log := range receipt.logs {
				label := "LOG"
				if log.repeat > 1 {
					label = "REPEAT_LOG"
					fmt.Fprintf(&w, "%s %d %s %d", label, log.repeat, strings.ToLower(log.address.Hex()), len(log.topics))
				} else {
					fmt.Fprintf(&w, "%s %s %d", label, strings.ToLower(log.address.Hex()), len(log.topics))
				}
				for _, topic := range log.topics {
					fmt.Fprintf(&w, " %s", topic.Hex())
				}
				fmt.Fprintln(&w)
			}
		}
		fmt.Fprintln(&w, "END_BLOCK")
	}
	fmt.Fprintln(&w, "END_BLOCKS")
	fmt.Fprintln(&w, "SUCCESSOR NONE")
	fmt.Fprintf(&w, "STREAM_BOUNDARIES %d\n", len(g.stream.boundaries))
	for _, boundary := range g.stream.boundaries {
		fmt.Fprintf(&w, "STREAM_BOUNDARY %d %d %s %s\n", boundary.mapIndex, boundary.resume, boundary.resumeHash.Hex(), boundary.ending)
	}
	fmt.Fprintln(&w, "END_STREAM_BOUNDARIES")
	fmt.Fprintf(&w, "POINTERS %d\n", len(g.stream.pointers))
	for _, pointer := range g.stream.pointers {
		fmt.Fprintf(&w, "POINTER %d %s %d\n", pointer.block, pointer.hash.Hex(), pointer.index)
	}
	fmt.Fprintln(&w, "END_POINTERS")
	fmt.Fprintf(&w, "COMPLETED_MAPS %d\n", len(g.rendered.completed))
	for _, observed := range g.rendered.completed {
		rows, marks := rowCounts(observed.rows)
		fmt.Fprintf(&w, "MAP %d %d %d %s %d %d\n", observed.mapIndex, observed.epoch, observed.lastBlock, observed.lastBlockHash.Hex(), rows, marks)
		writeNumberList(&w, "MAP_POINTER_BLOCKS", observed.pointerBlocks)
		writeRows(&w, observed.rows)
		if observed.boundary == nil {
			t.Fatalf("completed map %d has no boundary", observed.mapIndex)
		}
		fmt.Fprintf(&w, "BOUNDARY %d %s %d\n", observed.boundary.block, observed.boundary.hash.Hex(), observed.boundary.index)
		fmt.Fprintln(&w, "END_MAP")
	}
	fmt.Fprintln(&w, "END_COMPLETED_MAPS")
	fmt.Fprintf(&w, "PRIVATE_PARTIALS %d\n", len(g.rendered.partials))
	for _, observed := range g.rendered.partials {
		rows, marks := rowCounts(observed.rows)
		fmt.Fprintf(&w, "PRIVATE_PARTIAL %d %d %d %s %d %d %d\n", observed.mapIndex, observed.epoch, observed.lastBlock, observed.lastBlockHash.Hex(), observed.pendingDelimiter, rows, marks)
		writeRows(&w, observed.rows)
		fmt.Fprintln(&w, "END_PRIVATE_PARTIAL")
	}
	fmt.Fprintln(&w, "END_PRIVATE_PARTIALS")
	fmt.Fprintf(&w, "QUERIES %d\n", len(g.queries))
	for _, query := range g.queries {
		q := query.input
		fmt.Fprintf(&w, "QUERY %s %d %d\n", q.id, q.firstBlock, q.lastBlock)
		writeAddressList(&w, "ADDRESSES", q.addresses)
		fmt.Fprintf(&w, "TOPICS %d\n", len(q.topics))
		for position, alternatives := range q.topics {
			if len(alternatives) == 0 {
				fmt.Fprintf(&w, "TOPIC %d ANY\n", position)
			} else {
				fmt.Fprintf(&w, "TOPIC %d VALUES %d", position, len(alternatives))
				for _, topic := range alternatives {
					fmt.Fprintf(&w, " %s", topic.Hex())
				}
				fmt.Fprintln(&w)
			}
		}
		writeHashList(&w, "ADDRESS_VALUES", query.addressValues)
		for position, values := range query.topicValues {
			fmt.Fprintf(&w, "TOPIC_VALUES %d %d", position, len(values))
			for _, value := range values {
				fmt.Fprintf(&w, " %s", value.Hex())
			}
			fmt.Fprintln(&w)
		}
		fmt.Fprintf(&w, "INDEX_RANGE %d %d\n", query.firstIndex, query.lastIndex)
		fmt.Fprintf(&w, "MAP_RANGE %d %d\n", query.firstMap, query.lastMap)
		fmt.Fprintf(&w, "RESULT %s\n", query.result)
		writeNumberList(&w, "POTENTIAL_INDICES", query.potentialIndices)
		writeStringList(&w, "POTENTIAL_SLOT_CLASSES", query.slotClasses)
		writeNumberList(&w, "CANDIDATE_BLOCKS", query.candidateBlocks)
		writeStringList(&w, "POTENTIAL_LOGS", query.potentialLogs)
		writeStringList(&w, "EXACT_LOGS", query.exactLogs)
		writeNumberList(&w, "EXACT_BLOCKS", query.exactBlocks)
		fmt.Fprintf(&w, "PLANNER %s\n", query.planner)
		fmt.Fprintln(&w, "END_QUERY")
	}
	fmt.Fprintln(&w, "END_QUERIES")
	fmt.Fprintln(&w, "END_SCENARIO")
	return w.String()
}

func genericMatcherBlocks(first uint64) []pipelineBlock {
	return []pipelineBlock{
		pblock(first, plog("a", 1)),
		pblock(first+1, plog("b", 1)),
		pblock(first+2, plog("head", 0)),
	}
}
func syntheticOrigin(cursor uint64, reason string) pipelineOrigin {
	return pipelineOrigin{kind: "SYNTHETIC_CHECKPOINT", cursor: cursor, reason: reason}
}

func curatedPipelineScenarios() []pipelineScenario {
	const m = uint64(65536)
	a, b := plog("a", 1), plog("b", 1)
	allTopics := plog("four", 4)
	alternatives := plog("alternatives", 4)
	endToEndTopics := plog("end-to-end-topics", 4)
	endToEndBlock := pipelineBlock{number: 141, receipts: []pipelineReceipt{
		{}, {logs: []pipelineLog{endToEndTopics}}, {},
	}}
	return []pipelineScenario{
		{name: "empty-and-shared-map", class: classFocused, origin: syntheticOrigin(m-5, "empty-shared-map"), blocks: []pipelineBlock{pempty(10), pblock(11, plog("empty-shared-a", 0)), pblock(12, plog("empty-shared-b", 0)), pblock(13, plog("head", 0))}, queries: []pipelineQuery{{id: "shared-map", firstBlock: 10, lastBlock: 12}}},
		{name: "topic-cardinality", class: classFocused, origin: syntheticOrigin(m-16, "topic-cardinality"), blocks: []pipelineBlock{pblock(20, plog("zero", 0), plog("one", 1), plog("two", 2), plog("three", 3), allTopics), pblock(21, plog("head", 0))}, queries: []pipelineQuery{
			{id: "topic-position-0", firstBlock: 20, lastBlock: 20, topics: [][]common.Hash{{allTopics.topics[0]}}},
			{id: "topic-position-1", firstBlock: 20, lastBlock: 20, topics: [][]common.Hash{{}, {allTopics.topics[1]}}},
			{id: "topic-position-2", firstBlock: 20, lastBlock: 20, topics: [][]common.Hash{{}, {}, {allTopics.topics[2]}}},
			{id: "topic-position-3", firstBlock: 20, lastBlock: 20, topics: [][]common.Hash{{}, {}, {}, {allTopics.topics[3]}}},
			{id: "four-topics", firstBlock: 20, lastBlock: 20, addresses: []common.Address{allTopics.address}, topics: [][]common.Hash{{allTopics.topics[0]}, {allTopics.topics[1]}, {allTopics.topics[2]}, {allTopics.topics[3]}}},
		}},
		{name: "boundary-by-value", class: classFocused, origin: syntheticOrigin(m-1, "default-value-boundary"), blocks: []pipelineBlock{pblock(30, plog("boundary-value", 0)), pblock(31, plog("head", 0))}},
		{name: "boundary-by-delimiter", class: classFocused, origin: syntheticOrigin(m-2, "default-delimiter-boundary"), blocks: []pipelineBlock{pblock(40, plog("boundary-delimiter", 0)), pblock(41, plog("head", 0))}, queries: []pipelineQuery{{id: "delimiter-block", firstBlock: 40, lastBlock: 40, addresses: []common.Address{pipelineAddress("boundary-delimiter")}}}},
		{name: "boundary-by-padding", class: classFocused, origin: syntheticOrigin(m-4, "default-padding-boundary"), blocks: []pipelineBlock{pblock(50, plog("before-padding", 0), plog("after-padding", 3)), pblock(51, plog("head", 0))}},
		{name: "default-overflow", class: classFocused, origin: syntheticOrigin(m-138, "default-overflow"), blocks: []pipelineBlock{pblock(60, repeatedLog("overflow", 137, 0)), pblock(61, plog("head", 0))}, queries: []pipelineQuery{{id: "all-layers", firstBlock: 60, lastBlock: 60, addresses: []common.Address{pipelineAddress("overflow")}}}},
		{name: "epoch-boundary", class: classFocused, origin: syntheticOrigin(1024*m-1, "default-epoch-boundary"), blocks: []pipelineBlock{pblock(70, plog("epoch-last", 0)), pblock(71, plog("epoch-first", 1)), pblock(72, plog("head", 0))}},
		{name: "head-and-bounded-continuation", class: classEndToEnd, origin: pipelineOrigin{kind: "BATCH_CONTINUATION", cursor: m - 3, previousNumber: 79, previousPointer: m - 8}, blocks: []pipelineBlock{pblock(80, plog("continuation-wide", 3)), pblock(81, plog("continuation-next", 0)), pblock(82, plog("head", 0))}},
		{name: "matcher-address-and-topics", class: classFocused, origin: syntheticOrigin(m-9, "matcher-address-topics"), blocks: []pipelineBlock{pblock(90, allTopics), pblock(91, a), pblock(92, plog("head", 0))}, queries: []pipelineQuery{
			{id: "single-address", firstBlock: 90, lastBlock: 91, addresses: []common.Address{allTopics.address}},
			{id: "address-wildcard-topic", firstBlock: 90, lastBlock: 91, topics: [][]common.Hash{{allTopics.topics[0]}}},
			{id: "single-address-topic", firstBlock: 90, lastBlock: 91, addresses: []common.Address{allTopics.address}, topics: [][]common.Hash{{allTopics.topics[0]}}},
			{id: "topic-only", firstBlock: 90, lastBlock: 91, topics: [][]common.Hash{{a.topics[0]}}},
			{id: "four-topics", firstBlock: 90, lastBlock: 91, topics: [][]common.Hash{{allTopics.topics[0]}, {allTopics.topics[1]}, {allTopics.topics[2]}, {allTopics.topics[3]}}},
		}},
		{name: "matcher-alternatives-and-wildcards", class: classFocused, origin: syntheticOrigin(m-9, "matcher-alternatives"), blocks: []pipelineBlock{pblock(100, alternatives), pblock(101, b), pblock(102, plog("head", 0))}, queries: []pipelineQuery{
			{id: "address-alternatives", firstBlock: 100, lastBlock: 101, addresses: []common.Address{alternatives.address, b.address, alternatives.address}},
			{id: "topic-alternatives", firstBlock: 100, lastBlock: 101, topics: [][]common.Hash{{alternatives.topics[0], b.topics[0]}}},
			{id: "alternatives-each-position", firstBlock: 100, lastBlock: 101, topics: [][]common.Hash{
				{pipelineTopic("absent-0"), alternatives.topics[0]},
				{alternatives.topics[1], pipelineTopic("absent-1")},
				{pipelineTopic("absent-2"), alternatives.topics[2]},
				{alternatives.topics[3], pipelineTopic("absent-3")},
			}},
			{id: "internal-wildcard", firstBlock: 100, lastBlock: 101, topics: [][]common.Hash{{alternatives.topics[0], alternatives.topics[0]}, {}, {alternatives.topics[2]}}},
			{id: "omitted-trailing", firstBlock: 100, lastBlock: 101, topics: [][]common.Hash{{alternatives.topics[0]}}},
		}},
		{name: "matcher-no-match-and-dedup", class: classFocused, origin: syntheticOrigin(m-8, "matcher-no-match-dedup"), blocks: []pipelineBlock{pblock(110, repeatedLog("a", 2, 1)), pblock(111, b), pblock(112, plog("head", 0))}, queries: []pipelineQuery{
			{id: "no-match", firstBlock: 110, lastBlock: 111, addresses: []common.Address{pipelineAddress("absent")}},
			{id: "duplicate-alternatives", firstBlock: 110, lastBlock: 111, addresses: []common.Address{a.address, a.address}},
			{id: "match-all", firstBlock: 110, lastBlock: 111},
		}},
		// This fixed query address collides with block 121's `b` address mark at
		// absolute index 65533 (base row 3144, column 16776549) without being an
		// exact address match. Ordinary regeneration performs no collision search.
		{name: "matcher-false-positives", class: classFocused, origin: syntheticOrigin(m-6, "fixed-collision"), blocks: genericMatcherBlocks(120), queries: []pipelineQuery{{id: "fixed-collision", firstBlock: 120, lastBlock: 121, addresses: []common.Address{common.HexToAddress("0x0000000000000000000000000000000000bd7e30")}}}},
		{name: "positional-isolation", class: classFocused, origin: syntheticOrigin(m-8, "positional-isolation"), blocks: []pipelineBlock{pblock(130, a, b), pblock(131, plog("padding-trigger", 3)), pblock(132, plog("head", 0))}, queries: []pipelineQuery{
			{id: "collision-free-sequence", firstBlock: 130, lastBlock: 130, addresses: []common.Address{a.address}, topics: [][]common.Hash{{b.topics[0]}}},
			{id: "genuine-sequence", firstBlock: 130, lastBlock: 130, addresses: []common.Address{a.address}, topics: [][]common.Hash{{a.topics[0]}}},
		}},
		{name: "default-end-to-end", class: classEndToEnd, origin: syntheticOrigin(2*m-147, "default-end-to-end"), blocks: []pipelineBlock{pblock(140, repeatedLog("end-to-end-overflow", 140, 0)), endToEndBlock, pblock(142, plog("head", 0))}, queries: []pipelineQuery{
			{id: "overflow-address", firstBlock: 140, lastBlock: 140, addresses: []common.Address{pipelineAddress("end-to-end-overflow")}},
			{id: "four-topics", firstBlock: 140, lastBlock: 141, addresses: []common.Address{endToEndTopics.address}, topics: [][]common.Hash{{endToEndTopics.topics[0]}, {endToEndTopics.topics[1]}, {endToEndTopics.topics[2]}, {endToEndTopics.topics[3]}}},
		}},
	}
}

type splitMix64 struct{ state uint64 }

func (r *splitMix64) next() uint64 {
	r.state += 0x9e3779b97f4a7c15
	z := r.state
	z = (z ^ (z >> 30)) * 0xbf58476d1ce4e5b9
	z = (z ^ (z >> 27)) * 0x94d049bb133111eb
	return z ^ (z >> 31)
}
func stressPipelineScenarios() []pipelineScenario {
	rng := splitMix64{state: pipelineStressSeed}
	result := make([]pipelineScenario, 0, 16)
	for ordinal := uint64(0); ordinal < 16; ordinal++ {
		first := uint64(1000 + ordinal*10)
		topics := int(rng.next() % 5)
		secondTopics := int(rng.next() % 5)
		rangeParams := ordinal%4 == 0
		if rangeParams {
			// RangeTestParams has one value-space slot per map, so only one-value
			// logs are valid inputs under the shared whole-log boundary rule.
			topics, secondTopics = 0, 0
		}
		ordinalCopy := ordinal
		firstLog, secondLog := plog(fmt.Sprintf("stress-%02d-a", ordinal), topics), plog(fmt.Sprintf("stress-%02d-b", ordinal), secondTopics)
		queries := []pipelineQuery{{id: "address", firstBlock: first, lastBlock: first + 1, addresses: []common.Address{firstLog.address}}}
		if topics > 0 {
			queries = append(queries, pipelineQuery{id: "topic", firstBlock: first, lastBlock: first + 1, topics: [][]common.Hash{{firstLog.topics[0]}}})
		}
		if ordinal%5 == 0 {
			queries = append(queries, pipelineQuery{id: "all-blocks", firstBlock: first, lastBlock: first + 1})
		}
		firstBlock := pblock(first, firstLog)
		if ordinal%3 == 0 {
			firstBlock.receipts = []pipelineReceipt{{}, {logs: []pipelineLog{firstLog}}, {}}
		}
		secondBlock := pblock(first+1, secondLog)
		if ordinal%8 == 0 {
			secondBlock = pempty(first + 1)
		}
		cursor := ordinal * 32
		if !rangeParams {
			// Place the two fully queried blocks exactly at the end of a default map.
			// Their values and delimiters complete that map while the third block
			// leaves a private partial head map.
			consumed := uint64(topics + secondTopics + 4)
			cursor = (ordinal+1)*uint64(65536) - consumed
		}
		result = append(result, pipelineScenario{
			name: fmt.Sprintf("stress-%02d", ordinal), class: classStress, rangeParams: rangeParams, seedOrdinal: &ordinalCopy,
			origin: syntheticOrigin(cursor, "splitmix64-stress"),
			blocks: []pipelineBlock{firstBlock, secondBlock, pblock(first+2, plog(fmt.Sprintf("stress-%02d-head", ordinal), 0))}, queries: queries,
		})
	}
	return result
}

type manifestEntry struct {
	path, class, params                                   string
	bytes                                                 uint64
	hash                                                  common.Hash
	completed, partials, rows, marks, queries, potentials uint64
}

func manifestEntryFor(path string, generated generatedPipelineScenario) manifestEntry {
	entry := manifestEntry{path: path, class: string(generated.scenario.class), params: "DEFAULT", bytes: uint64(len(generated.text)), hash: sha256.Sum256([]byte(generated.text)), completed: uint64(len(generated.rendered.completed)), partials: uint64(len(generated.rendered.partials)), queries: uint64(len(generated.queries))}
	if generated.scenario.rangeParams {
		entry.params = "RANGE"
	}
	for _, m := range generated.rendered.completed {
		rows, marks := rowCounts(m.rows)
		entry.rows += rows
		entry.marks += marks
	}
	for _, m := range generated.rendered.partials {
		rows, marks := rowCounts(m.rows)
		entry.rows += rows
		entry.marks += marks
	}
	for _, q := range generated.queries {
		entry.potentials += uint64(len(q.potentialIndices))
	}
	return entry
}
func serializeManifest(entries []manifestEntry, oracleRevision string) string {
	var w strings.Builder
	fmt.Fprintln(&w, "PIPELINE_MANIFEST 1")
	fmt.Fprintln(&w, "FIXTURE_FORMAT 2")
	fmt.Fprintf(&w, "GETH %s\n", pipelineGethRevision)
	fmt.Fprintf(&w, "GENERATOR %s\n", oracleRevision)
	fmt.Fprintf(&w, "FILES %d\n", len(entries))
	var totals manifestEntry
	for _, e := range entries {
		fmt.Fprintf(&w, "FILE %s %s %s %d %x %d %d %d %d %d %d\n", e.path, e.class, e.params, e.bytes, e.hash, e.completed, e.partials, e.rows, e.marks, e.queries, e.potentials)
		totals.bytes += e.bytes
		totals.completed += e.completed
		totals.partials += e.partials
		totals.rows += e.rows
		totals.marks += e.marks
		totals.queries += e.queries
		totals.potentials += e.potentials
	}
	fmt.Fprintf(&w, "TOTALS %d %d %d %d %d %d %d\n", totals.completed, totals.partials, totals.rows, totals.marks, totals.queries, totals.potentials, totals.bytes)
	fmt.Fprintln(&w, "END_MANIFEST")
	return w.String()
}
func pipelineRelativePath(s pipelineScenario) string {
	switch s.class {
	case classFocused:
		return filepath.ToSlash(filepath.Join("curated", "focused", s.name+".txt"))
	case classEndToEnd:
		return filepath.ToSlash(filepath.Join("curated", "end_to_end", s.name+".txt"))
	default:
		return filepath.ToSlash(filepath.Join("stress", s.name+".txt"))
	}
}
func writeAtomicFile(path string, content []byte) error {
	if err := os.MkdirAll(filepath.Dir(path), 0755); err != nil {
		return err
	}
	tmp, err := os.CreateTemp(filepath.Dir(path), ".pipeline-*")
	if err != nil {
		return err
	}
	tmpName := tmp.Name()
	defer os.Remove(tmpName)
	if _, err = tmp.Write(content); err == nil {
		err = tmp.Chmod(0644)
	}
	if closeErr := tmp.Close(); err == nil {
		err = closeErr
	}
	if err != nil {
		return err
	}
	return os.Rename(tmpName, path)
}

func publishPipelineDirectory(staging, target string) error {
	parent := filepath.Dir(target)
	if err := os.MkdirAll(parent, 0755); err != nil {
		return err
	}
	backup, err := os.MkdirTemp(parent, ".pipeline-previous-*")
	if err != nil {
		return err
	}
	if err := os.Remove(backup); err != nil {
		return err
	}
	hadTarget := false
	if _, err := os.Stat(target); err == nil {
		hadTarget = true
		if err := os.Rename(target, backup); err != nil {
			return err
		}
	} else if !os.IsNotExist(err) {
		return err
	}
	if err := os.Rename(staging, target); err != nil {
		if hadTarget {
			_ = os.Rename(backup, target)
		}
		return err
	}
	if hadTarget {
		return os.RemoveAll(backup)
	}
	return nil
}

func TestGenPipeline(t *testing.T) {
	oracleRevision := os.Getenv("ORACLE_REV")
	if len(oracleRevision) != 40 {
		t.Fatal("ORACLE_REV must contain the exact generator commit; use regenerate.sh")
	}
	if _, err := hex.DecodeString(oracleRevision); err != nil {
		t.Fatal("ORACLE_REV must be 40 lowercase or uppercase hexadecimal digits")
	}
	out := os.Getenv("PIPELINE_OUT")
	if out == "" {
		t.Fatal("PIPELINE_OUT must name the fixture output root")
	}
	group := os.Getenv("PIPELINE_GROUP")
	if group == "" {
		group = "all"
	}
	if group != "all" && group != "curated" && group != "stress" {
		t.Fatalf("unknown PIPELINE_GROUP %q", group)
	}
	var scenarios []pipelineScenario
	if group == "all" || group == "curated" {
		scenarios = append(scenarios, curatedPipelineScenarios()...)
	}
	if group == "all" || group == "stress" {
		scenarios = append(scenarios, stressPipelineScenarios()...)
	}
	generated := make(map[string]generatedPipelineScenario)
	for _, scenario := range scenarios {
		scenario := scenario
		t.Run(scenario.name, func(t *testing.T) {
			first := generatePipelineScenario(t, scenario)
			second := generatePipelineScenario(t, scenario)
			if first.text != second.text {
				t.Fatal("repeated semantic generation was not byte-identical")
			}
			generated[scenario.name] = first
		})
	}
	if t.Failed() {
		return
	}
	paths := make([]string, 0, len(generated))
	for _, value := range generated {
		paths = append(paths, pipelineRelativePath(value.scenario))
	}
	sort.Strings(paths)
	entries := make([]manifestEntry, 0, len(paths))
	writeRoot := out
	if group == "all" {
		if err := os.MkdirAll(filepath.Dir(out), 0755); err != nil {
			t.Fatal(err)
		}
		staging, err := os.MkdirTemp(filepath.Dir(out), ".pipeline-next-*")
		if err != nil {
			t.Fatal(err)
		}
		defer os.RemoveAll(staging)
		writeRoot = staging
	}
	for _, relative := range paths {
		var value generatedPipelineScenario
		for _, candidate := range generated {
			if pipelineRelativePath(candidate.scenario) == relative {
				value = candidate
				break
			}
		}
		if err := writeAtomicFile(filepath.Join(writeRoot, filepath.FromSlash(relative)), []byte(value.text)); err != nil {
			t.Fatal(err)
		}
		entries = append(entries, manifestEntryFor(relative, value))
	}
	if group == "all" {
		if err := writeAtomicFile(filepath.Join(writeRoot, "MANIFEST.txt"), []byte(serializeManifest(entries, oracleRevision))); err != nil {
			t.Fatal(err)
		}
		if err := publishPipelineDirectory(writeRoot, out); err != nil {
			t.Fatal(err)
		}
	}
}
