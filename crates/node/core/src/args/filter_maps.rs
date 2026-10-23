//! clap [`Args`] for the `FilterMaps` local search index.

use clap::Args;

/// Parameters for the `FilterMaps` local search index.
#[derive(Debug, Clone, Copy, Args, PartialEq, Eq)]
#[command(next_help_heading = "FilterMaps")]
pub struct FilterMapsArgs {
    /// Index finalized blocks with `FilterMaps` and serve `eth_getLogs` from the index.
    ///
    /// The index needs every log of a block, so it cannot run with `--minimal` or
    /// `--prune.receiptslogfilter`.
    #[arg(
        id = "filter-maps.enabled",
        long = "filter-maps",
        default_value_t = false,
        conflicts_with_all = ["minimal", "receipts_log_filter"],
        help_heading = "FilterMaps"
    )]
    pub enabled: bool,

    /// Number of blocks up to the finalized block to keep indexed. 0 keeps every block with
    /// receipts.
    #[arg(
        long = "filter-maps.history",
        value_name = "BLOCKS",
        default_value_t = Self::DEFAULT_HISTORY,
        help_heading = "FilterMaps"
    )]
    pub history: u64,
}

impl FilterMapsArgs {
    /// About a year of mainnet blocks, as Geth's `--history.logs`.
    pub const DEFAULT_HISTORY: u64 = 2_350_000;
}

impl Default for FilterMapsArgs {
    fn default() -> Self {
        Self { enabled: false, history: Self::DEFAULT_HISTORY }
    }
}
