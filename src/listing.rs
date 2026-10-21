//! `git ls-files -z --cached --others --exclude-standard`, answered from `.git/index` plus a walk.
//!
//! `--cached` is every path the index tracks, even a tracked file that a `.gitignore` also matches
//! (a walk alone would drop it). `--others --exclude-standard` is every other file on disk that the
//! `.gitignore` files, `info/exclude` and the global excludes file let through, walked with the
//! `ignore` crate. Paths come back relative to `dir`, as git prints them when run in `dir`.

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use ignore::{DirEntry, Match, WalkBuilder};

use std::collections::HashSet;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use crate::discover::{locate, require_head, Location, DOT_GIT};
use crate::error::{Miss, MissReason};
use crate::index;
use crate::refs::{common_dir, CONFIG_FILE};

const INDEX_FILE: &str = "index";
const INDEX_LOCK_FILE: &str = "index.lock";
const INFO_EXCLUDE: &str = "info/exclude";
const GITIGNORE_FILE: &str = ".gitignore";
const OBJECT_FORMAT_KEY: &str = "objectformat";
const SHA256: &str = "sha256";
const IGNORE_CASE_KEY: &str = "ignorecase";
const TRUE: &str = "true";

/// The files `git ls-files -z --cached --others --exclude-standard` run in `dir` lists, sorted.
///
/// # Errors
/// A [`Miss`] for anything this reader does not settle: see [`MissReason`].
pub fn read_files(dir: &Path) -> Result<Vec<PathBuf>, Miss> {
	let Location { start, top, git_dir } = locate(dir)?;
	require_head(&git_dir)?;
	let top: &Path = &top;
	let common: PathBuf = common_dir(&git_dir)?;
	let config: String = read_config(&common)?;
	if config_says(&config, OBJECT_FORMAT_KEY, SHA256) {
		return Err(Miss::new(&git_dir, MissReason::ObjectFormat));
	}
	let is_case_folded: bool = config_says(&config, IGNORE_CASE_KEY, TRUE);
	let prefix: String = slashed(start.strip_prefix(top).map_err(|_| Miss::new(&start, MissReason::NotARepo))?);
	let prefix: String = if prefix.is_empty() { prefix } else { format!("{prefix}/") };

	let tracked: Vec<String> = tracked_under(&git_dir, &prefix)?;
	let fold = |path: &str| if is_case_folded { path.to_lowercase() } else { path.to_owned() };
	let known: HashSet<String> = tracked.iter().map(|p| fold(p)).collect();
	let mut files: Vec<String> = tracked;
	// git never descends into an ignored directory, so under one only the tracked files are listed.
	let exclude: PathBuf = common.join(INFO_EXCLUDE);
	if !is_under_ignored_dir(top, &start, &exclude, is_case_folded) {
		for path in walk(&start, top, is_case_folded)? {
			if !known.contains(&fold(&path)) {
				files.push(path);
			}
		}
	}
	let mut files: Vec<PathBuf> = files.into_iter().map(PathBuf::from).collect();
	files.sort();
	files.dedup();
	return Ok(files);
}

// Whether git's exclude rules ignore `start` or a directory between `top` and it. The walk applies
// the rules only below its root, never to the root itself, so this checks those levels.
fn is_under_ignored_dir(top: &Path, start: &Path, exclude: &Path, is_case_folded: bool) -> bool {
	let Ok(below) = start.strip_prefix(top) else {
		return false;
	};
	if below.as_os_str().is_empty() {
		return false;
	}
	let shared: [Gitignore; 2] = [matcher(top, exclude, is_case_folded), Gitignore::global().0];
	// The `.gitignore` of each level from `top` down to the parent of the next level.
	let mut levels: Vec<Gitignore> = Vec::new();
	let mut level: PathBuf = top.to_path_buf();
	for component in below.components() {
		levels.push(matcher(&level, &level.join(GITIGNORE_FILE), is_case_folded));
		level.push(component);
		if is_ignored_dir(&level, &levels, &shared) {
			return true;
		}
	}
	return false;
}

