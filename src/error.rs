use std::fmt::{Display, Formatter, Result as FmtResult};
use std::io::{Error as IoError, ErrorKind};
use std::path::{Path, PathBuf};
use std::process::ExitStatus;

use arrayvec::ArrayString;
use thiserror::Error as ThisError;

use crate::objects::Oid;

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
	/// `dir` is more directory levels below the work tree top than the ignore check holds.
	DirDepth,
	/// A SHA-256 repository: its index entries are wider than this reader parses.
	ObjectFormat,
	MissingObject,
	Alternates,
	ReplaceRefs,
	UnsupportedPack,
	DeltaChain,
	/// A path `ls-tree` would read as a pattern, normalise, or match together with another path.
	Pathspec,
	NotATree,
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
	// Boxed so the excerpt does not widen every `Result<_, Error>`.
	#[error("printed output GitBlitz does not parse: {0}")]
	Unparsed(Box<Unparsed>),
}

/// The most bytes of a bad line or record that [`Unparsed`] keeps; a longer one ends in `…`.
pub const EXCERPT_MAX: usize = 120;
const ELLIPSIS: char = '…';

/// Git output this does not parse, with an excerpt of the line or record at fault.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unparsed {
	NotUtf8(&'static str),
	Count { lines: usize, ids: usize },
	Line(ArrayString<EXCERPT_MAX>),
	Truncated,
	Object(Oid, &'static str),
	Record(ArrayString<EXCERPT_MAX>),
}

impl Unparsed {
	#[must_use]
	pub fn line(text: &str) -> Self {
		return Self::Line(excerpt(text));
	}

	#[must_use]
	pub fn record(text: &str) -> Self {
		return Self::Record(excerpt(text));
	}
}

impl Display for Unparsed {
	fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
		return match self {
			Self::NotUtf8(what) => write!(f, "non-UTF-8 {what}"),
			Self::Count { lines, ids } => write!(f, "{lines} lines for {ids} ids"),
			Self::Line(line) => write!(f, "batch line {:?}", line.as_str()),
			Self::Truncated => f.write_str("truncated batch header"),
			Self::Object(oid, what) => write!(f, "{oid} {what}"),
			Self::Record(text) => write!(f, "ls-tree record {:?}", text.as_str()),
		};
	}
}

// `text` whole when it fits, else its longest head that leaves room for the ellipsis, cut on a char boundary.
fn excerpt(text: &str) -> ArrayString<EXCERPT_MAX> {
	if let Ok(whole) = ArrayString::from(text) {
		return whole;
	}
	let room: usize = EXCERPT_MAX - ELLIPSIS.len_utf8();
	let end: usize = (0..=room).rev().find(|&at| return text.is_char_boundary(at)).unwrap_or_default();
	let mut cut: ArrayString<EXCERPT_MAX> = ArrayString::from(text.get(..end).unwrap_or_default()).unwrap_or_default();
	cut.push(ELLIPSIS);
	return cut;
}
