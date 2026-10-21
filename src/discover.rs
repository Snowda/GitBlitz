use std::fs::{self, Metadata};
use std::io::{Error as IoError, ErrorKind};
use std::path::{self, Component, Path, PathBuf};

use crate::error::{Miss, MissReason};
use crate::small_file::{read_small, SmallBuf, SMALL_FILE_MAX};

pub const HEAD_FILE: &str = "HEAD";
pub const DOT_GIT: &str = ".git";
const GITDIR_PREFIX: &str = "gitdir: ";

/// The absolute git dir for `dir` from the `.git` dir or file of its nearest ancestor, without spawning `git`:
/// git's answer when no `GIT_*` variable redirects discovery.
///
/// # Errors
/// A [`Miss`] whenever git might answer differently; [`crate::git_dir`] then asks git.
pub fn read_git_dir(dir: &Path) -> Result<PathBuf, Miss> {
	let git_dir: PathBuf = locate(dir)?.git_dir;
	require_head(&git_dir)?;
	return Ok(git_dir);
}

/// Where discovery from a directory ends.
#[derive(Debug)]
pub struct Location {
	/// The queried directory, made absolute.
	#[cfg(feature = "list")]
	pub start: PathBuf,
	/// The nearest ancestor of `start` (or `start` itself) that holds `.git`: the work tree's top.
	#[cfg(feature = "list")]
	pub top: PathBuf,
	pub git_dir: PathBuf,
}

/// [`read_git_dir`] without the check that `HEAD` is a file. Use it when the caller reads `HEAD` itself.
///
/// # Errors
/// A [`Miss`] whenever git might answer differently.
pub fn locate(dir: &Path) -> Result<Location, Miss> {
	let start: PathBuf = path::absolute(dir).map_err(|err| Miss::io(dir, &err))?;
	for level in start.ancestors() {
		let dot_git: PathBuf = level.join(DOT_GIT);
		match fs::metadata(&dot_git) {
			Ok(meta) => {
				return Ok(Location {
					#[cfg(feature = "list")]
					top: level.to_path_buf(),
					git_dir: from_dot_git(level, dot_git, &meta)?,
					#[cfg(feature = "list")]
					start,
				});
			}
			// A `.git` found in `start` proves it is a directory, so only a miss pays for that check.
			Err(err) if level == start.as_path() => {
				require_dir(&start)?;
				if err.kind() != ErrorKind::NotFound {
					return Err(Miss::io(&dot_git, &err));
				}
			}
			Err(err) if err.kind() == ErrorKind::NotFound => {}
			Err(err) => return Err(Miss::io(&dot_git, &err)),
		}
		// git tests `level` itself as a bare repo before it climbs, so a `HEAD` here is git's to judge.
		let head: PathBuf = level.join(HEAD_FILE);
		match head.try_exists() {
			Ok(false) => {}
			Ok(true) => return Err(Miss::new(&head, MissReason::BareCandidate)),
			Err(err) => return Err(Miss::io(&head, &err)),
		}
	}
	return Err(Miss::new(&start, MissReason::NotARepo));
}

/// A [`Miss`] unless `git_dir` holds a `HEAD` file, which git requires of a repository.
///
/// # Errors
/// `NoHead` when `HEAD` is absent or not a file.
pub fn require_head(git_dir: &Path) -> Result<(), Miss> {
	let head: PathBuf = git_dir.join(HEAD_FILE);
	return match fs::metadata(&head) {
		Ok(meta) if meta.is_file() => Ok(()),
		Ok(_) => Err(Miss::new(&head, MissReason::NoHead)),
		Err(err) => Err(no_head(&head, &err)),
	};
}

fn require_dir(start: &Path) -> Result<(), Miss> {
	return match fs::metadata(start) {
		Ok(meta) if meta.is_dir() => Ok(()),
		Ok(_) => Err(Miss::new(start, MissReason::NotADirectory)),
		Err(err) => Err(Miss::io(start, &err)),
	};
}

