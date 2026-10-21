// Edit-marker actions in a worktree's git dir; the caller names the marker and gives every input and rule.

use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::{Error as IoError, ErrorKind};
use std::path::{Component, Path, PathBuf};

use gitblitz::GitEnv;

/// A marker file name: one plain path component, so the marker always lands inside the git dir.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Flag<'a>(&'a OsStr);

impl<'a> Flag<'a> {
	/// `None` for an empty name, `.`, `..`, a separator, a slash at the end or a drive prefix.
	#[must_use]
	pub fn new(name: &'a OsStr) -> Option<Self> {
		let mut parts = Path::new(name).components();
		return match (parts.next(), parts.next()) {
			(Some(Component::Normal(part)), None) if part == name => Some(Self(name)),
			_ => None,
		};
	}
}

#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
	Run,
	Skip(&'static str),
}

/// The marker path for `dir`'s worktree, or `None` outside a git repo.
#[must_use]
pub fn marker(dir: &Path, flag: Flag<'_>) -> Option<PathBuf> {
	return gitblitz::git_dir(dir, GitEnv::NONE).ok().map(|git_dir: PathBuf| return git_dir.join(flag.0));
}

/// Touches the marker when `edited` ends in one of `suffixes` (any call, if none); returns the marker touched.
pub fn mark(dir: &Path, edited: Option<&OsStr>, flag: Flag<'_>, suffixes: &[&str]) -> Result<Option<PathBuf>, IoError> {
	let is_marked: bool = suffixes.is_empty()
		|| edited.is_some_and(|path: &OsStr| {
			return suffixes.iter().any(|suffix: &&str| return path.as_encoded_bytes().ends_with(suffix.as_bytes()));
		});
	let Some(path) = is_marked.then(|| return marker(dir, flag)).flatten() else {
		return Ok(None);
	};
	OpenOptions::new().create(true).append(true).open(&path)?;
	return Ok(Some(path));
}

/// Removes the marker; returns whether one was there.
pub fn clear(dir: &Path, flag: Flag<'_>) -> Result<bool, IoError> {
	let Some(path) = marker(dir, flag) else {
		return Ok(false);
	};
	return match fs::remove_file(&path) {
		Ok(()) => Ok(true),
		Err(err) if err.kind() == ErrorKind::NotFound => Ok(false),
		Err(err) => Err(err),
	};
}

/// Whether a gated command should run: `dir` holds every `required` file and its marker is set.
#[must_use]
pub fn gate(dir: &Path, flag: Flag<'_>, required: &[&Path]) -> Verdict {
	if !required.iter().all(|file: &&Path| return dir.join(file).is_file()) {
		return Verdict::Skip("a required file is missing from the dir");
	}
	return match marker(dir, flag) {
		Some(path) if path.is_file() => Verdict::Run,
		_ => Verdict::Skip("no edit marker"),
	};
}

#[cfg(test)]
mod tests {
	use std::ffi::OsStr;
	use std::fs;
	use std::path::{Path, PathBuf};
	use std::process::{Command, Output};

	use tempfile::TempDir;

	use super::{clear, gate, mark, Flag, Verdict};

	const NAME: &str = "test-edit-flag";
	const SUFFIXES: [&str; 2] = [".rs", ".toml"];
	const CRATE: [&str; 1] = ["Cargo.toml"];

