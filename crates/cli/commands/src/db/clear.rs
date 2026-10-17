use clap::{Parser, Subcommand};
use human_bytes::human_bytes;
use reth_db::static_file::iter_static_files;
use reth_db_api::{
    database::Database,
    table::Table,
    transaction::{DbTx, DbTxMut},
    TableViewer, Tables,
};
use reth_db_common::DbTool;
use reth_fs_util::{self as fs, FsPathError};
use reth_node_builder::NodeTypesWithDB;
use reth_node_core::dirs::{ChainPath, DataDirPath};
use reth_provider::StaticFileProviderFactory;
use reth_static_file_types::StaticFileSegment;
use std::path::Path;
use tracing::info;

/// The arguments for the `reth db clear` command
#[derive(Parser, Debug)]
pub struct Command {
    #[command(subcommand)]
    subcommand: Subcommands,
}

impl Command {
    /// Execute `db clear` command
    pub fn execute<N: NodeTypesWithDB>(
        self,
        tool: &DbTool<N>,
        data_dir: &ChainPath<DataDirPath>,
    ) -> eyre::Result<()> {
        match self.subcommand {
            Subcommands::Mdbx { table } => {
                table.view(&ClearViewer { db: tool.provider_factory.db_ref() })?
            }
            Subcommands::StaticFile { segment } => {
                let static_file_provider = tool.provider_factory.static_file_provider();
                let static_files = iter_static_files(static_file_provider.directory())?;

                if let Some(segment_static_files) = static_files.get(segment) {
                    for (block_range, _) in segment_static_files {
                        static_file_provider.delete_jar(segment, block_range.start())?;
                    }
                }
            }
            Subcommands::FilterMaps => clear_filter_maps(&data_dir.filter_maps())?,
        }

        Ok(())
    }
}

#[derive(Subcommand, Debug)]
enum Subcommands {
    /// Deletes all database table entries
    Mdbx { table: Tables },
    /// Deletes all static file segment entries
    StaticFile { segment: StaticFileSegment },
    /// Deletes the FilterMaps store. A node with --filter-maps rebuilds it from the index origin.
    FilterMaps,
}

struct ClearViewer<'a, DB: Database> {
    db: &'a DB,
}

impl<DB: Database> TableViewer<()> for ClearViewer<'_, DB> {
    type Error = eyre::Report;

    fn view<T: Table>(&self) -> Result<(), Self::Error> {
        let tx = self.db.tx_mut()?;
        tx.clear::<T>()?;
        tx.commit()?;
        Ok(())
    }
}

/// Deletes everything in the `FilterMaps` store directory at `path` and keeps the directory. A
/// missing `path` is already clear.
///
/// The directory stays because it can be a symlink to another disk: `remove_dir_all` would delete
/// only the link, and the next start would create a new store on the datadir disk. Deleting the
/// files also frees their space at once, which a range delete in the store would not.
pub(super) fn clear_filter_maps(path: &Path) -> eyre::Result<()> {
    if !path.exists() {
        return Ok(())
    }
    let mut freed = 0;
    for entry in fs::read_dir(path)? {
        let entry = entry.map_err(|err| FsPathError::read_dir(err, path))?;
        let entry_path = entry.path();
        let metadata = entry.metadata().map_err(|err| FsPathError::metadata(err, &entry_path))?;
        if metadata.is_dir() {
            fs::remove_dir_all(&entry_path)?;
        } else {
            freed += metadata.len();
            fs::remove_file(&entry_path)?;
        }
    }
    info!(target: "reth::cli", ?path, freed = %human_bytes(freed as f64), "Cleared the FilterMaps store");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use reth_chainspec::Chain;
    use reth_filter_maps::{FilterMaps, FilterMapsConfig};
    use reth_node_core::args::DatadirArgs;
    use reth_provider::test_utils::create_test_provider_factory;

    fn create_store(path: &Path) {
        FilterMaps::open(path, FilterMapsConfig { history: 0 }).unwrap();
    }

    #[test]
    fn db_clear_parses_filter_maps() {
        let command = Command::try_parse_from(["clear", "filter-maps"]).unwrap();
        assert!(matches!(command.subcommand, Subcommands::FilterMaps));
    }

    #[test]
    fn db_clear_filter_maps_empties_the_store_of_the_datadir() {
        let dir = tempfile::tempdir().unwrap();
        let datadir = dir.path().to_str().unwrap().parse().unwrap();
        let data_dir = ChainPath::new(datadir, Chain::mainnet(), DatadirArgs::default());
        create_store(&data_dir.filter_maps());
        let tool = DbTool::new(create_test_provider_factory()).unwrap();

        let command = Command::try_parse_from(["clear", "filter-maps"]).unwrap();
        command.execute(&tool, &data_dir).unwrap();
        assert_eq!(std::fs::read_dir(data_dir.filter_maps()).unwrap().count(), 0);
    }

    #[test]
    fn clearing_filter_maps_empties_the_store_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("filter-maps");
        create_store(&path);

        clear_filter_maps(&path).unwrap();
        assert!(path.is_dir());
        assert_eq!(std::fs::read_dir(&path).unwrap().count(), 0);
        clear_filter_maps(&path).unwrap();

        let missing = dir.path().join("missing");
        clear_filter_maps(&missing).unwrap();
        assert!(!missing.exists());
    }

    #[cfg(unix)]
    #[test]
    fn clearing_a_symlinked_store_keeps_the_link() {
        let (other_disk, datadir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let target = other_disk.path().join("filter-maps");
        std::fs::create_dir(&target).unwrap();
        let link = datadir.path().join("filter-maps");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        create_store(&link);
        assert!(target.join("CURRENT").exists());

        clear_filter_maps(&link).unwrap();
        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read_dir(&target).unwrap().count(), 0);

        create_store(&link);
        assert!(target.join("CURRENT").exists());
    }
}