// git's precedence for one directory: the nearest `.gitignore` above it first, then those further up,
// then `info/exclude`, then the global excludes file. The first rule that matches decides.
fn is_ignored_dir(dir: &Path, levels: &[Gitignore], shared: &[Gitignore]) -> bool {
	for gitignore in levels.iter().rev().chain(shared) {
		match gitignore.matched(dir, true) {
			Match::Ignore(_) => return true,
			Match::Whitelist(_) => return false,
			Match::None => {}
		}
	}
	return false;
}

// Patterns in `file` anchored at `root`; a file that is absent or unreadable matches nothing, as in git.
fn matcher(root: &Path, file: &Path, is_case_folded: bool) -> Gitignore {
	let mut builder: GitignoreBuilder = GitignoreBuilder::new(root);
	builder.case_insensitive(is_case_folded).ok();
	let _ = builder.add(file);
	return builder.build().unwrap_or_else(|_| Gitignore::empty());
}

// The index's paths under `prefix`, made relative to it. No index yet (a fresh repo) tracks nothing.
fn tracked_under(git_dir: &Path, prefix: &str) -> Result<Vec<String>, Miss> {
	if git_dir.join(INDEX_LOCK_FILE).exists() {
		return Err(Miss::new(&git_dir.join(INDEX_LOCK_FILE), MissReason::IndexLocked));
	}
	let path: PathBuf = git_dir.join(INDEX_FILE);
	let bytes: Vec<u8> = match fs::read(&path) {
		Ok(bytes) => bytes,
		Err(err) if err.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
		Err(err) => return Err(Miss::io(&path, &err)),
	};
	let paths: Vec<String> = index::tracked_paths(&bytes, &path)?;
	return Ok(paths.into_iter().filter_map(|p| p.strip_prefix(prefix).map(str::to_owned)).collect());
}

// Every file under `start` that git's exclude rules let through, relative to `start`. A nested repo
// (or a submodule) is listed by `ls-files` as one entry, which a file walk cannot reproduce: a Miss.
fn walk(start: &Path, top: &Path, is_case_folded: bool) -> Result<Vec<String>, Miss> {
	let walker = WalkBuilder::new(start)
		.hidden(false)
		.ignore(false)
		.parents(true)
		.git_ignore(true)
		.git_exclude(true)
		.git_global(true)
		.require_git(true)
		.ignore_case_insensitive(is_case_folded)
		.filter_entry(|entry| entry.file_name() != DOT_GIT)
		.build();
	let mut out: Vec<String> = Vec::new();
	for entry in walker {
		let entry: DirEntry = entry.map_err(|err| Miss::new(start, MissReason::Io(err.io_error().map_or(ErrorKind::Other, |e| e.kind()))))?;
		let is_dir: bool = entry.file_type().is_some_and(|t| t.is_dir());
		if is_dir {
			// A stat, not the walk's own `.git` entry: the nested repo's ignore rules can hide that entry.
			if entry.path() != top && entry.path().join(DOT_GIT).exists() {
				return Err(Miss::new(entry.path(), MissReason::NestedRepo));
			}
			continue;
		}
		let relative: &Path = entry.path().strip_prefix(start).map_err(|_| Miss::new(entry.path(), MissReason::Malformed))?;
		out.push(slashed(relative));
	}
	return Ok(out);
}

fn read_config(common: &Path) -> Result<String, Miss> {
	let path: PathBuf = common.join(CONFIG_FILE);
	return match fs::read_to_string(&path) {
		Ok(text) => Ok(text),
		Err(err) if err.kind() == ErrorKind::NotFound => Ok(String::new()),
		Err(err) => Err(Miss::io(&path, &err)),
	};
}

// Whether a `key = value` line sets `key` to `value`; git config keys and booleans are case-insensitive.
// This skips the section: `objectformat` (extensions) and `ignorecase` (core) each occur in one section only.
fn config_says(config: &str, key: &str, value: &str) -> bool {
	return config.lines().filter_map(|line| line.split_once('=')).any(|(k, v)| {
		return k.trim().eq_ignore_ascii_case(key) && v.trim().eq_ignore_ascii_case(value);
	});
}

