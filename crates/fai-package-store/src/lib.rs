//! System-Git package snapshots and recoverable metadata writes, outside compiler queries.

mod git;
mod transaction;

pub use git::{GitInventory, GitStore, MaterializedPackage, TreeEntry, tree_checksum};
pub use transaction::{FileEdit, MetadataTransaction, atomic_write, lock_file};

pub(crate) fn io_error(error: impl std::fmt::Display) -> fai_package::ProviderFailure {
    fai_package::ProviderFailure {
        code: fai_package::SOURCE_UNAVAILABLE,
        message: error.to_string(),
    }
}
