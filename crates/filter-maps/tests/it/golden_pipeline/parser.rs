//! Strict parser and structural validation for `FORMAT 2` pipeline fixtures.
//!
//! Every record is matched exactly: canonical decimal numbers, lowercase fixed-width hex, single
//! spaces, a final newline, and declared counts that must equal the items that follow. Structural
//! validation covers the relationships that hold without replaying the stream; the `replay` module
//! covers everything that needs the actual events.

use alloy_primitives::{Address, B256};
use reth_filter_maps::{address_value, topic_value, Params, DEFAULT_PARAMS, RANGE_TEST_PARAMS};
use sha2::{Digest, Sha256};
use std::{collections::HashSet, fmt, str::FromStr};

/// The go-ethereum revision every fixture must name.
pub(super) const GETH_REVISION: &str = "af7c0fd8ee09de71b1034dbe6d1112556b49b59f";
/// The fork revision of the generator every fixture must name.
pub(super) const GENERATOR_REVISION: &str = "ce40051a0c308cb01df7005cb25e6481da78616f";
/// The `SplitMix64` seed shared by every stress fixture; the ordinal selects the sequence.
const STRESS_SEED: &str = "0xaf7c0fd8ee09de71";
const GENERATOR_PREFIX: &str = "https://github.com/0xAysh/reth/blob/";
const GENERATOR_SUFFIX: &str = "/tools/filtermaps-oracles/pipeline/gen_pipeline_test.go";
/// Ethereum's topic limit, which also bounds a query's positional constraints.
const MAX_TOPICS: usize = 4;

pub(super) type ParseResult<T> = Result<T, String>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FixtureClass {
    Focused,
    EndToEnd,
    Stress,
}