fn slashed(path: &Path) -> String {
	return path.to_string_lossy().replace('\\', "/");
}

#[cfg(test)]
mod tests {
	use std::fs;
	use std::io::ErrorKind;
	use std::path::{Path, PathBuf};

	use super::read_files;
	use crate::env::GitEnv;
	use crate::error::MissReason;
	use crate::list_files;
	use crate::testrepo::{commit, git, new_repo};

	const LS_FILES: [&str; 5] = ["ls-files", "-z", "--cached", "--others", "--exclude-standard"];

	// The oracle: what git itself lists from `dir`.
	fn ls_files(dir: &Path) -> Result<Vec<PathBuf>, String> {
		let listed: String = git(dir, &LS_FILES)?;
		let mut files: Vec<PathBuf> = listed.split('\0').filter(|p| !p.is_empty()).map(PathBuf::from).collect();
		files.sort();
		return Ok(files);
	}

	fn put(root: &Path, rel: &str, text: &str) -> Result<(), String> {
		let path: PathBuf = root.join(rel);
		if let Some(parent) = path.parent() {
			fs::create_dir_all(parent).map_err(|e| format!("mkdir {rel}: {e}"))?;
		}
		return fs::write(&path, text).map_err(|e| format!("write {rel}: {e}"));
	}

	fn miss_reason(dir: &Path) -> Option<MissReason> {
		return read_files(dir).err().map(|m| m.reason);
	}

	// Tracked, tracked-and-ignored, untracked, ignored, nested and deleted files in one work tree.
	fn work_tree() -> Result<tempfile::TempDir, String> {
		let repo = new_repo()?;
		let root: &Path = repo.path();
		put(root, ".gitignore", "*.log\nbuild/\n")?;
		put(root, "src/main.rs", "fn main() {}\n")?;
		put(root, "sub/deep/b.rs", "pub fn b() {}\n")?;
		put(root, "forced.log", "tracked although ignored\n")?;
		put(root, "gone.rs", "deleted after commit\n")?;
		git(root, &["add", ".gitignore", "src/main.rs", "sub/deep/b.rs", "gone.rs"])?;
		git(root, &["add", "-f", "forced.log"])?;
		commit(root, "fixture")?;
		fs::remove_file(root.join("gone.rs")).map_err(|e| format!("rm: {e}"))?;
		put(root, "new.rs", "untracked\n")?;
		put(root, "sub/new.txt", "untracked too\n")?;
		put(root, "noise.log", "ignored\n")?;
		put(root, "build/out.o", "ignored dir\n")?;
		return Ok(repo);
	}

	#[test]
	fn a_work_tree_lists_exactly_what_ls_files_lists() -> Result<(), String> {
		let repo = work_tree()?;
		let root: &Path = repo.path();
		let got: Vec<PathBuf> = read_files(root).map_err(|m| format!("read_files: {m}"))?;
		assert_eq!(got, ls_files(root)?);
		// The index is what keeps the tracked-but-ignored file and the deleted one: a walk sees neither.
		assert!(got.contains(&PathBuf::from("forced.log")) && got.contains(&PathBuf::from("gone.rs")), "{got:?}");
		assert!(!got.contains(&PathBuf::from("noise.log")) && !got.contains(&PathBuf::from("build/out.o")), "{got:?}");
		// From a subdirectory, paths are relative to it and cover only its subtree.
		let sub: PathBuf = root.join("sub");
		assert_eq!(read_files(&sub).map_err(|m| format!("read_files(sub): {m}"))?, ls_files(&sub)?);
		assert_eq!(list_files(root, GitEnv::NONE).map_err(|e| format!("list_files: {e}"))?, got, "the reader answers");
		return Ok(());
	}

	fn is_io_miss_at(dir: &Path, at: &Path) -> bool {
		return read_files(dir).err().is_some_and(|m| {
			return m.path == at && matches!(m.reason, MissReason::Io(kind) if kind != ErrorKind::NotFound);
		});
	}

