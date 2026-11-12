//! Git access for keel.
//!
//! Reading (trees, blobs, history, diffs) goes through libgit2. Everything on
//! the wire (clone, fetch, push) is delegated to the real `git` binary, which
//! is battle-tested and implements every protocol detail and optimisation
//! (bitmaps, commit-graph, partial clone, protocol v2).

mod archive;
mod blame;
mod diff;
mod edit;
mod files;
mod merge;
mod mirror;
mod push;
mod read;
mod signed;
mod stats;
mod store;
mod transport;

pub use archive::{ArchiveFormat, archive_command};
pub use diff::{CommitDiff, DiffLine, DiffLineKind, DiffStatus, FileDiff, Hunk};
pub use read::{
    Blob, BlobInfo, CommitDetail, CommitSummary, EntryKind, GitRepo, RefInfo, RefNames, Resolved,
    Signature, TreeEntry,
};
pub use edit::{FileChange, replace_line};
pub use files::{FileChanges, FileRef, cat_blob_command};
pub use merge::{Identity, MergeStyle, Mergeability, Signer};
pub use blame::{Blame, BlameCommit, BlameHunk, IGNORE_REVS_FILE};
pub use mirror::{Remote, import_fetch, ls_remote, mirror_fetch, mirror_push, parse_lfs_pointer};
pub use push::{RefChange, Refs, ref_changes};
pub use signed::SignedCommit;
pub use stats::CommitStamp;
pub use store::{backup, create_bare, fork, pack_refs_if_due, repack_if_due, run_maintenance};
pub use transport::{Service, git_command};

pub use git2::Oid;

/// `git`, with the environment every command keel runs uses:
/// - replace refs (`refs/replace/*`) are ignored, as libgit2 ignores them,
///   so merges, archives and ancestry checks see the same commits as diffs,
///   file views and signature checks;
/// - no system or user configuration applies, only the repository's.
pub fn command() -> std::process::Command {
    let mut cmd = std::process::Command::new("git");
    standard_env(&mut cmd);
    cmd
}

/// Sets [`command`]'s environment on `cmd`, e.g. after `env_clear`.
pub(crate) fn standard_env(cmd: &mut std::process::Command) -> &mut std::process::Command {
    cmd.env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
}

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
    /// Why commits can't be rebased (conflicts, merge commits); fine to
    /// show.
    #[error("{0}")]
    Rebase(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