impl FixtureClass {
    pub(super) fn parse(token: &str) -> Option<Self> {
        match token {
            "FOCUSED" => Some(Self::Focused),
            "END_TO_END" => Some(Self::EndToEnd),
            "STRESS" => Some(Self::Stress),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ParamsName {
    Default,
    Range,
}

impl ParamsName {
    pub(super) fn parse(token: &str) -> Option<Self> {
        match token {
            "DEFAULT" => Some(Self::Default),
            "RANGE" => Some(Self::Range),
            _ => None,
        }
    }

    pub(super) const fn params(self) -> Params {
        match self {
            Self::Default => DEFAULT_PARAMS,
            Self::Range => RANGE_TEST_PARAMS,
        }
    }
}

/// A block identity bound to an absolute log value index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Pointer {
    pub(super) block: u64,
    pub(super) hash: B256,
    pub(super) index: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Origin {
    Genesis(Pointer),
    Checkpoint(Pointer),
    Continuation { block: u64, hash: B256, cursor: u64, previous: Pointer },
}

impl Origin {
    /// The identity of the first block the fixture feeds to the stream.
    pub(super) const fn first_block(&self) -> (u64, B256) {
        match self {
            Self::Genesis(anchor) | Self::Checkpoint(anchor) => (anchor.block, anchor.hash),
            Self::Continuation { block, hash, .. } => (*block, *hash),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Termination {
    Head,
    Batch { next_block: u64, next_hash: B256 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Log {
    pub(super) address: Address,
    pub(super) topics: Vec<B256>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Receipt {
    pub(super) logs: Vec<Log>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Block {
    pub(super) number: u64,
    pub(super) hash: B256,
    pub(super) receipts: Vec<Receipt>,
}

/// The kind of slot that completed a map.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum BoundaryEnding {
    Value,
    Delimiter,
    Padding,
}

impl BoundaryEnding {
    fn parse(token: &str) -> Option<Self> {
        match token {
            "VALUE" => Some(Self::Value),
            "DELIMITER" => Some(Self::Delimiter),
            "PADDING" => Some(Self::Padding),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct StreamBoundary {
    pub(super) map: u32,
    pub(super) block: u64,
    pub(super) hash: B256,
    pub(super) ending: BoundaryEnding,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Row {
    pub(super) index: u32,
    pub(super) columns: Vec<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct CompletedMap {
    pub(super) index: u32,
    pub(super) epoch: u32,
    pub(super) last_block: u64,
    pub(super) last_hash: B256,
    pub(super) pointer_blocks: Vec<u64>,
    pub(super) rows: Vec<Row>,
    pub(super) mark_count: usize,
    pub(super) boundary: Pointer,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct PartialMap {
    pub(super) index: u32,
    pub(super) epoch: u32,
    pub(super) last_block: u64,
    pub(super) last_hash: B256,
    pub(super) pending_delimiter: u64,
    pub(super) rows: Vec<Row>,
    pub(super) mark_count: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum TopicConstraint {
    Any,
    Values(Vec<B256>),
}

/// Classification of a candidate slot reported by the matcher.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SlotClass {
    Address,
    Topic(u8),
    Delimiter,
    Padding,
}

impl SlotClass {
    fn parse(token: &str) -> Option<Self> {
        match token {
            "A" => Some(Self::Address),
            "T0" => Some(Self::Topic(0)),
            "T1" => Some(Self::Topic(1)),
            "T2" => Some(Self::Topic(2)),
            "T3" => Some(Self::Topic(3)),
            "DELIMITER" => Some(Self::Delimiter),
            "PADDING" => Some(Self::Padding),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum QueryResult {
    Ok,
    ErrMatchAll,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Planner {
    Matcher,
    EveryBlock,
}

/// A log named by block number, receipt ordinal, and ordinal within the receipt.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct LogIdentity {
    pub(super) block: u64,
    pub(super) receipt: usize,
    pub(super) log: usize,
}

impl fmt::Display for LogIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}:{}", self.block, self.receipt, self.log)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Query {
    pub(super) id: String,
    pub(super) first_block: u64,
    pub(super) last_block: u64,
    pub(super) addresses: Vec<Address>,
    pub(super) topics: Vec<TopicConstraint>,
    pub(super) index_range: (u64, u64),
    pub(super) map_range: (u32, u32),
    pub(super) result: QueryResult,
    pub(super) potential_indices: Vec<u64>,
    pub(super) slot_classes: Vec<SlotClass>,
    pub(super) candidate_blocks: Vec<u64>,
    pub(super) potential_logs: Vec<LogIdentity>,
    pub(super) exact_logs: Vec<LogIdentity>,
    pub(super) exact_blocks: Vec<u64>,
    pub(super) planner: Planner,
}

impl Query {
    /// Whether Geth normalizes this query to `ErrMatchAll`: no address and no topic constraint.
    pub(super) const fn is_match_all(&self) -> bool {
        self.addresses.is_empty() && self.topics.is_empty()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Fixture {
    pub(super) scenario: String,
    pub(super) class: FixtureClass,
    pub(super) params_name: ParamsName,
    pub(super) stress_ordinal: Option<u64>,
    pub(super) origin: Origin,
    pub(super) termination: Termination,
    pub(super) blocks: Vec<Block>,
    pub(super) successor: Option<Block>,
    pub(super) boundaries: Vec<StreamBoundary>,
    pub(super) pointers: Vec<Pointer>,
    pub(super) completed_maps: Vec<CompletedMap>,
    pub(super) partial_maps: Vec<PartialMap>,
    pub(super) queries: Vec<Query>,
}

impl Fixture {
    pub(super) fn row_count(&self) -> usize {
        self.completed_maps.iter().map(|map| map.rows.len()).sum::<usize>() +
            self.partial_maps.iter().map(|map| map.rows.len()).sum::<usize>()
    }

    pub(super) fn mark_count(&self) -> usize {
        self.completed_maps.iter().map(|map| map.mark_count).sum::<usize>() +
            self.partial_maps.iter().map(|map| map.mark_count).sum::<usize>()
    }

    pub(super) fn potential_count(&self) -> usize {
        self.queries.iter().map(|query| query.potential_indices.len()).sum()
    }

    pub(super) fn pointer(&self, block: u64) -> Option<&Pointer> {
        self.pointers.iter().find(|pointer| pointer.block == block)
    }

    pub(super) fn log(&self, identity: LogIdentity) -> Option<&Log> {
        let block = self.blocks.iter().find(|block| block.number == identity.block)?;
        block.receipts.get(identity.receipt)?.logs.get(identity.log)
    }
}

/// Line cursor over fixture text that enforces the canonical single-space layout.
pub(super) struct Lines<'a> {
    path: &'a str,
    lines: Vec<&'a str>,
    position: usize,
}

impl<'a> Lines<'a> {
    pub(super) fn new(path: &'a str, text: &'a str) -> ParseResult<Self> {
        let Some(body) = text.strip_suffix('\n') else {
            return Err(format!("{path}: missing final newline"))
        };
        let lines = body.split('\n').collect::<Vec<_>>();
        for (offset, line) in lines.iter().enumerate() {
            if line.is_empty() ||
                !line.is_ascii() ||
                line.bytes().any(|byte| byte.is_ascii_control()) ||
                line.starts_with(' ') ||
                line.ends_with(' ') ||
                line.contains("  ")
            {
                return Err(format!("{path}:{}: non-canonical line", offset + 1))
            }
        }
        Ok(Self { path, lines, position: 0 })
    }

    /// Prefixes a message with the location of the most recently consumed line.
    pub(super) fn error(&self, message: impl fmt::Display) -> String {
        format!("{}:{}: {message}", self.path, self.position.max(1))
    }

    /// Attaches the current location to a field-level error.
    pub(super) fn located<T>(&self, result: ParseResult<T>) -> ParseResult<T> {
        result.map_err(|message| self.error(message))
    }

    pub(super) fn next(&mut self) -> ParseResult<Vec<&'a str>> {
        let Some(line) = self.lines.get(self.position).copied() else {
            return Err(self.error("unexpected end of fixture"))
        };
        self.position += 1;
        Ok(line.split(' ').collect())
    }

    /// Consumes the next line, which must start with the `expected` record name.
    pub(super) fn record(&mut self, expected: &str) -> ParseResult<Vec<&'a str>> {
        let fields = self.next()?;
        if fields[0] != expected {
            let found = fields.join(" ");
            return Err(self.error(format!("expected {expected}, found {found}")))
        }
        Ok(fields)
    }

    /// Consumes the next line, which must consist of exactly the `expected` fields.
    pub(super) fn exact(&mut self, expected: &[&str]) -> ParseResult<()> {
        let fields = self.next()?;
        if fields != expected {
            let (expected, found) = (expected.join(" "), fields.join(" "));
            return Err(self.error(format!("expected {expected}, found {found}")))
        }
        Ok(())
    }

    pub(super) fn done(&self) -> ParseResult<()> {
        if self.position != self.lines.len() {
            return Err(format!("{}:{}: unexpected trailing records", self.path, self.position + 1))
        }
        Ok(())
    }
}

fn invalid(what: &str, token: &str) -> String {
    format!("invalid {what}: {token}")
}

/// Parses a canonical decimal number: digits only, with no leading zero unless it is `0`.
pub(super) fn number<T: FromStr>(token: &str, what: &str) -> ParseResult<T> {
    let digits = !token.is_empty() && token.bytes().all(|byte| byte.is_ascii_digit());
    if !digits || (token != "0" && token.starts_with('0')) {
        return Err(format!("non-canonical {what}: {token}"))
    }
    token.parse().map_err(|_| format!("{what} out of range: {token}"))
}

pub(super) fn lowercase_hex(raw: &str) -> bool {
    raw.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn fixed_hex(token: &str, digits: usize, what: &str) -> ParseResult<()> {
    match token.strip_prefix("0x") {
        Some(raw) if raw.len() == digits && lowercase_hex(raw) => Ok(()),
        _ => Err(format!("malformed {what}: {token}")),
    }
}

pub(super) fn hash(token: &str) -> ParseResult<B256> {
    fixed_hex(token, 64, "hash")?;
    token.parse().map_err(|_| format!("malformed hash: {token}"))
}

fn address(token: &str) -> ParseResult<Address> {
    fixed_hex(token, 40, "address")?;
    token.parse().map_err(|_| format!("malformed address: {token}"))
}

/// Parses a full-length lowercase git revision.
pub(super) fn git_revision(token: &str, what: &str) -> ParseResult<String> {
    if token.len() != 40 || !lowercase_hex(token) {
        return Err(format!("malformed {what}: {token}"))
    }
    Ok(token.to_owned())
}

/// Parses a kebab-case identifier: lowercase ASCII letters, digits, and single interior dashes.
pub(super) fn identifier(token: &str, what: &str) -> ParseResult<String> {
    let alphabet = |byte: u8| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-';
    if token.is_empty() ||
        token.starts_with('-') ||
        token.ends_with('-') ||
        token.contains("--") ||
        !token.bytes().all(alphabet)
    {
        return Err(invalid(what, token))
    }
    Ok(token.to_owned())
}

fn exact_fields(fields: &[&str], count: usize, record: &str) -> ParseResult<()> {
    if fields.len() == count {
        Ok(())
    } else {
        Err(format!("{record} has {} fields, expected {count}", fields.len()))
    }
}

/// Parses `<count> <item>...` starting at `start`, requiring exactly `count` items.
pub(super) fn counted<T>(
    fields: &[&str],
    start: usize,
    record: &str,
    parse: impl Fn(&str) -> ParseResult<T>,
) -> ParseResult<Vec<T>> {
    let Some(declared) = fields.get(start) else {
        return Err(format!("{record} is missing its count"))
    };
    let count: usize = number(declared, "count")?;
    let items = &fields[start + 1..];
    if items.len() != count {
        return Err(format!("{record} declares {count} items but lists {}", items.len()))
    }
    items.iter().copied().map(parse).collect()
}

fn numbers<T: FromStr>(fields: &[&str], record: &str) -> ParseResult<Vec<T>> {
    counted(fields, 1, record, |token| number(token, record))
}

fn log_identity(token: &str) -> ParseResult<LogIdentity> {
    let parts = token.split(':').collect::<Vec<_>>();
    let [block, receipt, log] = parts.as_slice() else {
        return Err(format!("malformed log identity: {token}"))
    };
    Ok(LogIdentity {
        block: number(block, "log identity block")?,
        receipt: number(receipt, "log identity receipt")?,
        log: number(log, "log identity log")?,
    })
}

/// The deterministic hash the generator assigns to every synthetic block.
pub(super) fn canonical_block_hash(number: u64) -> B256 {
    B256::from_slice(&Sha256::digest(format!("canonical-block-{number}")))
}

/// Consumes a two-field record and returns its value.
fn single<'a>(lines: &mut Lines<'a>, record: &str) -> ParseResult<&'a str> {
    let fields = lines.record(record)?;
    lines.located(exact_fields(&fields, 2, record))?;
    Ok(fields[1])
}

/// Consumes `<header> <count>`, `count` items, and the `end` record.
fn section<'a, T>(
    lines: &mut Lines<'a>,
    header: &str,
    end: &str,
    mut item: impl FnMut(&mut Lines<'a>) -> ParseResult<T>,
) -> ParseResult<Vec<T>> {
    let declared = single(lines, header)?;
    let count: usize = lines.located(number(declared, "section count"))?;
    let mut items = Vec::new();
    for _ in 0..count {
        items.push(item(lines)?);
    }
    lines.exact(&[end])?;
    Ok(items)
}

fn parse_provenance(lines: &mut Lines<'_>) -> ParseResult<String> {
    lines.exact(&["#", "Geth", GETH_REVISION])?;
    let fields = lines.record("#")?;
    let [_, "Generator", url] = fields.as_slice() else {
        return Err(lines.error("expected generator provenance"))
    };
    let Some(rest) = url.strip_prefix(GENERATOR_PREFIX) else {
        return Err(lines.error("unrecognized generator location"))
    };
    let Some(revision) = rest.strip_suffix(GENERATOR_SUFFIX) else {
        return Err(lines.error("unrecognized generator location"))
    };
    lines.located(git_revision(revision, "generator revision"))
}

fn stress_ordinal_from(class: FixtureClass, seed: &[&str]) -> ParseResult<Option<u64>> {
    match (class, seed) {
        (FixtureClass::Stress, ["SEED", "SPLITMIX64", STRESS_SEED, ordinal]) => {
            Ok(Some(number(ordinal, "stress ordinal")?))
        }
        (FixtureClass::Stress, _) => Err("stress fixture must use the pinned seed".to_owned()),
        (_, ["SEED", "NONE"]) => Ok(None),
        _ => Err("curated fixture must use SEED NONE".to_owned()),
    }
}

fn pointer_from(block: &str, block_hash: &str, index: &str) -> ParseResult<Pointer> {
    Ok(Pointer {
        block: number(block, "block number")?,
        hash: hash(block_hash)?,
        index: number(index, "log value index")?,
    })
}

fn origin_from(fields: &[&str]) -> ParseResult<Origin> {
    match fields {
        ["ORIGIN", "GENESIS", block, block_hash, index] => {
            let anchor = pointer_from(block, block_hash, index)?;
            if anchor.block != 0 || anchor.index != 0 {
                return Err("GENESIS origin must start at block zero and index zero".to_owned())
            }
            Ok(Origin::Genesis(anchor))
        }
        ["ORIGIN", "SYNTHETIC_CHECKPOINT", block, block_hash, index, reason] => {
            identifier(reason, "anchor reason")?;
            Ok(Origin::Checkpoint(pointer_from(block, block_hash, index)?))
        }
        ["ORIGIN", "BATCH_CONTINUATION", block, block_hash, cursor, previous @ ..] => {
            let [previous_block, previous_hash, previous_index] = previous else {
                return Err("malformed BATCH_CONTINUATION origin".to_owned())
            };
            Ok(Origin::Continuation {
                block: number(block, "continuation block")?,
                hash: hash(block_hash)?,
                cursor: number(cursor, "continuation cursor")?,
                previous: pointer_from(previous_block, previous_hash, previous_index)?,
            })
        }
        _ => Err("malformed ORIGIN".to_owned()),
    }
}

fn termination_from(fields: &[&str]) -> ParseResult<Termination> {
    match fields {
        ["TERMINATION", "HEAD"] => Ok(Termination::Head),
        ["TERMINATION", "BATCH", block, block_hash] => Ok(Termination::Batch {
            next_block: number(block, "successor block")?,
            next_hash: hash(block_hash)?,
        }),
        _ => Err("malformed TERMINATION".to_owned()),
    }
}

/// Parses a `LOG` or `REPEAT_LOG` record into a repeat count and the repeated log.
fn log_from(fields: &[&str]) -> ParseResult<(usize, Log)> {
    let (repeat, rest) = match fields {
        ["LOG", rest @ ..] => (1, rest),
        ["REPEAT_LOG", count, rest @ ..] => (number(count, "repeat count")?, rest),
        _ => return Err(format!("expected LOG or REPEAT_LOG, found {}", fields.join(" "))),
    };
    if repeat == 0 {
        return Err("REPEAT_LOG count must be nonzero".to_owned())
    }
    let [emitter, topic_count, topics @ ..] = rest else { return Err("malformed LOG".to_owned()) };
    let topic_count: usize = number(topic_count, "topic count")?;
    if topic_count > MAX_TOPICS {
        return Err(format!("LOG declares {topic_count} topics, more than {MAX_TOPICS}"))
    }
    if topics.len() != topic_count {
        return Err(format!("LOG declares {topic_count} topics but lists {}", topics.len()))
    }
    let topics = topics.iter().copied().map(hash).collect::<ParseResult<Vec<_>>>()?;
    Ok((repeat, Log { address: address(emitter)?, topics }))
}

fn parse_receipt(lines: &mut Lines<'_>) -> ParseResult<Receipt> {
    let fields = lines.record("RECEIPT")?;
    lines.located(exact_fields(&fields, 2, "RECEIPT"))?;
    let log_count: usize = lines.located(number(fields[1], "log count"))?;
    let mut logs = Vec::new();
    while logs.len() < log_count {
        let fields = lines.next()?;
        let (repeat, log) = lines.located(log_from(&fields))?;
        if logs.len() + repeat > log_count {
            return Err(lines.error("REPEAT_LOG exceeds the receipt's declared log count"))
        }
        logs.extend(std::iter::repeat_n(log, repeat));
    }
    Ok(Receipt { logs })
}

/// Parses a block whose header record has already been consumed into `fields`.
fn parse_block(lines: &mut Lines<'_>, fields: &[&str], end: &str) -> ParseResult<Block> {
    lines.located(exact_fields(fields, 4, fields[0]))?;
    let block_number = lines.located(number(fields[1], "block number"))?;
    let block_hash = lines.located(hash(fields[2]))?;
    let receipt_count: usize = lines.located(number(fields[3], "receipt count"))?;
    let mut receipts = Vec::new();
    for _ in 0..receipt_count {
        receipts.push(parse_receipt(lines)?);
    }
    lines.exact(&[end])?;
    Ok(Block { number: block_number, hash: block_hash, receipts })
}

/// Parses the expanded block section, including lossless runs of canonical empty blocks.
fn parse_blocks(lines: &mut Lines<'_>) -> ParseResult<Vec<Block>> {
    let declared = single(lines, "BLOCKS")?;
    let declared: usize = lines.located(number(declared, "expanded block count"))?;
    let mut blocks = Vec::new();
    while blocks.len() < declared {
        let fields = lines.next()?;
        match fields.as_slice() {
            ["BLOCK", ..] => blocks.push(parse_block(lines, &fields, "END_BLOCK")?),
            ["EMPTY_BLOCK_RUN", first, count] => {
                let first: u64 = lines.located(number(first, "empty-run first block"))?;
                let count: usize = lines.located(number(count, "empty-run count"))?;
                if count == 0 {
                    return Err(lines.error("EMPTY_BLOCK_RUN count must be nonzero"))
                }
                if blocks.len().checked_add(count).is_none_or(|total| total > declared) {
                    return Err(lines.error("EMPTY_BLOCK_RUN exceeds the expanded block count"))
                }
                for offset in 0..count {
                    let offset = u64::try_from(offset)
                        .map_err(|_| lines.error("EMPTY_BLOCK_RUN count exceeds u64"))?;
                    let number = first
                        .checked_add(offset)
                        .ok_or_else(|| lines.error("EMPTY_BLOCK_RUN block number overflow"))?;
                    blocks.push(Block {
                        number,
                        hash: canonical_block_hash(number),
                        receipts: Vec::new(),
                    });
                }
            }
            _ => {
                return Err(lines.error(format!(
                    "expected BLOCK or EMPTY_BLOCK_RUN, found {}",
                    fields.join(" ")
                )))
            }
        }
    }
    lines.exact(&["END_BLOCKS"])?;
    Ok(blocks)
}

fn boundary_from(fields: &[&str]) -> ParseResult<StreamBoundary> {
    let ["STREAM_BOUNDARY", map, block, block_hash, ending] = fields else {
        return Err("malformed STREAM_BOUNDARY".to_owned())
    };
    Ok(StreamBoundary {
        map: number(map, "boundary map")?,
        block: number(block, "boundary block")?,
        hash: hash(block_hash)?,
        ending: BoundaryEnding::parse(ending).ok_or_else(|| invalid("boundary ending", ending))?,
    })
}

struct MapHeader {
    index: u32,
    epoch: u32,
    last_block: u64,
    last_hash: B256,
    rows: usize,
    marks: usize,
}

fn map_header_from(fields: &[&str], record: &str) -> ParseResult<MapHeader> {
    let [index, epoch, last_block, last_hash, rows, marks] = fields else {
        return Err(format!("malformed {record}"))
    };
    Ok(MapHeader {
        index: number(index, "map index")?,
        epoch: number(epoch, "map epoch")?,
        last_block: number(last_block, "map last block")?,
        last_hash: hash(last_hash)?,
        rows: number(rows, "row count")?,
        marks: number(marks, "mark count")?,
    })
}

fn row_from(fields: &[&str], params: Params) -> ParseResult<Row> {
    if fields[0] != "ROW" {
        return Err(format!("expected ROW or the map terminator, found {}", fields.join(" ")))
    }
    let Some(index) = fields.get(1) else { return Err("malformed ROW".to_owned()) };
    let index: u32 = number(index, "row index")?;
    if index >= params.map_height() {
        return Err(format!("row index {index} exceeds the map height"))
    }
    let columns = counted(fields, 2, "ROW", |token| number::<u32>(token, "column"))?;
    if columns.is_empty() {
        return Err(format!("row {index} lists no columns"))
    }
    if let Some(column) = columns.iter().find(|column| **column >= params.map_width()) {
        return Err(format!("column {column} exceeds the map width"))
    }
    Ok(Row { index, columns })
}

/// Reads `ROW` records up to the `end` record, returning the rows and the `end` record's fields.
fn parse_rows<'a>(
    lines: &mut Lines<'a>,
    end: &str,
    header: &MapHeader,
    params: Params,
) -> ParseResult<(Vec<Row>, Vec<&'a str>)> {
    let mut rows: Vec<Row> = Vec::new();
    let mut marks = 0;
    loop {
        let fields = lines.next()?;
        if fields[0] == end {
            if rows.len() != header.rows || marks != header.marks {
                let declared = format!("{}/{}", header.rows, header.marks);
                let found = format!("{}/{marks}", rows.len());
                return Err(lines.error(format!("declared {declared} rows/marks, found {found}")))
            }
            return Ok((rows, fields))
        }
        let row = lines.located(row_from(&fields, params))?;
        if rows.last().is_some_and(|previous| previous.index >= row.index) {
            return Err(lines.error("rows must be strictly ascending"))
        }
        marks += row.columns.len();
        rows.push(row);
    }
}

fn parse_completed_map(lines: &mut Lines<'_>, params: Params) -> ParseResult<CompletedMap> {
    let fields = lines.record("MAP")?;
    let header = lines.located(map_header_from(&fields[1..], "MAP"))?;
    let fields = lines.record("MAP_POINTER_BLOCKS")?;
    let pointer_blocks = lines.located(numbers(&fields, "MAP_POINTER_BLOCKS"))?;
    let (rows, fields) = parse_rows(lines, "BOUNDARY", &header, params)?;
    let boundary = match fields.as_slice() {
        ["BOUNDARY", block, block_hash, index] => {
            lines.located(pointer_from(block, block_hash, index))?
        }
        _ => return Err(lines.error("malformed BOUNDARY")),
    };
    lines.exact(&["END_MAP"])?;
    Ok(CompletedMap {
        index: header.index,
        epoch: header.epoch,
        last_block: header.last_block,
        last_hash: header.last_hash,
        pointer_blocks,
        rows,
        mark_count: header.marks,
        boundary,
    })
}

fn parse_partial_map(lines: &mut Lines<'_>, params: Params) -> ParseResult<PartialMap> {
    let fields = lines.record("PRIVATE_PARTIAL")?;
    lines.located(exact_fields(&fields, 8, "PRIVATE_PARTIAL"))?;
    let pending_delimiter = lines.located(number(fields[5], "pending delimiter"))?;
    let mut header_fields = fields[1..5].to_vec();
    header_fields.extend_from_slice(&fields[6..]);
    let header = lines.located(map_header_from(&header_fields, "PRIVATE_PARTIAL"))?;
    let (rows, fields) = parse_rows(lines, "END_PRIVATE_PARTIAL", &header, params)?;
    lines.located(exact_fields(&fields, 1, "END_PRIVATE_PARTIAL"))?;
    Ok(PartialMap {
        index: header.index,
        epoch: header.epoch,
        last_block: header.last_block,
        last_hash: header.last_hash,
        pending_delimiter,
        rows,
        mark_count: header.marks,
    })
}

fn topic_from(fields: &[&str], position: usize) -> ParseResult<TopicConstraint> {
    let [_, declared, constraint @ ..] = fields else { return Err("malformed TOPIC".to_owned()) };
    if number::<usize>(declared, "topic position")? != position {
        return Err(format!("expected TOPIC {position}, found TOPIC {declared}"))
    }
    match constraint {
        ["ANY"] => Ok(TopicConstraint::Any),
        ["VALUES", rest @ ..] => {
            let values = counted(rest, 0, "TOPIC VALUES", hash)?;
            if values.is_empty() {
                return Err("TOPIC VALUES must list at least one topic".to_owned())
            }
            Ok(TopicConstraint::Values(values))
        }
        _ => Err("malformed TOPIC constraint".to_owned()),
    }
}

fn topic_values_from(fields: &[&str], position: usize) -> ParseResult<Vec<B256>> {
    let [_, declared, rest @ ..] = fields else { return Err("malformed TOPIC_VALUES".to_owned()) };
    if number::<usize>(declared, "topic-value position")? != position {
        return Err(format!("expected TOPIC_VALUES {position}, found TOPIC_VALUES {declared}"))
    }
    counted(rest, 0, "TOPIC_VALUES", hash)
}

fn range_from<T: FromStr>(fields: &[&str], record: &str) -> ParseResult<(T, T)> {
    let [_, first, last] = fields else { return Err(format!("malformed {record}")) };
    Ok((number(first, record)?, number(last, record)?))
}

fn parse_query(lines: &mut Lines<'_>) -> ParseResult<Query> {
    let fields = lines.record("QUERY")?;
    lines.located(exact_fields(&fields, 4, "QUERY"))?;
    let id = lines.located(identifier(fields[1], "query id"))?;
    let first_block = lines.located(number(fields[2], "query first block"))?;
    let last_block = lines.located(number(fields[3], "query last block"))?;

    let fields = lines.record("ADDRESSES")?;
    let addresses = lines.located(counted(&fields, 1, "ADDRESSES", address))?;
    let declared = single(lines, "TOPICS")?;
    let topic_length: usize = lines.located(number(declared, "topic length"))?;
    if topic_length > MAX_TOPICS {
        return Err(lines.error(format!("TOPICS declares {topic_length}, more than {MAX_TOPICS}")))
    }
    let mut topics = Vec::new();
    for position in 0..topic_length {
        let fields = lines.record("TOPIC")?;
        topics.push(lines.located(topic_from(&fields, position))?);
    }

    let fields = lines.record("ADDRESS_VALUES")?;
    let address_values = lines.located(counted(&fields, 1, "ADDRESS_VALUES", hash))?;
    let expected = addresses.iter().copied().map(address_value).collect::<Vec<_>>();
    if address_values != expected {
        return Err(lines.error("ADDRESS_VALUES are not the derived address values"))
    }
    for (position, constraint) in topics.iter().enumerate() {
        let fields = lines.record("TOPIC_VALUES")?;
        let values = lines.located(topic_values_from(&fields, position))?;
        let expected = match constraint {
            TopicConstraint::Any => Vec::new(),
            TopicConstraint::Values(raw) => raw.iter().copied().map(topic_value).collect(),
        };
        if values != expected {
            return Err(lines.error("TOPIC_VALUES are not the derived topic values"))
        }
    }

    let fields = lines.record("INDEX_RANGE")?;
    let index_range = lines.located(range_from(&fields, "INDEX_RANGE"))?;
    let fields = lines.record("MAP_RANGE")?;
    let map_range = lines.located(range_from(&fields, "MAP_RANGE"))?;
    let result = match single(lines, "RESULT")? {
        "OK" => QueryResult::Ok,
        "ERR_MATCH_ALL" => QueryResult::ErrMatchAll,
        other => return Err(lines.error(invalid("query result", other))),
    };

    let fields = lines.record("POTENTIAL_INDICES")?;
    let potential_indices = lines.located(numbers(&fields, "POTENTIAL_INDICES"))?;
    let fields = lines.record("POTENTIAL_SLOT_CLASSES")?;
    let slot_classes = lines.located(counted(&fields, 1, "POTENTIAL_SLOT_CLASSES", |token| {
        SlotClass::parse(token).ok_or_else(|| invalid("slot class", token))
    }))?;
    let fields = lines.record("CANDIDATE_BLOCKS")?;
    let candidate_blocks = lines.located(numbers(&fields, "CANDIDATE_BLOCKS"))?;
    let fields = lines.record("POTENTIAL_LOGS")?;
    let potential_logs = lines.located(counted(&fields, 1, "POTENTIAL_LOGS", log_identity))?;
    let fields = lines.record("EXACT_LOGS")?;
    let exact_logs = lines.located(counted(&fields, 1, "EXACT_LOGS", log_identity))?;
    let fields = lines.record("EXACT_BLOCKS")?;
    let exact_blocks = lines.located(numbers(&fields, "EXACT_BLOCKS"))?;
    let planner = match single(lines, "PLANNER")? {
        "MATCHER" => Planner::Matcher,
        "EVERY_BLOCK" => Planner::EveryBlock,
        other => return Err(lines.error(invalid("planner", other))),
    };
    lines.exact(&["END_QUERY"])?;

    Ok(Query {
        id,
        first_block,
        last_block,
        addresses,
        topics,
        index_range,
        map_range,
        result,
        potential_indices,
        slot_classes,
        candidate_blocks,
        potential_logs,
        exact_logs,
        exact_blocks,
        planner,
    })
}

/// Parses and structurally validates one fixture; `path` only labels error messages.
pub(super) fn parse_fixture(path: &str, text: &str) -> ParseResult<Fixture> {
    let mut lines = Lines::new(path, text)?;
    let generator = parse_provenance(&mut lines)?;
    if generator != GENERATOR_REVISION {
        return Err(lines.error(format!("generator revision {generator} is not the pinned one")))
    }
    lines.exact(&["FORMAT", "2"])?;
    let token = single(&mut lines, "SCENARIO")?;
    let scenario = lines.located(identifier(token, "scenario id"))?;
    let token = single(&mut lines, "CLASS")?;
    let class = FixtureClass::parse(token).ok_or_else(|| lines.error(invalid("class", token)))?;
    let token = single(&mut lines, "PARAMS")?;
    let params_name =
        ParamsName::parse(token).ok_or_else(|| lines.error(invalid("params", token)))?;
    let params = params_name.params();
    lines.exact(&["VALUE_SPACE", "GETH_V1"])?;
    let fields = lines.record("SEED")?;
    let stress_ordinal = lines.located(stress_ordinal_from(class, &fields))?;
    let fields = lines.record("ORIGIN")?;
    let origin = lines.located(origin_from(&fields))?;
    let fields = lines.record("TERMINATION")?;
    let termination = lines.located(termination_from(&fields))?;

    let blocks = parse_blocks(&mut lines)?;
    let fields = lines.record("SUCCESSOR")?;
    let successor = if fields == ["SUCCESSOR", "NONE"] {
        None
    } else {
        Some(parse_block(&mut lines, &fields, "END_SUCCESSOR")?)
    };
    let boundaries = section(&mut lines, "STREAM_BOUNDARIES", "END_STREAM_BOUNDARIES", |lines| {
        let fields = lines.record("STREAM_BOUNDARY")?;
        lines.located(boundary_from(&fields))
    })?;
    let pointers = section(&mut lines, "POINTERS", "END_POINTERS", |lines| {
        let fields = lines.record("POINTER")?;
        match fields.as_slice() {
            ["POINTER", block, block_hash, index] => {
                lines.located(pointer_from(block, block_hash, index))
            }
            _ => Err(lines.error("malformed POINTER")),
        }
    })?;
    let completed_maps = section(&mut lines, "COMPLETED_MAPS", "END_COMPLETED_MAPS", |lines| {
        parse_completed_map(lines, params)
    })?;
    let partial_maps = section(&mut lines, "PRIVATE_PARTIALS", "END_PRIVATE_PARTIALS", |lines| {
        parse_partial_map(lines, params)
    })?;
    let queries = section(&mut lines, "QUERIES", "END_QUERIES", parse_query)?;
    lines.exact(&["END_SCENARIO"])?;
    lines.done()?;

    let fixture = Fixture {
        scenario,
        class,
        params_name,
        stress_ordinal,
        origin,
        termination,
        blocks,
        successor,
        boundaries,
        pointers,
        completed_maps,
        partial_maps,
        queries,
    };
    validate_fixture(path, &fixture)?;
    Ok(fixture)
}

fn strictly_ascending<T: Ord>(values: &[T]) -> bool {
    values.windows(2).all(|pair| pair[0] < pair[1])
}

fn validate_blocks(fixture: &Fixture) -> ParseResult<()> {
    let Some(first) = fixture.blocks.first() else {
        return Err("fixture has no blocks".to_owned())
    };
    for pair in fixture.blocks.windows(2) {
        let (previous, next) = (pair[0].number, pair[1].number);
        if previous + 1 != next {
            return Err(format!("block {next} does not follow block {previous}"))
        }
    }
    let synthetic = fixture.blocks.iter().chain(&fixture.successor);
    for block in synthetic {
        if block.hash != canonical_block_hash(block.number) {
            return Err(format!("block {} does not carry its canonical hash", block.number))
        }
    }
    if fixture.origin.first_block() != (first.number, first.hash) {
        return Err("ORIGIN does not name the first block".to_owned())
    }
    let Some(first_pointer) = fixture.pointers.first() else {
        return Err("fixture has no block pointers".to_owned())
    };
    match fixture.origin {
        Origin::Genesis(anchor) | Origin::Checkpoint(anchor) => {
            if *first_pointer != anchor {
                return Err("ORIGIN pointer disagrees with the POINTERS table".to_owned())
            }
        }
        Origin::Continuation { block, cursor, previous, .. } => {
            if previous.block + 1 != block {
                return Err("BATCH_CONTINUATION previous block is not the parent".to_owned())
            }
            if previous.hash != canonical_block_hash(previous.block) {
                return Err("BATCH_CONTINUATION previous block has a non-canonical hash".to_owned())
            }
            if previous.index >= cursor || cursor > first_pointer.index {
                return Err("BATCH_CONTINUATION cursor is out of order".to_owned())
            }
        }
    }
    let last = fixture.blocks.last().expect("checked above");
    let consistent = match (fixture.termination, &fixture.successor) {
        (Termination::Head, None) => true,
        (Termination::Batch { next_block, next_hash }, Some(successor)) => {
            successor.number == next_block &&
                successor.hash == next_hash &&
                last.number + 1 == next_block
        }
        _ => false,
    };
    if !consistent {
        return Err("TERMINATION and SUCCESSOR disagree".to_owned())
    }
    Ok(())
}

fn validate_pointers(fixture: &Fixture) -> ParseResult<()> {
    if fixture.pointers.len() != fixture.blocks.len() {
        return Err("POINTERS must list exactly one pointer per block".to_owned())
    }
    for (pointer, block) in fixture.pointers.iter().zip(&fixture.blocks) {
        if (pointer.block, pointer.hash) != (block.number, block.hash) {
            return Err(format!("POINTER {} does not match its block", pointer.block))
        }
    }
    if !strictly_ascending(&fixture.pointers.iter().map(|p| p.index).collect::<Vec<_>>()) {
        return Err("POINTERS indices must be strictly ascending".to_owned())
    }
    if !strictly_ascending(&fixture.boundaries.iter().map(|b| b.map).collect::<Vec<_>>()) {
        return Err("STREAM_BOUNDARIES must be strictly ascending".to_owned())
    }
    for boundary in &fixture.boundaries {
        let resolved = fixture.pointer(boundary.block).is_some_and(|p| p.hash == boundary.hash);
        if !resolved {
            return Err(format!("STREAM_BOUNDARY {} names an unknown block", boundary.map))
        }
    }
    Ok(())
}

fn validate_maps(fixture: &Fixture) -> ParseResult<()> {
    let params = fixture.params_name.params();
    let mut seen = HashSet::new();
    for map in &fixture.completed_maps {
        if !seen.insert(map.index) {
            return Err(format!("map {} is listed twice", map.index))
        }
        if map.epoch != params.map_epoch(map.index) {
            return Err(format!("map {} carries the wrong epoch", map.index))
        }
        let resolved = fixture.pointer(map.last_block).is_some_and(|p| p.hash == map.last_hash);
        if !resolved {
            return Err(format!("map {} names an unknown last block", map.index))
        }
        if map.pointer_blocks.iter().any(|block| fixture.pointer(*block).is_none()) {
            return Err(format!("map {} names an unknown pointer block", map.index))
        }
        if !strictly_ascending(&map.pointer_blocks) {
            return Err(format!("map {} pointer blocks are not ascending", map.index))
        }
        if fixture.pointer(map.boundary.block) != Some(&map.boundary) {
            return Err(format!("map {} BOUNDARY disagrees with the POINTERS table", map.index))
        }
        let stream_boundary = fixture.boundaries.iter().find(|boundary| boundary.map == map.index);
        if stream_boundary.is_none_or(|boundary| {
            (boundary.block, boundary.hash) != (map.boundary.block, map.boundary.hash)
        }) {
            return Err(format!("map {} BOUNDARY disagrees with STREAM_BOUNDARIES", map.index))
        }
    }
    if fixture.completed_maps.len() != fixture.boundaries.len() {
        return Err("COMPLETED_MAPS and STREAM_BOUNDARIES counts disagree".to_owned())
    }
    for map in &fixture.partial_maps {
        if !seen.insert(map.index) {
            return Err(format!("map {} is listed twice", map.index))
        }
        if map.epoch != params.map_epoch(map.index) {
            return Err(format!("private partial {} carries the wrong epoch", map.index))
        }
        let resolved = fixture.pointer(map.last_block).is_some_and(|p| p.hash == map.last_hash);
        if !resolved {
            return Err(format!("private partial {} names an unknown last block", map.index))
        }
        let values_per_map = params.values_per_map();
        if map.pending_delimiter / values_per_map != u64::from(map.index) {
            return Err(format!(
                "private partial {} pending delimiter lies in another map",
                map.index
            ))
        }
    }
    let completed = fixture.completed_maps.iter().map(|map| map.index).collect::<Vec<_>>();
    let partial = fixture.partial_maps.iter().map(|map| map.index).collect::<Vec<_>>();
    if !strictly_ascending(&completed) || !strictly_ascending(&partial) {
        return Err("maps must be listed in ascending order".to_owned())
    }
    Ok(())
}

fn validate_query(fixture: &Fixture, query: &Query) -> ParseResult<()> {
    let params = fixture.params_name.params();
    let id = &query.id;
    if query.first_block > query.last_block {
        return Err(format!("query {id} has a reversed block range"))
    }
    let Some(first) = fixture.pointer(query.first_block) else {
        return Err(format!("query {id} starts at a block without a pointer"))
    };
    let successor = query
        .last_block
        .checked_add(1)
        .ok_or_else(|| format!("query {id} last block has no successor"))?;
    let Some(after) = fixture.pointer(successor) else {
        return Err(format!("query {id} ends at a block whose successor has no pointer"))
    };
    let Some(last_index) = after.index.checked_sub(1) else {
        return Err(format!("query {id} successor pointer cannot form an inclusive range"))
    };
    if query.index_range != (first.index, last_index) {
        return Err(format!("query {id} INDEX_RANGE disagrees with the POINTERS table"))
    }
    let map_of = |index: u64| u32::try_from(index >> params.log_values_per_map());
    let expected = (map_of(query.index_range.0), map_of(query.index_range.1));
    if expected != (Ok(query.map_range.0), Ok(query.map_range.1)) {
        return Err(format!("query {id} MAP_RANGE disagrees with its INDEX_RANGE"))
    }
    let expected_map_count = u64::from(query.map_range.1) - u64::from(query.map_range.0) + 1;
    let completed_map_count = fixture
        .completed_maps
        .iter()
        .filter(|map| (query.map_range.0..=query.map_range.1).contains(&map.index))
        .count() as u64;
    if completed_map_count != expected_map_count {
        return Err(format!("query {id} spans a map which is not completed"))
    }
    let (low, high) = query.index_range;
    if !strictly_ascending(&query.potential_indices) ||
        query.potential_indices.iter().any(|index| *index < low || *index > high)
    {
        return Err(format!("query {id} POTENTIAL_INDICES are unsorted or out of range"))
    }
    if query.slot_classes.len() != query.potential_indices.len() {
        return Err(format!("query {id} classifies a different number of slots than it lists"))
    }
    let in_blocks = |block: &u64| *block >= query.first_block && *block <= query.last_block;
    let candidates = &query.candidate_blocks;
    if !strictly_ascending(candidates) || !candidates.iter().all(in_blocks) {
        return Err(format!("query {id} CANDIDATE_BLOCKS are unsorted or out of range"))
    }
    if !strictly_ascending(&query.exact_blocks) ||
        !query.exact_blocks.iter().all(|block| query.candidate_blocks.contains(block))
    {
        return Err(format!("query {id} EXACT_BLOCKS are not a sorted subset of the candidates"))
    }
    if !strictly_ascending(&query.potential_logs) || !strictly_ascending(&query.exact_logs) {
        return Err(format!("query {id} lists logs out of order or twice"))
    }
    for identity in query.potential_logs.iter().chain(&query.exact_logs) {
        if !in_blocks(&identity.block) || fixture.log(*identity).is_none() {
            return Err(format!("query {id} names log {identity}, which does not exist in range"))
        }
    }
    let exact_log_blocks = query
        .exact_logs
        .iter()
        .map(|identity| identity.block)
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    if query.exact_blocks != exact_log_blocks {
        return Err(format!("query {id} EXACT_BLOCKS disagree with EXACT_LOGS"))
    }
    if query.is_match_all() {
        let every_block = (query.first_block..=query.last_block).collect::<Vec<_>>();
        if query.result != QueryResult::ErrMatchAll ||
            query.planner != Planner::EveryBlock ||
            !query.potential_indices.is_empty() ||
            query.candidate_blocks != every_block
        {
            return Err(format!("query {id} is unconstrained but not normalized to ErrMatchAll"))
        }
    } else if query.result != QueryResult::Ok || query.planner != Planner::Matcher {
        return Err(format!("query {id} is constrained but does not use the matcher"))
    }
    Ok(())
}

fn validate_fixture(path: &str, fixture: &Fixture) -> ParseResult<()> {
    let at = |result: ParseResult<()>| result.map_err(|message| format!("{path}: {message}"));
    at(validate_blocks(fixture))?;
    at(validate_pointers(fixture))?;
    at(validate_maps(fixture))?;
    let mut ids = HashSet::new();
    for query in &fixture.queries {
        if !ids.insert(&query.id) {
            return Err(format!("{path}: query {} is listed twice", query.id))
        }
        at(validate_query(fixture, query))?;
    }
    Ok(())
}

/// Parser-negative tests: every strict check rejects a one-line mutation of a real fixture.
#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = include_str!("fixtures/curated/focused/boundary-by-delimiter.txt");
    const PATH: &str = "boundary-by-delimiter.txt";

    fn parse(text: &str) -> ParseResult<Fixture> {
        parse_fixture(PATH, text)
    }

    fn rejects(text: &str, fragment: &str) {
        match parse(text) {
            Ok(_) => panic!("accepted a fixture that should fail with {fragment:?}"),
            Err(error) => assert!(error.contains(fragment), "{error:?} lacks {fragment:?}"),
        }
    }

    /// The unique line of `text` starting with `prefix`.
    fn line<'a>(text: &'a str, prefix: &str) -> &'a str {
        let mut found = text.lines().filter(|line| line.starts_with(prefix));
        let line = found.next().unwrap_or_else(|| panic!("no line starts with {prefix:?}"));
        assert!(found.next().is_none(), "several lines start with {prefix:?}");
        line
    }

    fn field(text: &str, prefix: &str, position: usize) -> String {
        line(text, prefix).split(' ').nth(position).expect("field exists").to_owned()
    }

    /// `text` with the unique line starting with `prefix` rewritten by `rewrite`.
    fn edit(text: &str, prefix: &str, rewrite: impl Fn(&str) -> String) -> String {
        let old = line(text, prefix);
        let mut out = String::new();
        for line in text.lines() {
            let line = if line == old { rewrite(line) } else { line.to_owned() };
            out.push_str(&line);
            out.push('\n');
        }
        out
    }

    fn replace(text: &str, prefix: &str, new: &str) -> String {
        edit(text, prefix, |_| new.to_owned())
    }

    fn swap(text: &str, prefix: &str, from: &str, to: &str) -> String {
        edit(text, prefix, |line| {
            assert_eq!(line.matches(from).count(), 1, "{from:?} must occur once in {line:?}");
            line.replace(from, to)
        })
    }

    fn remove(text: &str, prefix: &str) -> String {
        let old = line(text, prefix);
        text.lines().filter(|line| *line != old).map(|line| format!("{line}\n")).collect()
    }

    fn insert_after(text: &str, prefix: &str, new: &str) -> String {
        edit(text, prefix, |line| format!("{line}\n{new}"))
    }

    /// The lines from the one starting with `first` through the one starting with `last`.
    fn section(text: &str, first: &str, last: &str) -> String {
        let (first, last) = (line(text, first), line(text, last));
        let lines = text.lines().skip_while(|line| *line != first);
        let lines = lines.take_while(|line| *line != last);
        let mut out = lines.map(|line| format!("{line}\n")).collect::<String>();
        out.push_str(last);
        out
    }

    #[test]
    fn accepts_the_curated_fixture() {
        let fixture = parse(VALID).unwrap();
        assert_eq!(fixture.scenario, "boundary-by-delimiter");
        assert_eq!(fixture.class, FixtureClass::Focused);
        assert_eq!(fixture.params_name, ParamsName::Default);
        assert_eq!(fixture.stress_ordinal, None);
        let Origin::Checkpoint(anchor) = fixture.origin else { panic!("checkpoint origin") };
        assert_eq!((anchor.block, anchor.index), (40, 65534));
        assert_eq!(anchor.hash, canonical_block_hash(40));
        assert_eq!(fixture.termination, Termination::Head);
        assert_eq!(fixture.blocks.len(), 2);
        assert!(fixture.blocks[0].receipts[0].logs[0].topics.is_empty());
        assert!(fixture.successor.is_none());
        assert_eq!(fixture.boundaries[0].ending, BoundaryEnding::Delimiter);
        assert_eq!(fixture.pointers.len(), 2);
        assert_eq!(fixture.completed_maps[0].pointer_blocks, [41]);
        assert_eq!(fixture.completed_maps[0].rows, [Row { index: 21556, columns: vec![16776742] }]);
        assert_eq!(fixture.completed_maps[0].boundary, fixture.pointers[1]);
        assert_eq!(fixture.partial_maps[0].pending_delimiter, 65537);
        let query = &fixture.queries[0];
        assert_eq!(query.potential_logs, [LogIdentity { block: 40, receipt: 0, log: 0 }]);
        assert_eq!(query.slot_classes, [SlotClass::Address]);
        assert_eq!((query.result, query.planner), (QueryResult::Ok, Planner::Matcher));
        assert_eq!(fixture.row_count(), 2);
        assert_eq!(fixture.mark_count(), 2);
        assert_eq!(fixture.potential_count(), 1);
    }

    #[test]
    fn expands_only_canonical_empty_block_runs() {
        let text = "BLOCKS 2\nEMPTY_BLOCK_RUN 7 2\nEND_BLOCKS\n";
        let mut lines = Lines::new("inline", text).unwrap();
        let blocks = parse_blocks(&mut lines).unwrap();
        lines.done().unwrap();
        assert_eq!(blocks.iter().map(|block| block.number).collect::<Vec<_>>(), [7, 8]);
        assert_eq!(blocks[0].hash, canonical_block_hash(7));
        assert!(blocks.iter().all(|block| block.receipts.is_empty()));

        for (text, fragment) in [
            ("BLOCKS 1\nEMPTY_BLOCK_RUN 7 0\nEND_BLOCKS\n", "must be nonzero"),
            ("BLOCKS 1\nEMPTY_BLOCK_RUN 7 2\nEND_BLOCKS\n", "exceeds the expanded"),
            ("BLOCKS 1\nEMPTY_BLOCK_RUN 07 1\nEND_BLOCKS\n", "non-canonical"),
            ("BLOCKS 1\nEMPTY_BLOCK_RUN 7\nEND_BLOCKS\n", "expected BLOCK or"),
        ] {
            let mut lines = Lines::new("inline", text).unwrap();
            let error = parse_blocks(&mut lines).unwrap_err();
            assert!(error.contains(fragment), "{error:?} lacks {fragment:?}");
        }
    }

    #[test]
    fn accepts_the_other_origin_and_class_shapes() {
        let previous = canonical_block_hash(39);
        let hash = field(VALID, "BLOCK 40", 2);
        let origin = format!("ORIGIN BATCH_CONTINUATION 40 {hash} 65534 39 {previous} 65530");
        let fixture = parse(&replace(VALID, "ORIGIN", &origin)).unwrap();
        let Origin::Continuation { cursor, previous, .. } = fixture.origin else {
            panic!("continuation origin")
        };
        assert_eq!((cursor, previous.block, previous.index), (65534, 39, 65530));

        let stress = replace(VALID, "CLASS", "CLASS STRESS");
        let stress = replace(&stress, "SEED", "SEED SPLITMIX64 0xaf7c0fd8ee09de71 7");
        assert_eq!(parse(&stress).unwrap().stress_ordinal, Some(7));

        let successor = format!("SUCCESSOR 42 {} 0\nEND_SUCCESSOR", canonical_block_hash(42));
        let batch = replace(VALID, "SUCCESSOR", &successor);
        let termination = format!("TERMINATION BATCH 42 {}", canonical_block_hash(42));
        let batch = replace(&batch, "TERMINATION", &termination);
        let fixture = parse(&batch).unwrap();
        assert!(matches!(fixture.termination, Termination::Batch { next_block: 42, .. }));
        assert_eq!(fixture.successor.map(|block| block.number), Some(42));
    }

    #[test]
    fn errors_name_the_offending_line() {
        let error = parse(&replace(VALID, "FORMAT", "FORMAT 1")).unwrap_err();
        assert!(error.starts_with("boundary-by-delimiter.txt:3: "), "{error}");
        let error = parse(&replace(VALID, "ROW 13150", "ROW 13150 0")).unwrap_err();
        assert!(error.starts_with("boundary-by-delimiter.txt:38: "), "{error}");
        let error = parse(&swap(VALID, "BLOCK 41", "BLOCK 41", "BLOCK 42")).unwrap_err();
        assert!(error.starts_with("boundary-by-delimiter.txt: "), "{error}");
    }

    #[test]
    fn rejects_non_canonical_text() {
        rejects(VALID.trim_end(), "missing final newline");
        rejects(&format!("{VALID}EXTRA\n"), "unexpected trailing records");
        rejects(&VALID.replace('\n', "\r\n"), "non-canonical line");
        rejects(&insert_after(VALID, "FORMAT", ""), "non-canonical line");
        rejects(&replace(VALID, "FORMAT", "FORMAT 2 "), "non-canonical line");
        rejects(&replace(VALID, "FORMAT", " FORMAT 2"), "non-canonical line");
        rejects(&replace(VALID, "FORMAT", "FORMAT  2"), "non-canonical line");
        rejects(&replace(VALID, "FORMAT", "FORMAT\t2"), "non-canonical line");
        rejects(&replace(VALID, "SCENARIO", "SCENARIO délimiter"), "non-canonical line");
        rejects(&remove(VALID, "END_SCENARIO"), "unexpected end of fixture");
    }

    #[test]
    fn rejects_unpinned_or_malformed_headers() {
        let zeros = "0".repeat(40);
        rejects(&replace(VALID, "# Geth", &format!("# Geth {zeros}")), "expected # Geth");
        rejects(&swap(VALID, "# Generator", GENERATOR_REVISION, &zeros), "not the pinned one");
        let upper = GENERATOR_REVISION.to_uppercase();
        rejects(&swap(VALID, "# Generator", GENERATOR_REVISION, &upper), "malformed generator");
        rejects(&swap(VALID, "# Generator", GENERATOR_REVISION, "abc"), "malformed generator");
        rejects(&swap(VALID, "# Generator", "/blob/", "/tree/"), "unrecognized generator location");
        rejects(&swap(VALID, "# Generator", ".go", ".rs"), "unrecognized generator location");
        rejects(&replace(VALID, "# Generator", "# Generator"), "expected generator provenance");
        rejects(&replace(VALID, "FORMAT", "FORMAT 1"), "expected FORMAT 2");
        rejects(&replace(VALID, "SCENARIO", "SCENARIO Boundary"), "invalid scenario id");
        rejects(&replace(VALID, "SCENARIO", "SCENARIO a--b"), "invalid scenario id");
        rejects(&replace(VALID, "SCENARIO", "SCENARIO -a"), "invalid scenario id");
        rejects(&replace(VALID, "SCENARIO", "SCENARIO a b"), "SCENARIO has 3 fields, expected 2");
        rejects(&replace(VALID, "CLASS", "CLASS OTHER"), "invalid class: OTHER");
        rejects(&replace(VALID, "PARAMS", "PARAMS MAINNET"), "invalid params: MAINNET");
        rejects(&replace(VALID, "VALUE_SPACE", "VALUE_SPACE GETH_V2"), "expected VALUE_SPACE");
        let seeded = replace(VALID, "SEED", "SEED SPLITMIX64 0xaf7c0fd8ee09de71 0");
        rejects(&seeded, "curated fixture must use SEED NONE");
        let stress = replace(VALID, "CLASS", "CLASS STRESS");
        rejects(&stress, "stress fixture must use the pinned seed");
        let unpinned = replace(&stress, "SEED", "SEED SPLITMIX64 0x0000000000000000 0");
        rejects(&unpinned, "stress fixture must use the pinned seed");
        let padded = replace(&stress, "SEED", "SEED SPLITMIX64 0xaf7c0fd8ee09de71 07");
        rejects(&padded, "non-canonical stress ordinal");
    }

    #[test]
    fn rejects_inconsistent_origins_and_terminations() {
        rejects(&swap(VALID, "ORIGIN", "SYNTHETIC_CHECKPOINT", "OTHER"), "malformed ORIGIN");
        rejects(&swap(VALID, "ORIGIN", "SYNTHETIC_CHECKPOINT", "GENESIS"), "malformed ORIGIN");
        let genesis = edit(VALID, "ORIGIN", |line| {
            line.replace("SYNTHETIC_CHECKPOINT", "GENESIS")
                .replace(" default-delimiter-boundary", "")
        });
        rejects(&genesis, "GENESIS origin must start at block zero");
        rejects(
            &swap(VALID, "ORIGIN", "default-delimiter-boundary", "Bad"),
            "invalid anchor reason",
        );
        let malformed = swap(VALID, "ORIGIN", "SYNTHETIC_CHECKPOINT", "BATCH_CONTINUATION");
        rejects(&malformed, "malformed BATCH_CONTINUATION origin");
        rejects(&swap(VALID, "ORIGIN", " 40 ", " 41 "), "ORIGIN does not name the first block");
        rejects(&swap(VALID, "ORIGIN", " 65534 ", " 65533 "), "ORIGIN pointer disagrees");

        fn continuation(tail: &str) -> String {
            replace(VALID, "ORIGIN", &format!("ORIGIN BATCH_CONTINUATION {tail}"))
        }
        let hash = field(VALID, "BLOCK 40", 2);
        let previous = canonical_block_hash(39);
        let late = format!("40 {hash} 65535 39 {previous} 65530");
        rejects(&continuation(&late), "cursor is out of order");
        let early = format!("40 {hash} 65530 39 {previous} 65530");
        rejects(&continuation(&early), "cursor is out of order");
        let skipped = format!("40 {hash} 65534 38 {previous} 65530");
        rejects(&continuation(&skipped), "is not the parent");
        let forged = format!("40 {hash} 65534 39 {hash} 65530");
        rejects(&continuation(&forged), "non-canonical hash");

        rejects(&replace(VALID, "TERMINATION", "TERMINATION BATCH 42"), "malformed TERMINATION");
        rejects(&replace(VALID, "TERMINATION", "TERMINATION SOON"), "malformed TERMINATION");
        let batch = format!("TERMINATION BATCH 42 {}", canonical_block_hash(42));
        rejects(&replace(VALID, "TERMINATION", &batch), "TERMINATION and SUCCESSOR disagree");
        let successor = format!("SUCCESSOR 42 {} 0\nEND_SUCCESSOR", canonical_block_hash(42));
        rejects(&replace(VALID, "SUCCESSOR", &successor), "TERMINATION and SUCCESSOR disagree");
        let successor = format!("SUCCESSOR 43 {} 0\nEND_SUCCESSOR", canonical_block_hash(43));
        let skipped = replace(&replace(VALID, "SUCCESSOR", &successor), "TERMINATION", &batch);
        rejects(&skipped, "TERMINATION and SUCCESSOR disagree");
        rejects(&replace(VALID, "SUCCESSOR", "SUCCESSOR 42"), "SUCCESSOR has 2 fields, expected 4");
    }

    #[test]
    fn rejects_malformed_blocks_and_logs() {
        rejects(
            &replace(VALID, "BLOCKS", "BLOCKS 3"),
            "expected BLOCK or EMPTY_BLOCK_RUN, found END_BLOCKS",
        );
        rejects(&replace(VALID, "BLOCKS", "BLOCKS 1"), "expected END_BLOCKS, found BLOCK 41");
        rejects(&replace(VALID, "BLOCKS", "BLOCKS 02"), "non-canonical expanded block count: 02");
        rejects(&replace(VALID, "BLOCKS", "BLOCKS"), "BLOCKS has 1 fields, expected 2");
        rejects(&swap(VALID, "BLOCK 40", " 1", " 2"), "expected RECEIPT, found END_BLOCK");
        rejects(&swap(VALID, "BLOCK 40", " 1", " 0"), "expected END_BLOCK, found RECEIPT 1");
        rejects(&swap(VALID, "BLOCK 40", "BLOCK 40", "BLOCK 040"), "non-canonical block number");
        let hash = field(VALID, "BLOCK 40", 2);
        rejects(&swap(VALID, "BLOCK 40", &hash, "0x1234"), "malformed hash: 0x1234");
        rejects(&swap(VALID, "BLOCK 40", &hash, &hash.to_uppercase()), "malformed hash");
        rejects(&swap(VALID, "BLOCK 40", &hash, &field(VALID, "BLOCK 41", 2)), "canonical hash");
        rejects(&swap(VALID, "BLOCK 41", "BLOCK 41", "BLOCK 42"), "does not follow block 40");

        let address = field(VALID, "LOG 0x4057", 1);
        let log = |tail: &str| replace(VALID, "LOG 0x4057", &format!("LOG {address} {tail}"));
        rejects(&log("5"), "LOG declares 5 topics, more than 4");
        rejects(&log("1"), "LOG declares 1 topics but lists 0");
        rejects(&log("0 0x00"), "LOG declares 0 topics but lists 1");
        rejects(&log("1 0x1234"), "malformed hash: 0x1234");
        rejects(&log(&format!("1 {}", hash.to_uppercase())), "malformed hash");
        rejects(&replace(VALID, "LOG 0x4057", "LOG"), "malformed LOG");
        rejects(&replace(VALID, "LOG 0x4057", "NOTE"), "expected LOG or REPEAT_LOG, found NOTE");
        let upper = address.to_uppercase().replace("0X", "0x");
        rejects(&replace(VALID, "LOG 0x4057", &format!("LOG {upper} 0")), "malformed address");
        rejects(&replace(VALID, "LOG 0x4057", "LOG 0x1234 0"), "malformed address: 0x1234");
        let repeat = |count: &str| format!("REPEAT_LOG {count} {address} 0");
        rejects(&replace(VALID, "LOG 0x4057", &repeat("0")), "REPEAT_LOG count must be nonzero");
        rejects(&replace(VALID, "LOG 0x4057", &repeat("2")), "REPEAT_LOG exceeds the receipt");
        rejects(&replace(VALID, "LOG 0x4057", "REPEAT_LOG 1"), "malformed LOG");
        rejects(&remove(VALID, "LOG 0x4057"), "expected LOG or REPEAT_LOG, found END_BLOCK");
    }

    #[test]
    fn rejects_inconsistent_boundaries_and_pointers() {
        let unsorted = replace(VALID, "STREAM_BOUNDARIES", "STREAM_BOUNDARIES 2");
        let duplicate = line(VALID, "STREAM_BOUNDARY");
        rejects(&insert_after(&unsorted, "STREAM_BOUNDARY", duplicate), "strictly ascending");
        rejects(&replace(VALID, "STREAM_BOUNDARIES", "STREAM_BOUNDARIES 0"), "expected END_STREAM");
        rejects(&swap(VALID, "STREAM_BOUNDARY", "DELIMITER", "SLOT"), "invalid boundary ending");
        rejects(&swap(VALID, "STREAM_BOUNDARY", " 41 ", " 43 "), "names an unknown block");
        rejects(
            &replace(VALID, "STREAM_BOUNDARY", "STREAM_BOUNDARY 0"),
            "malformed STREAM_BOUNDARY",
        );

        rejects(&replace(VALID, "POINTERS", "POINTERS 1"), "expected END_POINTERS, found POINTER");
        let short = remove(&replace(VALID, "POINTERS", "POINTERS 1"), "POINTER 41");
        rejects(&short, "exactly one pointer per block");
        rejects(&replace(VALID, "POINTER 41", "POINTER 41"), "malformed POINTER");
        let hash = field(VALID, "POINTER 40", 2);
        rejects(
            &swap(VALID, "POINTER 41", &field(VALID, "POINTER 41", 2), &hash),
            "match its block",
        );
        rejects(
            &swap(VALID, "POINTER 41", " 65536", " 65534"),
            "indices must be strictly ascending",
        );
        rejects(&swap(VALID, "POINTER 40", " 65534", " 065534"), "non-canonical log value index");
        let huge = " 99999999999999999999";
        rejects(&swap(VALID, "POINTER 40", " 65534", huge), "log value index out of range");
        rejects(&swap(VALID, "POINTER 40", &hash, "0x1234"), "malformed hash: 0x1234");
    }

    #[test]
    fn rejects_malformed_maps() {
        rejects(&replace(VALID, "MAP 0 0", "MAP 0 0 41"), "malformed MAP");
        rejects(&swap(VALID, "MAP 0 0", "MAP 0 0", "MAP 0 1"), "map 0 carries the wrong epoch");
        rejects(&swap(VALID, "MAP 0 0", " 41 ", " 43 "), "map 0 names an unknown last block");
        let blocks = "MAP_POINTER_BLOCKS";
        rejects(&replace(VALID, blocks, "MAP_POINTER_BLOCKS 1 43"), "unknown pointer block");
        rejects(&replace(VALID, blocks, "MAP_POINTER_BLOCKS 2 41"), "declares 2 items");
        rejects(&replace(VALID, blocks, "MAP_POINTER_BLOCKS 2 41 41"), "not ascending");
        rejects(&replace(VALID, blocks, "MAP_POINTER_BLOCKS"), "missing its count");
        rejects(&replace(VALID, "ROW 21556", "ROW 65536 1 16776742"), "exceeds the map height");
        rejects(&replace(VALID, "ROW 21556", "ROW 21556 1 16777216"), "exceeds the map width");
        rejects(&replace(VALID, "ROW 21556", "ROW 21556 0"), "row 21556 lists no columns");
        rejects(&replace(VALID, "ROW 21556", "ROW 21556"), "ROW is missing its count");
        rejects(&replace(VALID, "ROW 21556", "ROW"), "malformed ROW");
        rejects(&remove(VALID, "ROW 21556"), "declared 1/1 rows/marks, found 0/0");
        rejects(
            &replace(VALID, "ROW 21556", "ROW 21556 2 1 2"),
            "declared 1/1 rows/marks, found 1/2",
        );
        rejects(&insert_after(VALID, "ROW 21556", "NOTE"), "expected ROW or the map terminator");
        let wide = swap(VALID, "MAP 0 0", " 1 1", " 2 2");
        rejects(&insert_after(&wide, "ROW 21556", "ROW 21556 1 1"), "rows must be strictly");
        rejects(&insert_after(&wide, "ROW 21556", "ROW 100 1 1"), "rows must be strictly");
        rejects(
            &swap(VALID, "BOUNDARY", " 65536", " 65535"),
            "BOUNDARY disagrees with the POINTERS",
        );
        rejects(&replace(VALID, "BOUNDARY", "BOUNDARY 41"), "malformed BOUNDARY");
        rejects(&remove(VALID, "END_MAP"), "expected END_MAP, found END_COMPLETED_MAPS");

        let partial = "PRIVATE_PARTIAL 1";
        rejects(&swap(VALID, partial, "PARTIAL 1 0", "PARTIAL 0 0"), "map 0 is listed twice");
        rejects(&swap(VALID, partial, "PARTIAL 1 0", "PARTIAL 1 1"), "wrong epoch");
        rejects(
            &swap(VALID, partial, " 41 ", " 43 "),
            "private partial 1 names an unknown last block",
        );
        rejects(&replace(VALID, partial, "PRIVATE_PARTIAL 1 0"), "has 3 fields, expected 8");
        rejects(
            &replace(VALID, "ROW 13150", "ROW 13150 1 73 74"),
            "ROW declares 1 items but lists 2",
        );
        rejects(&replace(VALID, "ROW 13150", "ROW 13150 1 073"), "non-canonical column: 073");
    }

    #[test]
    fn rejects_inconsistent_queries() {
        let query = |tail: &str| replace(VALID, "QUERY ", &format!("QUERY delimiter-block {tail}"));
        rejects(&query("41 40"), "has a reversed block range");
        rejects(&query("39 40"), "starts at a block without a pointer");
        rejects(&query("40 41"), "ends at a block whose successor has no pointer");
        rejects(&replace(VALID, "QUERY ", "QUERY Delimiter 40 40"), "invalid query id");
        let twice = insert_after(VALID, "END_QUERY", &section(VALID, "QUERY ", "END_QUERY"));
        rejects(&replace(&twice, "QUERIES", "QUERIES 2"), "query delimiter-block is listed twice");
        rejects(&replace(VALID, "INDEX_RANGE", "INDEX_RANGE 65534 65536"), "INDEX_RANGE disagrees");
        rejects(&replace(VALID, "INDEX_RANGE", "INDEX_RANGE 65534"), "malformed INDEX_RANGE");
        rejects(&replace(VALID, "MAP_RANGE", "MAP_RANGE 0 1"), "MAP_RANGE disagrees");
        rejects(&replace(VALID, "MAP_RANGE", "MAP_RANGE 0 4294967296"), "MAP_RANGE out of range");
        rejects(&replace(VALID, "RESULT", "RESULT MAYBE"), "invalid query result: MAYBE");
        rejects(&replace(VALID, "RESULT", "RESULT ERR_MATCH_ALL"), "constrained but does not use");
        rejects(&replace(VALID, "PLANNER", "PLANNER BLOOM"), "invalid planner: BLOOM");
        rejects(&replace(VALID, "PLANNER", "PLANNER EVERY_BLOCK"), "constrained but does not use");

        fn potential(tail: &str) -> String {
            replace(VALID, "POTENTIAL_INDICES", &format!("POTENTIAL_INDICES {tail}"))
        }
        rejects(&potential("1 65536"), "POTENTIAL_INDICES are unsorted or out of range");
        rejects(&potential("2 65535 65534"), "POTENTIAL_INDICES are unsorted or out of range");
        rejects(&potential("2 65534 65534"), "POTENTIAL_INDICES are unsorted or out of range");
        rejects(&potential("0"), "classifies a different number of slots");
        let classes = "POTENTIAL_SLOT_CLASSES";
        rejects(&replace(VALID, classes, "POTENTIAL_SLOT_CLASSES 1 T4"), "invalid slot class: T4");
        rejects(&replace(VALID, classes, "POTENTIAL_SLOT_CLASSES 0"), "different number of slots");
        rejects(&replace(VALID, "CANDIDATE_BLOCKS", "CANDIDATE_BLOCKS 1 41"), "out of range");
        rejects(&replace(VALID, "CANDIDATE_BLOCKS", "CANDIDATE_BLOCKS 0"), "not a sorted subset");
        rejects(&replace(VALID, "EXACT_BLOCKS", "EXACT_BLOCKS 2 40 40"), "not a sorted subset");
        let logs = "POTENTIAL_LOGS";
        rejects(&replace(VALID, logs, "POTENTIAL_LOGS 1 40:1:0"), "does not exist in range");
        rejects(&replace(VALID, logs, "POTENTIAL_LOGS 1 41:0:0"), "does not exist in range");
        rejects(&replace(VALID, logs, "POTENTIAL_LOGS 1 40:0"), "malformed log identity");
        rejects(&replace(VALID, logs, "POTENTIAL_LOGS 1 40:0:00"), "non-canonical");
        let exact = "EXACT_LOGS";
        rejects(&replace(VALID, exact, "EXACT_LOGS 2 40:0:0 40:0:0"), "out of order or twice");

        rejects(&replace(VALID, "ADDRESS_VALUES", "ADDRESS_VALUES 0"), "derived address values");
        let hash = field(VALID, "BLOCK 41", 2);
        let wrong = format!("ADDRESS_VALUES 1 {hash}");
        rejects(&replace(VALID, "ADDRESS_VALUES", &wrong), "derived address values");
        let unconstrained = replace(VALID, "ADDRESSES", "ADDRESSES 0");
        let unconstrained = replace(&unconstrained, "ADDRESS_VALUES", "ADDRESS_VALUES 0");
        rejects(&unconstrained, "unconstrained but not normalized to ErrMatchAll");
        rejects(&replace(VALID, "ADDRESSES", "ADDRESSES 2 0x00"), "declares 2 items but lists 1");

        rejects(&replace(VALID, "TOPICS", "TOPICS 5"), "TOPICS declares 5, more than 4");
        let topics = replace(VALID, "TOPICS", "TOPICS 1");
        rejects(&topics, "expected TOPIC, found ADDRESS_VALUES");
        rejects(&insert_after(&topics, "TOPICS", "TOPIC 1 ANY"), "expected TOPIC 0, found TOPIC 1");
        rejects(
            &insert_after(&topics, "TOPICS", "TOPIC 0 VALUES 0"),
            "must list at least one topic",
        );
        rejects(&insert_after(&topics, "TOPICS", "TOPIC 0 SOME"), "malformed TOPIC constraint");
        rejects(
            &insert_after(&topics, "TOPICS", "TOPIC 0 ANY extra"),
            "malformed TOPIC constraint",
        );
        rejects(&insert_after(&topics, "TOPICS", "TOPIC 0"), "malformed TOPIC");
        let any = insert_after(&topics, "TOPICS", "TOPIC 0 ANY");
        rejects(&any, "expected TOPIC_VALUES, found INDEX_RANGE");
        let values = format!("TOPIC_VALUES 0 1 {hash}");
        rejects(&insert_after(&any, "ADDRESS_VALUES", &values), "not the derived topic values");
        rejects(&insert_after(&any, "ADDRESS_VALUES", "TOPIC_VALUES 1 0"), "found TOPIC_VALUES 1");
        let constrained = insert_after(&topics, "TOPICS", &format!("TOPIC 0 VALUES 1 {hash}"));
        let derived = format!("TOPIC_VALUES 0 1 {}", topic_value(hash.parse().unwrap()));
        assert!(parse(&insert_after(&constrained, "ADDRESS_VALUES", &derived)).is_ok());
        rejects(&insert_after(&constrained, "ADDRESS_VALUES", &values), "derived topic values");
    }

    #[test]
    fn token_parsers_are_strict() {
        assert_eq!(number::<u64>("0", "n"), Ok(0));
        assert_eq!(number::<u64>("18446744073709551615", "n"), Ok(u64::MAX));
        assert!(number::<u64>("18446744073709551616", "n").unwrap_err().contains("out of range"));
        for token in ["", "00", "01", "+1", "-1", "1e3", "0x10", " 1", "1_000"] {
            assert!(number::<u64>(token, "n").unwrap_err().contains("non-canonical"), "{token:?}");
        }
        let hex = "0".repeat(64);
        assert_eq!(hash(&format!("0x{hex}")), Ok(B256::ZERO));
        assert!(hash(&hex).is_err(), "prefix is required");
        assert!(hash(&format!("0x{}", "0".repeat(63))).is_err());
        assert!(hash(&format!("0X{hex}")).is_err(), "prefix must be lowercase");
        assert!(hash(&format!("0x{}", "A".repeat(64))).is_err(), "digits must be lowercase");
        assert_eq!(address(&format!("0x{}", "0".repeat(40))), Ok(Address::ZERO));
        assert!(address(&format!("0x{hex}")).is_err());
        assert!(git_revision(GETH_REVISION, "r").is_ok());
        assert!(git_revision(&GETH_REVISION[1..], "r").is_err());
        assert!(git_revision(&GETH_REVISION.to_uppercase(), "r").is_err());
        for token in ["a", "a-b", "a1-b2", "stress-00"] {
            assert!(identifier(token, "id").is_ok(), "{token:?}");
        }
        for token in ["", "A", "a_b", "-a", "a-", "a--b", "a b", "a.b"] {
            assert!(identifier(token, "id").is_err(), "{token:?}");
        }
        assert!(log_identity("1:2:3").is_ok());
        assert!(log_identity("1:2").is_err());
        assert!(log_identity("1:2:3:4").is_err());
        assert!(log_identity("1:02:3").is_err());
    }
}