	// A SHA-256 index has wider entries than the reader parses, so it misses and `list_files` asks git.
	#[test]
	fn a_sha256_repo_misses_and_list_files_asks_git() -> Result<(), String> {
		let repo = tempfile::tempdir().map_err(|e| format!("tempdir: {e}"))?;
		git(repo.path(), &["init", "-q", "-b", "main", "--object-format=sha256"])?;
		put(repo.path(), "a.rs", "x\n")?;
		git(repo.path(), &["add", "a.rs"])?;
		assert_eq!(miss_reason(repo.path()), Some(MissReason::ObjectFormat));
		assert_eq!(list_files(repo.path(), GitEnv::NONE).map_err(|e| format!("list_files: {e}"))?, ls_files(repo.path())?);
		assert_eq!(ls_files(repo.path())?, vec![PathBuf::from("a.rs")]);
		return Ok(());
	}

	// No config means no case folding and SHA-1, as git assumes too.
	#[test]
	fn a_missing_config_reads_as_empty() -> Result<(), String> {
		let repo = work_tree()?;
		fs::remove_file(repo.path().join(".git/config")).map_err(|e| format!("rm config: {e}"))?;
		assert_eq!(read_files(repo.path()).map_err(|m| format!("read_files: {m}"))?, ls_files(repo.path())?);
		return Ok(());
	}

	// An index or config that does not read as a file is an I/O miss, never an empty listing.
	#[test]
	fn an_unreadable_index_or_config_is_an_io_miss() -> Result<(), String> {
		let repo = new_repo()?;
		put(repo.path(), "a.rs", "x\n")?;
		let index: PathBuf = repo.path().join(".git/index");
		fs::create_dir(&index).map_err(|e| format!("mkdir index: {e}"))?;
		assert!(is_io_miss_at(repo.path(), &index), "{:?}", read_files(repo.path()));
		fs::remove_dir(&index).map_err(|e| format!("rmdir index: {e}"))?;
		let config: PathBuf = repo.path().join(".git/config");
		fs::remove_file(&config).map_err(|e| format!("rm config: {e}"))?;
		fs::create_dir(&config).map_err(|e| format!("mkdir config: {e}"))?;
		assert!(is_io_miss_at(repo.path(), &config), "{:?}", read_files(repo.path()));
		return Ok(());
	}

	#[test]
	fn from_inside_an_ignored_directory_only_its_tracked_files_are_listed() -> Result<(), String> {
		let repo = new_repo()?;
		let root: &Path = repo.path();
		put(root, ".gitignore", "comparison/\n")?;
		put(root, "comparison/kept.rs", "force-added\n")?;
		put(root, "comparison/Cargo.toml", "[package]\n")?;
		put(root, "comparison/src/lib.rs", "untracked and ignored\n")?;
		git(root, &["add", ".gitignore"])?;
		git(root, &["add", "-f", "comparison/kept.rs"])?;
		commit(root, "fixture")?;
		let sub: PathBuf = root.join("comparison");
		let got: Vec<PathBuf> = read_files(&sub).map_err(|m| format!("read_files: {m}"))?;
		assert_eq!(got, ls_files(&sub)?);
		assert_eq!(got, vec![PathBuf::from("kept.rs")], "the walk adds nothing under an ignored directory");
		return Ok(());
	}

	// Two levels down, the nearer `.gitignore` overrides the top one: git lists `a/deep` but not `c/deep`.
	#[test]
	fn the_nearest_gitignore_decides_for_a_directory_two_levels_down() -> Result<(), String> {
		let repo = new_repo()?;
		let root: &Path = repo.path();
		put(root, ".gitignore", "deep/\n")?;
		put(root, "a/.gitignore", "!deep/\n")?;
		put(root, "a/deep/x/y.rs", "whitelisted by a/.gitignore\n")?;
		put(root, "c/deep/x/y.rs", "ignored by the top .gitignore\n")?;
		git(root, &["add", ".gitignore", "a/.gitignore"])?;
		commit(root, "fixture")?;
		let kept: PathBuf = root.join("a/deep/x");
		assert_eq!(read_files(&kept).map_err(|m| format!("read_files(a): {m}"))?, ls_files(&kept)?);
		assert_eq!(ls_files(&kept)?, vec![PathBuf::from("y.rs")], "git lists the whitelisted file");
		let dropped: PathBuf = root.join("c/deep/x");
		assert_eq!(read_files(&dropped).map_err(|m| format!("read_files(c): {m}"))?, ls_files(&dropped)?);
		assert_eq!(ls_files(&dropped)?, Vec::<PathBuf>::new(), "git lists nothing under the ignored directory");
		return Ok(());
	}

