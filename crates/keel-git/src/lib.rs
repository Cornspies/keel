//! Git access for keel.
//!
//! Reading (trees, blobs, history, diffs) goes through libgit2. Everything on
//! the wire (clone, fetch, push) is delegated to the real `git` binary, which
//! is battle-tested and implements every protocol detail and optimisation
//! (bitmaps, commit-graph, partial clone, protocol v2).

mod diff;
mod edit;
mod files;
mod merge;
mod mirror;
mod push;
mod read;
mod store;
mod transport;

pub use diff::{CommitDiff, DiffLine, DiffLineKind, DiffStatus, FileDiff, Hunk};
pub use read::{
    Blob, CommitDetail, CommitSummary, EntryKind, GitRepo, RefInfo, Resolved, Signature,
    TreeEntry,
};
pub use edit::replace_line;
pub use files::{FileChanges, FileRef};
pub use merge::{Identity, MergeStyle, Mergeability};
pub use mirror::{Remote, ls_remote, mirror_fetch, mirror_push, parse_lfs_pointer};
pub use push::{RefChange, Refs, ref_changes};
pub use store::{backup, create_bare, fork, run_maintenance};
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
