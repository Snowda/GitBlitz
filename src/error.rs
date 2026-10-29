use std::io::{Error as IoError, ErrorKind};
use std::path::{Path, PathBuf};
use std::process::ExitStatus;

use thiserror::Error as ThisError;

/// Why the file path declined to answer; each one sends [`crate::git_dir`]/[`crate::head`] to `git`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MissReason {
	/// The caller's [`crate::GitEnv`] sets a `GIT_*` variable that redirects git, named in [`Miss::path`].
	GitEnv,
	NotADirectory,
	NotARepo,
	BareCandidate,
	BadGitFile,
	NoHead,
	Reftable,
	UnsupportedRef,
	SymbolicRef,
	Unborn,
	Malformed,
	/// An index this reader does not settle: an unknown version, split or sparse, or unmerged.
	UnsupportedIndex,
	/// `index.lock` exists: a writer is replacing the index right now.
	IndexLocked,
	/// A repository nested in the work tree, which `ls-files` lists as one directory entry.
	NestedRepo,
	/// A SHA-256 repository: its index entries are wider than this reader parses.
	ObjectFormat,
	Io(ErrorKind),
}

#[derive(Debug, Clone, PartialEq, Eq, ThisError)]
#[error("{reason:?} at {}", .path.display())]
pub struct Miss {
	pub path: PathBuf,
	pub reason: MissReason,
}

impl Miss {
	#[must_use]
	pub fn new(path: &Path, reason: MissReason) -> Self {
		return Self { path: path.to_path_buf(), reason };
	}

	#[must_use]
	pub fn io(path: &Path, err: &IoError) -> Self {
		return Self::new(path, MissReason::Io(err.kind()));
	}
}

/// Both the file reader and the `git` fallback failed; [`Cause`] says how git failed.
#[derive(Debug, ThisError)]
#[error("`git {}` in {} {cause} (file reader missed: {miss})", .args.join(" "), .dir.display())]
pub struct Error {
	pub dir: PathBuf,
	pub args: &'static [&'static str],
	pub miss: Miss,
	#[source]
	pub cause: Cause,
}

#[derive(Debug, ThisError)]
pub enum Cause {
	#[error("could not start: {0}")]
	Spawn(IoError),
	#[error("exited {status}: {stderr}")]
	Exit { status: ExitStatus, stderr: String },
	#[error("printed non-UTF-8 output")]
	NotUtf8,
	#[error("printed {len} bytes, more than the {max} hex digits of an object id", len = .0, max = crate::OID_HEX_MAX)]
	NotAnOid(usize),
}