	#[test]
	fn a_path_compressed_v4_index_reads_the_same() -> Result<(), String> {
		let repo = work_tree()?;
		git(repo.path(), &["update-index", "--index-version", "4"])?;
		let header: Vec<u8> = fs::read(repo.path().join(".git/index")).map_err(|e| format!("read index: {e}"))?;
		assert_eq!(header.get(4..8), Some([0u8, 0, 0, 4].as_slice()), "the fixture really is a v4 index");
		assert_eq!(read_files(repo.path()).map_err(|m| format!("read_files: {m}"))?, ls_files(repo.path())?);
		return Ok(());
	}

	#[test]
	fn layouts_the_reader_does_not_settle_miss_and_list_files_asks_git() -> Result<(), String> {
		let split = work_tree()?;
		git(split.path(), &["update-index", "--split-index"])?;
		assert_eq!(miss_reason(split.path()), Some(MissReason::UnsupportedIndex));
		assert_eq!(list_files(split.path(), GitEnv::NONE).map_err(|e| format!("list_files: {e}"))?, ls_files(split.path())?);

		let nested = work_tree()?;
		git(&nested.path().join("sub"), &["init", "-q"])?;
		assert_eq!(miss_reason(nested.path()), Some(MissReason::NestedRepo));
		assert_eq!(list_files(nested.path(), GitEnv::NONE).map_err(|e| format!("list_files: {e}"))?, ls_files(nested.path())?);

		let locked = work_tree()?;
		put(locked.path(), ".git/index.lock", "")?;
		assert_eq!(miss_reason(locked.path()), Some(MissReason::IndexLocked));
		return Ok(());
	}

	// A `.*` rule hides the nested `.git` from the walk, yet git still lists `sub/` as one nested repo.
	#[test]
	fn a_nested_repo_whose_dot_git_is_ignored_still_misses() -> Result<(), String> {
		let repo = new_repo()?;
		let root: &Path = repo.path();
		put(root, ".gitignore", ".*\n!.gitignore\n")?;
		put(root, "a.rs", "x\n")?;
		git(root, &["add", ".gitignore", "a.rs"])?;
		commit(root, "fixture")?;
		put(root, "sub/x.rs", "in the nested repo\n")?;
		git(&root.join("sub"), &["init", "-q"])?;
		assert_eq!(miss_reason(root), Some(MissReason::NestedRepo));
		let listed: Vec<PathBuf> = list_files(root, GitEnv::NONE).map_err(|e| format!("list_files: {e}"))?;
		assert_eq!(listed, ls_files(root)?);
		assert!(!listed.contains(&PathBuf::from("sub/x.rs")), "git does not list inside a nested repo: {listed:?}");
		// The nested repo's own rules apply inside it, so its `.gitignore` can hide its `.git` too.
		put(root, "sub/.gitignore", ".*\n")?;
		assert_eq!(miss_reason(root), Some(MissReason::NestedRepo));
		assert_eq!(list_files(root, GitEnv::NONE).map_err(|e| format!("list_files: {e}"))?, ls_files(root)?);
		return Ok(());
	}

	#[test]
	fn a_repo_with_nothing_added_lists_its_untracked_files() -> Result<(), String> {
		let repo = new_repo()?;
		put(repo.path(), "a.rs", "x\n")?;
		assert_eq!(read_files(repo.path()).map_err(|m| format!("read_files: {m}"))?, vec![PathBuf::from("a.rs")]);
		assert_eq!(ls_files(repo.path())?, vec![PathBuf::from("a.rs")]);
		return Ok(());
	}
}
