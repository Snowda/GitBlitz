//! `git ls-files -z --cached --others --exclude-standard`, answered from `.git/index` plus a walk.
//!
//! `--cached` is every path the index tracks, even a tracked file that a `.gitignore` also matches
//! (a walk alone would drop it). `--others --exclude-standard` is every other file on disk that the
//! `.gitignore` files, `info/exclude` and the global excludes file let through, walked with the
//! `ignore` crate. Paths come back relative to `dir`, as git prints them when run in `dir`.
use foldhash::HashSet;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use ignore::{DirEntry, Match, Walk, WalkBuilder};

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use arrayvec::ArrayVec;

use crate::config::{self, RepoConfig, CONFIG_FILE};
use crate::discover::{locate, require_head, Location, DOT_GIT};
use crate::error::{Miss, MissReason};
use crate::file_list::FileList;
use crate::index;
use crate::object_format::ObjectFormat;
use crate::refs::common_dir;

const INDEX_FILE: &str = "index";
const INDEX_LOCK_FILE: &str = "index.lock";
const INFO_EXCLUDE: &str = "info/exclude";
const GITIGNORE_FILE: &str = ".gitignore";
/// The most directory levels between the work tree top and `dir` that the ignore check holds; deeper is a Miss.
pub const DIR_DEPTH_MAX: usize = 32;

/// The files `git ls-files -z --cached --others --exclude-standard` run in `dir` lists, sorted.
///
/// # Errors
/// A [`Miss`] for anything this reader does not settle: see [`MissReason`].
pub fn read_files(dir: &Path) -> Result<FileList, Miss> {
	let Location { start, top, git_dir } = locate(dir)?;
	require_head(&git_dir)?;
	let top: &Path = &top;
	let common: PathBuf = common_dir(&git_dir)?;
	// Without a config file, git uses SHA-1 and does not fold case; so does this reader.
	let config: RepoConfig = config::read(&common.join(CONFIG_FILE))?.unwrap_or_default();
	if config.object_format != ObjectFormat::Sha1 {
		return Err(Miss::new(&git_dir, MissReason::ObjectFormat));
	}
	let is_case_folded: bool = config.ignore_case;
	let mut prefix: String = slashed(start.strip_prefix(top).map_err(|_| Miss::new(&start, MissReason::NotARepo))?);
	if !prefix.is_empty() {
		prefix.push('/');
	}

	let mut files: FileList = tracked_under(&git_dir, &prefix)?;
	let is_new = untracked_filter(&files, is_case_folded);
	// git never descends into an ignored directory, so under one only the tracked files are listed.
	let exclude: PathBuf = common.join(INFO_EXCLUDE);
	if !is_under_ignored_dir(top, &start, &exclude, is_case_folded)? {
		walk(&start, top, is_case_folded, &mut files, is_new)?;
	}
	files.sort();
	files.dedup();
	return Ok(files);
}

// The dedup drops a walked path equal to a tracked one; with `core.ignoreCase`, this drops one that differs only in case.
fn untracked_filter(tracked: &FileList, is_case_folded: bool) -> impl FnMut(&str) -> bool + use<> {
	let folded: Option<HashSet<String>> = is_case_folded.then(|| return tracked.texts().map(str::to_lowercase).collect());
	let mut lower: String = String::new();
	return move |path: &str| {
		let Some(known) = folded.as_ref() else {
			return true;
		};
		// For ASCII, `make_ascii_lowercase` equals `str::to_lowercase`, so one buffer serves every path.
		if !path.is_ascii() {
			return !known.contains(&path.to_lowercase());
		}
		lower.clear();
		lower.push_str(path);
		lower.make_ascii_lowercase();
		return !known.contains(&lower);
	};
}