/// The [`Miss`] when `HEAD` does not open. git sees an absent `HEAD` or a directory as no HEAD.
#[must_use]
pub fn no_head(head: &Path, err: &IoError) -> Miss {
	let is_absent: bool = err.kind() == ErrorKind::NotFound || head.is_dir();
	return if is_absent { Miss::new(head, MissReason::NoHead) } else { Miss::io(head, err) };
}

fn from_dot_git(level: &Path, dot_git: PathBuf, meta: &Metadata) -> Result<PathBuf, Miss> {
	if meta.is_dir() {
		return Ok(dot_git);
	}
	let mut buf: SmallBuf = [0; SMALL_FILE_MAX];
	let text: &str = read_small(&dot_git, &mut buf).map_err(|err| Miss::io(&dot_git, &err))?;
	let target: &str = match text.strip_prefix(GITDIR_PREFIX).map(str::trim_end) {
		Some(target) if !target.is_empty() && !is_drive_relative(Path::new(target)) => target,
		_ => return Err(Miss::new(&dot_git, MissReason::BadGitFile)),
	};
	let joined: PathBuf = level.join(target);
	let lexical: PathBuf = without_dots(&joined);
	// Lexical `..` matches git's realpath unless a symlink sits on the way; only trust it if both agree.
	if lexical != joined && fs::canonicalize(&lexical).ok() != fs::canonicalize(&joined).ok() {
		return Err(Miss::new(&joined, MissReason::BadGitFile));
	}
	return Ok(lexical);
}

// `/x` on Windows has a root but no drive; the reader would have to guess the drive, so git decides.
fn is_drive_relative(path: &Path) -> bool {
	return path.has_root() && !path.is_absolute();
}

fn without_dots(path: &Path) -> PathBuf {
	return path.components().fold(PathBuf::new(), |mut out: PathBuf, part: Component<'_>| {
		match part {
			Component::CurDir => {}
			Component::ParentDir if matches!(out.components().next_back(), Some(Component::Normal(_))) => {
				out.pop();
			}
			_ => out.push(part),
		}
		return out;
	});
}

#[cfg(test)]
mod tests {
	use std::fs;
	use std::path::{Path, PathBuf};

	use super::{read_git_dir, without_dots};
	use crate::error::MissReason;
	use crate::testrepo::{expect_err, new_repo, same_dir};

	#[test]
	fn without_dots_folds_parent_and_current_components() {
		assert_eq!(without_dots(Path::new("/a/b/./../c")), PathBuf::from("/a/c"));
		assert_eq!(without_dots(Path::new("../x")), PathBuf::from("../x"));
	}

	#[test]
	fn a_file_as_dir_is_not_a_directory() -> Result<(), String> {
		let repo = new_repo()?;
		let file: PathBuf = repo.path().join("plain.txt");
		fs::write(&file, "x").map_err(|err| format!("fs::write(&file, \"x\"): {err:?}"))?;
		let got_read_git_dir = expect_err(read_git_dir(&file), "read_git_dir(&file)")?;
		assert_eq!(got_read_git_dir.reason, MissReason::NotADirectory);
		return Ok(());
	}

	#[test]
	fn missing_dir_is_a_not_found_miss() -> Result<(), String> {
		let repo = new_repo()?;
		let miss = expect_err(read_git_dir(&repo.path().join("missing")), "read_git_dir(missing)")?;
		assert!(matches!(miss.reason, MissReason::Io(_)), "{miss}");
		assert!(miss.path.ends_with("missing"));
		return Ok(());
	}

