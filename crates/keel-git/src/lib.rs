//! Git access for keel.
//!
//! Reading (trees, blobs, history, diffs) goes through libgit2. Everything on
//! the wire (clone, fetch, push) is delegated to the real `git` binary, which
//! is battle-tested and implements every protocol detail and optimisation
//! (bitmaps, commit-graph, partial clone, protocol v2).

mod diff;
mod files;
mod merge;
mod push;
mod read;
mod store;
mod transport;

pub use diff::{CommitDiff, DiffLine, DiffLineKind, DiffStatus, FileDiff, Hunk};
pub use read::{
    Blob, CommitDetail, CommitSummary, EntryKind, GitRepo, RefInfo, Resolved, Signature,
    TreeEntry,
};
pub use files::{FileChanges, FileRef};
pub use merge::{Identity, MergeStyle, Mergeability};
pub use push::{RefChange, Refs, ref_changes};
pub use store::{create_bare, fork, run_maintenance};
pub use transport::{Service, git_command};

pub use git2::Oid;

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