// Whether git's exclude rules ignore `start` or a directory between `top` and it. The walk applies
// the rules only below its root, never to the root itself, so this checks those levels.
fn is_under_ignored_dir(top: &Path, start: &Path, exclude: &Path, is_case_folded: bool) -> Result<bool, Miss> {
	let Ok(below) = start.strip_prefix(top) else {
		return Ok(false);
	};
	if below.as_os_str().is_empty() {
		return Ok(false);
	}
	let shared: [Gitignore; 2] = [matcher(top, exclude, is_case_folded), Gitignore::global().0];
	// The `.gitignore` of each level from `top` down to the parent of the next level.
	let mut levels: ArrayVec<Gitignore, DIR_DEPTH_MAX> = ArrayVec::new();
	let mut level: PathBuf = top.to_path_buf();
	for component in below.components() {
		levels.try_push(matcher(&level, &level.join(GITIGNORE_FILE), is_case_folded)).map_err(|_| return Miss::new(start, MissReason::DirDepth))?;
		level.push(component);
		if is_ignored_dir(&level, &levels, &shared) {
			return Ok(true);
		}
	}
	return Ok(false);
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
fn tracked_under(git_dir: &Path, prefix: &str) -> Result<FileList, Miss> {
	if git_dir.join(INDEX_LOCK_FILE).exists() {
		return Err(Miss::new(&git_dir.join(INDEX_LOCK_FILE), MissReason::IndexLocked));
	}
	let path: PathBuf = git_dir.join(INDEX_FILE);
	let bytes: Vec<u8> = match fs::read(&path) {
		Ok(bytes) => bytes,
		Err(err) if err.kind() == ErrorKind::NotFound => return Ok(FileList::default()),
		Err(err) => return Err(Miss::io(&path, &err)),
	};
	let mut paths: FileList = index::tracked_paths(bytes, &path)?;
	paths.retain_under(prefix);
	return Ok(paths);
}

// Appends to `files` every file under `start` that git's exclude rules let through and `keep` accepts, relative
// to `start`. A nested repo (or a submodule) is listed by `ls-files` as one entry, which a file walk cannot reproduce: a Miss.
fn walk(start: &Path, top: &Path, is_case_folded: bool, files: &mut FileList, mut keep: impl FnMut(&str) -> bool) -> Result<(), Miss> {
	for entry in walker(start, is_case_folded) {
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
		files.push_slashed(&relative.to_string_lossy());
		if files.last().is_some_and(|path: &str| return !keep(path)) {
			files.pop();
		}
	}
	return Ok(());
}

// A walk under `start` with git's exclude rules: `.gitignore` files, `info/exclude` and the global excludes file.
fn walker(start: &Path, is_case_folded: bool) -> Walk {
	return WalkBuilder::new(start)
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
}

fn slashed(path: &Path) -> String {
	return path.to_string_lossy().replace('\\', "/");
}

#[cfg(test)]
mod tests {
	use std::fs;
	use std::io::ErrorKind;
	use std::path::{Path, PathBuf};

	use foldhash::HashSet;

	use super::{is_under_ignored_dir, read_files, untracked_filter, DIR_DEPTH_MAX};
	use crate::env::GitEnv;
	use crate::error::MissReason;
	use crate::file_list::FileList;
	use crate::testrepo::{commit, git, new_repo, ok, TestResult};
	use crate::{list_files, LS_FILES_ARGS};

	// The oracle: what git itself lists from `dir`.
	fn ls_files(dir: &Path) -> TestResult<Vec<PathBuf>> {
		let listed: String = git(dir, LS_FILES_ARGS)?;
		let mut files: Vec<PathBuf> = listed.split('\0').filter(|p| !p.is_empty()).map(PathBuf::from).collect();
		files.sort();
		return Ok(files);
	}

	fn has(files: &FileList, path: &str) -> bool {
		return files.iter().any(|listed: &Path| return listed == Path::new(path));
	}

	fn put(root: &Path, rel: &str, text: &str) -> TestResult {
		let path: PathBuf = root.join(rel);
		ok(fs::create_dir_all(path.parent().unwrap_or(root)), &format!("mkdir {rel}"))?;
		return ok(fs::write(&path, text), &format!("write {rel}"));
	}

	fn miss_reason(dir: &Path) -> Option<MissReason> {
		return read_files(dir).err().map(|m| m.reason);
	}

	// Tracked, tracked-and-ignored, untracked, ignored, nested and deleted files in one work tree.
	fn work_tree() -> TestResult<tempfile::TempDir> {
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
		ok(fs::remove_file(root.join("gone.rs")), "rm")?;
		put(root, "new.rs", "untracked\n")?;
		put(root, "sub/new.txt", "untracked too\n")?;
		put(root, "noise.log", "ignored\n")?;
		put(root, "build/out.o", "ignored dir\n")?;
		return Ok(repo);
	}

	#[test]
	fn a_work_tree_lists_exactly_what_ls_files_lists() -> TestResult {
		let repo = work_tree()?;
		let root: &Path = repo.path();
		let got: FileList = ok(read_files(root), "read_files")?;
		assert_eq!(got, *ls_files(root)?);
		// The index is what keeps the tracked-but-ignored file and the deleted one: a walk sees neither.
		assert!(has(&got, "forced.log") && has(&got, "gone.rs"), "{got:?}");
		assert!(!has(&got, "noise.log") && !has(&got, "build/out.o"), "{got:?}");
		// From a subdirectory, paths are relative to it and cover only its subtree.
		let sub: PathBuf = root.join("sub");
		assert_eq!(ok(read_files(&sub), "read_files(sub)")?, *ls_files(&sub)?);
		assert_eq!(ok(list_files(root, GitEnv::NONE), "list_files")?, got, "the reader answers");
		return Ok(());
	}

	fn is_io_miss_at(dir: &Path, at: &Path) -> bool {
		return read_files(dir).err().is_some_and(|m| {
			return m.path == at && matches!(m.reason, MissReason::Io(kind) if kind != ErrorKind::NotFound);
		});
	}

	// A SHA-256 index has wider entries than the reader parses, so it misses and `list_files` asks git.
	#[test]
	fn a_sha256_repo_misses_and_list_files_asks_git() -> TestResult {
		let repo = ok(tempfile::tempdir(), "tempdir")?;
		git(repo.path(), &["init", "-q", "-b", "main", "--object-format=sha256"])?;
		put(repo.path(), "a.rs", "x\n")?;
		git(repo.path(), &["add", "a.rs"])?;
		assert_eq!(miss_reason(repo.path()), Some(MissReason::ObjectFormat));
		assert_eq!(ok(list_files(repo.path(), GitEnv::NONE), "list_files")?, *ls_files(repo.path())?);
		assert_eq!(ls_files(repo.path())?, [Path::new("a.rs")]);
		return Ok(());
	}

	// No config means no case folding and SHA-1, as git assumes too.
	#[test]
	fn a_missing_config_reads_as_empty() -> TestResult {
		let repo = work_tree()?;
		ok(fs::remove_file(repo.path().join(".git/config")), "rm config")?;
		assert_eq!(ok(read_files(repo.path()), "read_files")?, *ls_files(repo.path())?);
		return Ok(());
	}

	// An index or config that does not read as a file is an I/O miss, never an empty listing.
	#[test]
	fn an_unreadable_index_or_config_is_an_io_miss() -> TestResult {
		let repo = new_repo()?;
		put(repo.path(), "a.rs", "x\n")?;
		let index: PathBuf = repo.path().join(".git/index");
		ok(fs::create_dir(&index), "mkdir index")?;
		assert!(is_io_miss_at(repo.path(), &index), "{:?}", read_files(repo.path()));
		ok(fs::remove_dir(&index), "rmdir index")?;
		let config: PathBuf = repo.path().join(".git/config");
		ok(fs::remove_file(&config), "rm config")?;
		ok(fs::create_dir(&config), "mkdir config")?;
		assert!(is_io_miss_at(repo.path(), &config), "{:?}", read_files(repo.path()));
		return Ok(());
	}

	#[test]
	fn from_inside_an_ignored_directory_only_its_tracked_files_are_listed() -> TestResult {
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
		let got: FileList = ok(read_files(&sub), "read_files")?;
		assert_eq!(got, *ls_files(&sub)?);
		assert_eq!(got, ["kept.rs"], "the walk adds nothing under an ignored directory");
		return Ok(());
	}

	// Two levels down, the nearer `.gitignore` overrides the top one: git lists `a/deep` but not `c/deep`.
	#[test]
	fn the_nearest_gitignore_decides_for_a_directory_two_levels_down() -> TestResult {
		let repo = new_repo()?;
		let root: &Path = repo.path();
		put(root, ".gitignore", "deep/\n")?;
		put(root, "a/.gitignore", "!deep/\n")?;
		put(root, "a/deep/x/y.rs", "whitelisted by a/.gitignore\n")?;
		put(root, "c/deep/x/y.rs", "ignored by the top .gitignore\n")?;
		git(root, &["add", ".gitignore", "a/.gitignore"])?;
		commit(root, "fixture")?;
		let kept: PathBuf = root.join("a/deep/x");
		assert_eq!(ok(read_files(&kept), "read_files(a)")?, *ls_files(&kept)?);
		assert_eq!(ls_files(&kept)?, [Path::new("y.rs")], "git lists the whitelisted file");
		let dropped: PathBuf = root.join("c/deep/x");
		assert_eq!(ok(read_files(&dropped), "read_files(c)")?, *ls_files(&dropped)?);
		assert!(ls_files(&dropped)?.is_empty(), "git lists nothing under the ignored directory");
		return Ok(());
	}

	#[test]
	fn a_path_compressed_v4_index_reads_the_same() -> TestResult {
		let repo = work_tree()?;
		git(repo.path(), &["update-index", "--index-version", "4"])?;
		let header: Vec<u8> = ok(fs::read(repo.path().join(".git/index")), "read index")?;
		assert_eq!(header.get(4..8), Some([0u8, 0, 0, 4].as_slice()), "the fixture really is a v4 index");
		assert_eq!(ok(read_files(repo.path()), "read_files")?, *ls_files(repo.path())?);
		return Ok(());
	}

	#[test]
	fn layouts_the_reader_does_not_settle_miss_and_list_files_asks_git() -> TestResult {
		let split = work_tree()?;
		git(split.path(), &["update-index", "--split-index"])?;
		assert_eq!(miss_reason(split.path()), Some(MissReason::UnsupportedIndex));
		assert_eq!(ok(list_files(split.path(), GitEnv::NONE), "list_files")?, *ls_files(split.path())?);

		let nested = work_tree()?;
		git(&nested.path().join("sub"), &["init", "-q"])?;
		assert_eq!(miss_reason(nested.path()), Some(MissReason::NestedRepo));
		assert_eq!(ok(list_files(nested.path(), GitEnv::NONE), "list_files")?, *ls_files(nested.path())?);

		let locked = work_tree()?;
		put(locked.path(), ".git/index.lock", "")?;
		assert_eq!(miss_reason(locked.path()), Some(MissReason::IndexLocked));
		return Ok(());
	}

	// A `.*` rule hides the nested `.git` from the walk, yet git still lists `sub/` as one nested repo.
	#[test]
	fn a_nested_repo_whose_dot_git_is_ignored_still_misses() -> TestResult {
		let repo = new_repo()?;
		let root: &Path = repo.path();
		put(root, ".gitignore", ".*\n!.gitignore\n")?;
		put(root, "a.rs", "x\n")?;
		git(root, &["add", ".gitignore", "a.rs"])?;
		commit(root, "fixture")?;
		put(root, "sub/x.rs", "in the nested repo\n")?;
		git(&root.join("sub"), &["init", "-q"])?;
		assert_eq!(miss_reason(root), Some(MissReason::NestedRepo));
		let listed: FileList = ok(list_files(root, GitEnv::NONE), "list_files")?;
		assert_eq!(listed, *ls_files(root)?);
		assert!(!has(&listed, "sub/x.rs"), "git does not list inside a nested repo: {listed:?}");
		// The nested repo's own rules apply inside it, so its `.gitignore` can hide its `.git` too.
		put(root, "sub/.gitignore", ".*\n")?;
		assert_eq!(miss_reason(root), Some(MissReason::NestedRepo));
		assert_eq!(ok(list_files(root, GitEnv::NONE), "list_files")?, *ls_files(root)?);
		return Ok(());
	}

	// A repo with `Readme.md` committed, renamed on disk to `README.md`, and `core.ignoreCase` set to `value`.
	fn renamed_case(value: &str) -> TestResult<tempfile::TempDir> {
		let repo = new_repo()?;
		let root: &Path = repo.path();
		put(root, "Readme.md", "x\n")?;
		git(root, &["add", "Readme.md"])?;
		commit(root, "fixture")?;
		ok(fs::rename(root.join("Readme.md"), root.join("README.md")), "rename")?;
		let config: PathBuf = root.join(".git/config");
		let text: String = ok(fs::read_to_string(&config), "read config")?;
		let kept: String = text.lines().filter(|l| !l.to_ascii_lowercase().contains("ignorecase")).collect::<Vec<_>>().join("\n");
		ok(fs::write(&config, format!("{kept}\n[core]\n\tignorecase = {value}\n")), "write config")?;
		return Ok(repo);
	}

	// git folds case for `1` as for `true`: the renamed file matches its index entry and is not untracked.
	#[test]
	fn ignorecase_set_to_1_folds_case_like_git() -> TestResult {
		let folded = renamed_case("1")?;
		let got: FileList = ok(read_files(folded.path()), "read_files(1)")?;
		assert_eq!(got, *ls_files(folded.path())?);
		assert_eq!(got, ["Readme.md"], "folded: only the index entry");
		let exact = renamed_case("0")?;
		let got: FileList = ok(read_files(exact.path()), "read_files(0)")?;
		assert_eq!(got, *ls_files(exact.path())?);
		assert_eq!(got, ["README.md", "Readme.md"], "not folded: the rename is untracked");
		return Ok(());
	}

	#[test]
	fn a_repo_with_nothing_added_lists_its_untracked_files() -> TestResult {
		let repo = new_repo()?;
		put(repo.path(), "a.rs", "x\n")?;
		assert_eq!(ok(read_files(repo.path()), "read_files")?, ["a.rs"]);
		assert_eq!(ls_files(repo.path())?, [Path::new("a.rs")]);
		return Ok(());
	}

	// No level lies between `top` and a start outside it, so even a `*` rule ignores nothing.
	#[test]
	fn a_start_outside_top_is_never_under_an_ignored_dir() -> TestResult {
		let top = ok(tempfile::tempdir(), "tempdir(top)")?;
		let elsewhere = ok(tempfile::tempdir(), "tempdir(elsewhere)")?;
		put(top.path(), ".gitignore", "*\n")?;
		let exclude: PathBuf = top.path().join(".git/info/exclude");
		assert_eq!(is_under_ignored_dir(top.path(), elsewhere.path(), &exclude, false), Ok(false));
		assert_eq!(is_under_ignored_dir(top.path(), top.path(), &exclude, false), Ok(false), "top itself is never checked");
		assert_eq!(is_under_ignored_dir(top.path(), &top.path().join("x"), &exclude, false), Ok(true), "the same rule ignores a level below top");
		return Ok(());
	}

	// The ignore check holds `DIR_DEPTH_MAX` levels: one more misses, and `list_files` asks git.
	#[test]
	fn a_start_deeper_than_the_level_buffer_misses_and_list_files_asks_git() -> TestResult {
		let repo = new_repo()?;
		let root: &Path = repo.path();
		let deepest: String = vec!["d"; DIR_DEPTH_MAX + 1].join("/");
		put(root, &format!("{deepest}/a.rs"), "x\n")?;
		let held: PathBuf = root.join(vec!["d"; DIR_DEPTH_MAX].join("/"));
		assert_eq!(ok(read_files(&held), "read_files(held)")?, *ls_files(&held)?);
		assert_eq!(ls_files(&held)?, [Path::new("d/a.rs")]);
		let deeper: PathBuf = root.join(&deepest);
		assert_eq!(miss_reason(&deeper), Some(MissReason::DirDepth));
		assert_eq!(ok(list_files(&deeper, GitEnv::NONE), "list_files")?, *ls_files(&deeper)?);
		return Ok(());
	}

	// The oracle is the old filter, `str::to_lowercase` on every path, including non-ASCII and final sigma.
	#[test]
	fn the_folded_filter_matches_to_lowercase_on_every_path() {
		let mut tracked: FileList = FileList::default();
		["Readme.md", "Ünï.TXT", "ΟΔΟΣ", "a/B.rs"].iter().for_each(|path| tracked.push_slashed(path));
		let known: HashSet<String> = tracked.texts().map(str::to_lowercase).collect();
		let mut is_new = untracked_filter(&tracked, true);
		for probe in ["README.MD", "readme.md", "üNÏ.txt", "οδοσ", "ΟΔΟΣ", "A/b.RS", "new.rs", "Ünï.TXT2"] {
			assert_eq!(is_new(probe), !known.contains(&probe.to_lowercase()), "{probe}");
		}
		assert!(!is_new("README.MD") && is_new("new.rs"), "the oracle sees both outcomes");
		let mut exact = untracked_filter(&tracked, false);
		assert!(exact("Readme.md"), "without folding every walked path passes; the dedup drops exact matches");
	}
}
