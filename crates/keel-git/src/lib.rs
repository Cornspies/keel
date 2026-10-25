//! Git access for keel.
//!
//! Reading (trees, blobs, history, diffs) goes through libgit2. Everything on
//! the wire (clone, fetch, push) is delegated to the real `git` binary, which
//! is battle-tested and implements every protocol detail and optimisation
//! (bitmaps, commit-graph, partial clone, protocol v2).

mod archive;
mod diff;
mod edit;
mod files;
mod merge;
mod mirror;
mod push;
mod read;
mod stats;
mod store;
mod transport;

pub use archive::{ArchiveFormat, archive_command};
pub use diff::{CommitDiff, DiffLine, DiffLineKind, DiffStatus, FileDiff, Hunk};
pub use read::{
    Blob, CommitDetail, CommitSummary, EntryKind, GitRepo, RefInfo, Resolved, Signature,
    TreeEntry,
};
pub use edit::{FileChange, replace_line};
pub use files::{FileChanges, FileRef};
pub use merge::{Identity, MergeStyle, Mergeability};
pub use mirror::{Remote, import_fetch, ls_remote, mirror_fetch, mirror_push, parse_lfs_pointer};
pub use push::{RefChange, Refs, ref_changes};
pub use stats::CommitStamp;
pub use store::{backup, create_bare, fork, run_maintenance};
pub use transport::{Service, git_command};

pub use git2::Oid;

/// Whether `name` can be a branch name.
pub fn valid_branch_name(name: &str) -> bool {
    !name.is_empty() && !name.starts_with('-') && name.len() <= 200 && valid_ref_name(&format!("refs/heads/{name}"))
}

/// Whether `name` is a valid full ref name, like `refs/tags/v1.0`.
pub fn valid_ref_name(name: &str) -> bool {
    git2::Reference::is_valid_name(name)
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Git(#[from] git2::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Other(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