	#[test]
	fn gitfile_without_prefix_is_bad() -> Result<(), String> {
		let repo = new_repo()?;
		let work: PathBuf = repo.path().join("work");
		fs::create_dir(&work).map_err(|err| format!("fs::create_dir(&work): {err:?}"))?;
		fs::write(work.join(".git"), "nonsense\n").map_err(|err| format!("write work/.git: {err:?}"))?;
		let miss = expect_err(read_git_dir(&work), "read_git_dir(&work)")?;
		assert_eq!(miss.reason, MissReason::BadGitFile);
		assert!(miss.path.ends_with(".git"));
		return Ok(());
	}

	#[test]
	fn gitfile_to_a_dir_without_head_is_refused() -> Result<(), String> {
		let repo = new_repo()?;
		let work: PathBuf = repo.path().join("work");
		fs::create_dir_all(work.join("empty")).map_err(|err| format!("mkdir work/empty: {err:?}"))?;
		fs::write(work.join(".git"), "gitdir: empty\n").map_err(|err| format!("write work/.git: {err:?}"))?;
		let got_read_git_dir_2 = expect_err(read_git_dir(&work), "read_git_dir(&work)")?;
		assert_eq!(got_read_git_dir_2.reason, MissReason::NoHead);
		return Ok(());
	}

	#[test]
	fn dot_git_dir_without_head_is_refused() -> Result<(), String> {
		let repo = new_repo()?;
		let work: PathBuf = repo.path().join("work");
		fs::create_dir_all(work.join(".git")).map_err(|err| format!("mkdir work/.git: {err:?}"))?;
		let got_read_git_dir_3 = expect_err(read_git_dir(&work), "read_git_dir(&work)")?;
		assert_eq!(got_read_git_dir_3.reason, MissReason::NoHead);
		return Ok(());
	}

	#[test]
	fn inside_dot_git_defers_to_git() -> Result<(), String> {
		let repo = new_repo()?;
		let miss = expect_err(read_git_dir(&repo.path().join(".git").join("refs")), "read_git_dir(.git/refs)")?;
		assert_eq!(miss.reason, MissReason::BareCandidate);
		let got_parent: &Path = miss.path.parent().ok_or_else(|| String::from("miss.path.parent() is None"))?;
		assert!(same_dir(got_parent, &repo.path().join(".git"))?);
		return Ok(());
	}

	#[test]
	fn a_head_that_is_a_directory_is_no_head() -> Result<(), String> {
		let repo = new_repo()?;
		let work: PathBuf = repo.path().join("work");
		fs::create_dir_all(work.join(".git").join("HEAD")).map_err(|err| format!("mkdir work/.git/HEAD: {err:?}"))?;
		let miss = expect_err(read_git_dir(&work), "read_git_dir(&work)")?;
		assert_eq!(miss.reason, MissReason::NoHead);
		assert!(miss.path.ends_with(".git/HEAD"), "{miss}");
		return Ok(());
	}

	// A gitfile's `..` resolves lexically when no symlink is on the way, and the answer is the real git dir.
	#[test]
	fn gitfile_with_parent_steps_resolves_to_the_target_git_dir() -> Result<(), String> {
		let repo = new_repo()?;
		let work: PathBuf = repo.path().join("nest").join("work");
		fs::create_dir_all(&work).map_err(|err| format!("mkdir nest/work: {err:?}"))?;
		fs::write(work.join(".git"), "gitdir: ../../.git\n").map_err(|err| format!("write gitfile: {err:?}"))?;
		let got: PathBuf = read_git_dir(&work).map_err(|err| format!("read_git_dir(&work): {err:?}"))?;
		assert!(!got.components().any(|part| part.as_os_str() == ".."), "{got:?} keeps a `..`");
		assert!(same_dir(&got, &repo.path().join(".git"))?, "{got:?}");
		return Ok(());
	}

	#[test]
	fn without_dots_drops_a_leading_current_dir_and_keeps_a_root_parent() {
		assert_eq!(without_dots(Path::new("./x/y")), PathBuf::from("x/y"));
		assert_eq!(without_dots(Path::new("a/../../b")), PathBuf::from("../b"));
	}
}