	fn flag() -> Result<Flag<'static>, String> {
		return Flag::new(OsStr::new(NAME)).ok_or_else(|| return format!("Flag::new({NAME:?}) is None"));
	}

	fn crate_files() -> [&'static Path; 1] {
		return CRATE.map(Path::new);
	}

	fn edit(path: &str) -> Option<&OsStr> {
		return Some(OsStr::new(path));
	}

	fn git(dir: &Path, args: &[&str]) -> Result<(), String> {
		let out: Output = match Command::new("git")
			.args(["-c", "user.name=gitblitz", "-c", "user.email=gitblitz@test.invalid", "-c", "commit.gpgsign=false"])
			.args(args)
			.current_dir(dir)
			.output() { Ok(value) => value, Err(err) => return Err(format!("Command::new: {err:?}")) };
		assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
		return Ok(());
	}

	// A crate repo with one commit plus a linked worktree `wt` that also holds a Cargo.toml.
	fn crate_with_worktree() -> Result<(TempDir, PathBuf), String> {
		let tmp: TempDir = tempfile::tempdir().map_err(|err| format!("tempfile::tempdir(): {err:?}"))?;
		let main: PathBuf = tmp.path().join("main");
		fs::create_dir(&main).map_err(|err| format!("fs::create_dir(&main): {err:?}"))?;
		git(&main, &["init", "-q", "-b", "main"])?;
		fs::write(main.join("Cargo.toml"), "[package]\n").map_err(|err| format!("write Cargo.toml: {err:?}"))?;
		let wt: PathBuf = tmp.path().join("wt");
		let wt_arg: &str = wt.to_str().ok_or_else(|| String::from("wt.to_str() is None"))?;
		let steps: [&[&str]; 3] =
			[&["add", "Cargo.toml"], &["commit", "-q", "--no-verify", "-m", "one"], &["worktree", "add", "-q", "-b", "side", wt_arg]];
		steps.iter().try_for_each(|args: &&[&str]| return git(&main, args))?;
		return Ok((tmp, wt));
	}

	#[test]
	fn worktree_marker_round_trip_opens_and_closes_the_gate() -> Result<(), String> {
		let (_tmp, wt) = crate_with_worktree()?;
		let (flag, files) = (flag()?, crate_files());
		assert!(fs::write(wt.join(".git").join(NAME), "").is_err(), "the old .git/ marker path cannot exist here");
		assert_eq!(gate(&wt, flag, &files), Verdict::Skip("no edit marker"));
		let got_mark = mark(&wt, edit("src/lib.rs"), flag, &SUFFIXES).map_err(|err| format!("mark: {err:?}"))?;
		let touched: PathBuf = got_mark.ok_or_else(|| String::from("a .rs edit marks"))?;
		assert!(touched.ends_with(Path::new("worktrees").join("wt").join(NAME)), "{}", touched.display());
		assert_eq!(gate(&wt, flag, &files), Verdict::Run);
		assert!(clear(&wt, flag).map_err(|err| format!("clear: {err:?}"))?);
		assert_eq!(gate(&wt, flag, &files), Verdict::Skip("no edit marker"));
		assert!(!clear(&wt, flag).map_err(|err| format!("clear: {err:?}"))?);
		return Ok(());
	}

	#[test]
	fn only_edits_with_a_listed_suffix_mark() -> Result<(), String> {
		let (_tmp, wt) = crate_with_worktree()?;
		let (flag, files) = (flag()?, crate_files());
		let mark_one = |edited: Option<&OsStr>| return mark(&wt, edited, flag, &SUFFIXES).map_err(|err| format!("mark: {err:?}"));
		assert_eq!(mark_one(edit("README.md"))?, None);
		assert_eq!(mark_one(None)?, None);
		assert_eq!(gate(&wt, flag, &files), Verdict::Skip("no edit marker"));
		assert!(mark_one(edit("C:/x/Cargo.toml"))?.is_some());
		assert_eq!(gate(&wt, flag, &files), Verdict::Run);
		return Ok(());
	}

	#[test]
	fn no_suffixes_means_every_call_marks() -> Result<(), String> {
		let (_tmp, wt) = crate_with_worktree()?;
		let got: Option<PathBuf> = mark(&wt, None, flag()?, &[]).map_err(|err| format!("mark: {err:?}"))?;
		assert!(got.is_some_and(|path: PathBuf| return path.is_file()));
		return Ok(());
	}

	// The caller's name is the whole contract: two names are two independent markers.
	#[test]
	fn each_flag_name_is_its_own_marker() -> Result<(), String> {
		let (_tmp, wt) = crate_with_worktree()?;
		let other: Flag<'_> = Flag::new(OsStr::new("other-flag")).ok_or_else(|| String::from("other-flag is valid"))?;
		let (flag, files) = (flag()?, crate_files());
		let touched: Option<PathBuf> = mark(&wt, edit("a.rs"), flag, &SUFFIXES).map_err(|err| format!("mark: {err:?}"))?;
		assert!(touched.is_some_and(|path: PathBuf| return path.file_name() == Some(OsStr::new(NAME))));
		assert_eq!(gate(&wt, flag, &files), Verdict::Run);
		assert_eq!(gate(&wt, other, &files), Verdict::Skip("no edit marker"));
		assert!(!clear(&wt, other).map_err(|err| format!("clear: {err:?}"))?);
		assert_eq!(gate(&wt, flag, &files), Verdict::Run, "clearing another flag leaves this one");
		return Ok(());
	}

	#[test]
	fn gate_skips_dirs_missing_a_required_file() -> Result<(), String> {
		let (_tmp, wt) = crate_with_worktree()?;
		let (flag, files) = (flag()?, crate_files());
		mark(&wt, edit("a.rs"), flag, &SUFFIXES).map_err(|err| format!("mark: {err:?}"))?;
		let sub: PathBuf = wt.join("docs");
		fs::create_dir(&sub).map_err(|err| format!("fs::create_dir(&sub): {err:?}"))?;
		assert_eq!(gate(&sub, flag, &files), Verdict::Skip("a required file is missing from the dir"));
		assert_eq!(gate(&sub, flag, &[]), Verdict::Run, "nothing required: the marker alone decides");
		let both: [&Path; 2] = [Path::new("Cargo.toml"), Path::new("absent.lock")];
		assert_eq!(gate(&wt, flag, &both), Verdict::Skip("a required file is missing from the dir"));
		return Ok(());
	}

	#[test]
	fn flag_names_stay_one_plain_component() {
		let good: [&str; 3] = ["rs-edited", "a.b", "x"];
		let bad: [&str; 7] = ["", ".", "..", "a/b", "a/", "./a", "/a"];
		good.iter().for_each(|name: &&str| assert!(Flag::new(OsStr::new(name)).is_some(), "{name:?}"));
		bad.iter().for_each(|name: &&str| assert_eq!(Flag::new(OsStr::new(name)), None, "{name:?}"));
		#[cfg(windows)]
		["a\\b", "C:a", "C:\\a"].iter().for_each(|name: &&str| assert_eq!(Flag::new(OsStr::new(name)), None, "{name:?}"));
	}

	#[test]
	fn outside_a_repo_nothing_is_marked_or_cleared() -> Result<(), String> {
		let tmp: TempDir = tempfile::tempdir().map_err(|err| format!("tempfile::tempdir(): {err:?}"))?;
		let flag: Flag<'_> = flag()?;
		assert_eq!(mark(tmp.path(), edit("a.rs"), flag, &SUFFIXES).map_err(|err| format!("mark: {err:?}"))?, None);
		assert!(!clear(tmp.path(), flag).map_err(|err| format!("clear: {err:?}"))?);
		return Ok(());
	}
}
