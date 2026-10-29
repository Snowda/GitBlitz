//! Answers `git rev-parse --absolute-git-dir` and `git rev-parse HEAD` by reading files under `.git`.
//!
//! [`git_dir`] and [`head`] try the file path first and spawn `git rev-parse` only when it cannot answer for
//! sure; [`read_git_dir`] and [`read_head`] are the file path alone, returning a [`Miss`] instead of spawning.
//! The caller passes the `GIT_*` variables git should see as a [`GitEnv`]; GitBlitz reads no environment itself.

mod discover;
mod env;
mod error;
mod fallback;
#[cfg(feature = "list")]
mod index;
#[cfg(test)]
mod isolating_config;
#[cfg(feature = "list")]
mod listing;
mod object_format;
mod refs;
mod small_file;
#[cfg(test)]
mod testrepo;

use std::path::{Path, PathBuf};

pub use discover::read_git_dir;
pub use env::GitEnv;
pub use error::{Cause, Error, Miss, MissReason};
pub use refs::{read_head, OidHex, OID_HEX_MAX};
#[cfg(feature = "list")]
pub use listing::read_files;

const GIT_DIR_ARGS: &[&str] = &["rev-parse", "--absolute-git-dir"];
const HEAD_ARGS: &[&str] = &["rev-parse", "HEAD"];
#[cfg(feature = "list")]
const LS_FILES_ARGS: &[&str] = &["ls-files", "-z", "--cached", "--others", "--exclude-standard"];

/// The absolute git directory for `dir`, as `git rev-parse --absolute-git-dir` run in `dir` with `env` reports it.
///
/// # Errors
/// [`Error`] when the file path missed and `git` failed too; it names `dir`, git's stderr and the missed path.
pub fn git_dir(dir: &Path, env: GitEnv<'_>) -> Result<PathBuf, Error> {
	return match unless_redirected(env, || return read_git_dir(dir)) {
		Ok(path) => Ok(path),
		Err(miss) => fallback::rev_parse(dir, GIT_DIR_ARGS, env, miss).map(PathBuf::from),
	};
}

/// The commit id of HEAD, as `git rev-parse HEAD` run in `dir` with `env` reports it: 40 hex, or 64 in a
/// SHA-256 repo.
///
/// # Errors
/// [`Error`] when the file path missed and `git` failed too (e.g. an unborn branch).
pub fn head(dir: &Path, env: GitEnv<'_>) -> Result<OidHex, Error> {
	return match unless_redirected(env, || return read_head(dir)) {
		Ok(oid) => Ok(oid),
		Err(miss) => fallback::rev_parse_into(dir, HEAD_ARGS, env, miss, |text: String| return oid_hex(&text)),
	};
}

// The file reader's answer, unless a caller variable redirects git: the reader cannot follow one.
fn unless_redirected<T>(env: GitEnv<'_>, read: impl FnOnce() -> Result<T, Miss>) -> Result<T, Miss> {
	return match env.redirect() {
		Some(name) => Err(Miss::new(Path::new(name), MissReason::GitEnv)),
		None => read(),
	};
}

// git's answer as an inline oid; git checks the hex before it prints, so this only checks that the text fits.
fn oid_hex(text: &str) -> Result<OidHex, Cause> {
	return OidHex::from(text).map_err(|_| Cause::NotAnOid(text.len()));
}

/// The files `git ls-files -z --cached --others --exclude-standard` run in `dir` with `env` lists, relative
/// to `dir`, sorted. [`read_files`] answers from `.git/index` and a walk; a [`Miss`] runs `git`.
///
/// # Errors
/// [`Error`] when the file path missed and `git` failed too.
#[cfg(feature = "list")]
pub fn list_files(dir: &Path, env: GitEnv<'_>) -> Result<Vec<PathBuf>, Error> {
	return match unless_redirected(env, || return read_files(dir)) {
		Ok(files) => Ok(files),
		Err(miss) => {
			let listed: String = fallback::rev_parse(dir, LS_FILES_ARGS, env, miss)?;
			let mut files: Vec<PathBuf> = listed.split('\0').filter(|p| !p.is_empty()).map(PathBuf::from).collect();
			files.sort();
			Ok(files)
		}
	};
}

#[cfg(test)]
mod tests {
	use std::ffi::OsStr;
	use std::fs::{self, OpenOptions};
	use std::io::{ErrorKind, Write};
	use std::path::{Path, PathBuf};

	use tempfile::{tempdir, TempDir};

	use super::{git_dir, head, oid_hex, read_git_dir, read_head, unless_redirected, Cause, Error, GitEnv, Miss, MissReason, OidHex};
	use crate::small_file::SMALL_FILE_MAX;
	use crate::testrepo::{commit, expect_err, git, git_fails, is_oid, new_repo, same_dir};

	fn utf8(path: &Path) -> Result<&str, String> {
		return path.to_str().ok_or_else(|| format!("{} is not UTF-8", path.display()));
	}

	// A repo with one commit on `main`.
	fn committed_repo() -> Result<TempDir, String> {
		let repo: TempDir = new_repo()?;
		commit(repo.path(), "one")?;
		return Ok(repo);
	}

	fn assert_git_dir_matches(dir: &Path) -> Result<(), String> {
		let fast: PathBuf = read_git_dir(dir).map_err(|err| format!("fast path should answer: {err:?}"))?;
		let truth: String = git(dir, &["rev-parse", "--absolute-git-dir"])?;
		assert!(same_dir(&fast, Path::new(&truth))?, "{} vs git {truth}", fast.display());
		let slow: PathBuf = git_dir(dir, GitEnv::NONE).map_err(|err| format!("git_dir(dir): {err:?}"))?;
		assert!(same_dir(&slow, Path::new(&truth))?);
		return Ok(());
	}

	fn assert_head_matches(dir: &Path) -> Result<String, String> {
		let fast: OidHex = read_head(dir).map_err(|err| format!("fast path should answer: {err:?}"))?;
		assert_eq!(fast.as_str(), git(dir, &["rev-parse", "HEAD"])?);
		assert_eq!(head(dir, GitEnv::NONE).map_err(|err| format!("head(dir): {err:?}"))?, fast);
		assert!(is_oid(&fast), "{fast}");
		return Ok(fast.as_str().to_owned());
	}

	// The reader misses for `reason`, and `head` still answers what git prints.
	fn assert_head_falls_back(dir: &Path, reason: MissReason) -> Result<Miss, String> {
		let miss: Miss = expect_err(read_head(dir), "read_head")?;
		assert_eq!(miss.reason, reason);
		let slow: OidHex = head(dir, GitEnv::NONE).map_err(|err| format!("head: {err:?}"))?;
		assert_eq!(slow.as_str(), git(dir, &["rev-parse", "HEAD"])?);
		return Ok(miss);
	}

	// git's answer converts whole up to a SHA-256 oid's length; longer output is refused, not truncated.
	#[test]
	fn oid_hex_keeps_up_to_64_digits_and_refuses_more() -> Result<(), String> {
		let longest: String = "ab".repeat(32);
		let whole: OidHex = oid_hex(&longest).map_err(|err| format!("oid_hex(64 digits): {err:?}"))?;
		assert_eq!(whole.as_str(), longest);
		assert!(matches!(oid_hex(&format!("{longest}0")), Err(Cause::NotAnOid(65))));
		assert_eq!(Cause::NotAnOid(65).to_string(), "printed 65 bytes, more than the 64 hex digits of an object id");
		return Ok(());
	}

	#[test]
	fn normal_repo_answers_from_files() -> Result<(), String> {
		let repo: TempDir = committed_repo()?;
		assert_git_dir_matches(repo.path())?;
		assert_head_matches(repo.path())?;
		return Ok(());
	}

	#[test]
	fn subdirectory_climbs_to_the_repo() -> Result<(), String> {
		let repo: TempDir = committed_repo()?;
		let sub: PathBuf = repo.path().join("a").join("b");
		fs::create_dir_all(&sub).map_err(|err| format!("create {}: {err:?}", sub.display()))?;
		assert_git_dir_matches(&sub)?;
		assert_head_matches(&sub)?;
		return Ok(());
	}

	// The reader misses on a SHA-256 repo's 64-hex ref, so `head` relays git's answer whole.
	#[test]
	fn sha256_repo_head_is_what_git_prints() -> Result<(), String> {
		let repo: TempDir = tempdir().map_err(|err| format!("tempdir(): {err:?}"))?;
		git(repo.path(), &["init", "-q", "-b", "main", "--object-format=sha256"])?;
		commit(repo.path(), "one")?;
		let truth: String = git(repo.path(), &["rev-parse", "HEAD"])?;
		assert_eq!(truth.len(), 64, "{truth}");
		assert_head_falls_back(repo.path(), MissReason::Malformed)?;
		return Ok(());
	}

	#[test]
	fn head_tracks_a_new_commit() -> Result<(), String> {
		let repo: TempDir = committed_repo()?;
		let before: String = assert_head_matches(repo.path())?;
		commit(repo.path(), "two")?;
		let after: String = assert_head_matches(repo.path())?;
		assert_ne!(before, after);
		assert_eq!(git(repo.path(), &["rev-parse", "HEAD~1"])?, before);
		return Ok(());
	}

	#[test]
	fn linked_worktree_resolves_its_own_git_dir_and_branch() -> Result<(), String> {
		let repo: TempDir = committed_repo()?;
		let wt: PathBuf = repo.path().join("wt");
		git(repo.path(), &["worktree", "add", "-b", "side", utf8(&wt)?])?;
		commit(&wt, "on side")?;
		assert!(wt.join(".git").is_file());
		assert_git_dir_matches(&wt)?;
		let side: String = assert_head_matches(&wt)?;
		let main: String = assert_head_matches(repo.path())?;
		assert_ne!(side, main);
		let linked: PathBuf = read_git_dir(&wt).map_err(|err| format!("read_git_dir(&wt): {err:?}"))?;
		assert!(same_dir(&linked, &repo.path().join(".git").join("worktrees").join("wt"))?);
		return Ok(());
	}

	#[test]
	fn packed_branch_is_read_from_packed_refs() -> Result<(), String> {
		let repo: TempDir = committed_repo()?;
		git(repo.path(), &["pack-refs", "--all"])?;
		assert!(!repo.path().join(".git/refs/heads/main").exists());
		assert_head_matches(repo.path())?;
		return Ok(());
	}

	#[test]
	fn detached_head_is_read_directly() -> Result<(), String> {
		let repo: TempDir = committed_repo()?;
		let first: String = git(repo.path(), &["rev-parse", "HEAD"])?;
		commit(repo.path(), "two")?;
		git(repo.path(), &["switch", "--detach", &first])?;
		let head_file: String = fs::read_to_string(repo.path().join(".git/HEAD")).map_err(|err| format!("read .git/HEAD: {err:?}"))?;
		assert_eq!(head_file.trim(), first);
		assert_eq!(assert_head_matches(repo.path())?, first);
		return Ok(());
	}

	#[test]
	fn unborn_branch_falls_back_and_fails_like_git() -> Result<(), String> {
		let repo: TempDir = new_repo()?;
		assert_git_dir_matches(repo.path())?;
		assert_eq!(expect_err(read_head(repo.path()), "read_head")?.reason, MissReason::Unborn);
		let git_stderr: String = git_fails(repo.path(), &["rev-parse", "HEAD"])?;
		let err: Error = expect_err(head(repo.path(), GitEnv::NONE), "head")?;
		assert!(matches!(&err.cause, Cause::Exit { stderr, .. } if *stderr == git_stderr), "expected git's failure: {err}");
		assert_eq!(err.miss.reason, MissReason::Unborn);
		assert!(err.miss.path.ends_with("refs/heads/main"), "{}", err.miss.path.display());
		return Ok(());
	}

	#[test]
	fn branch_pointing_at_a_ref_falls_back() -> Result<(), String> {
		let repo: TempDir = committed_repo()?;
		git(repo.path(), &["symbolic-ref", "refs/heads/alias", "refs/heads/main"])?;
		git(repo.path(), &["symbolic-ref", "HEAD", "refs/heads/alias"])?;
		assert_head_falls_back(repo.path(), MissReason::SymbolicRef)?;
		return Ok(());
	}

	// Without `extensions.refStorage` git keeps the files backend, so a stray `reftable/` changes neither answer.
	#[test]
	fn a_stray_reftable_dir_is_ignored_like_git_does() -> Result<(), String> {
		let repo: TempDir = committed_repo()?;
		let before: String = assert_head_matches(repo.path())?;
		fs::create_dir(repo.path().join(".git/reftable")).map_err(|err| format!("create .git/reftable: {err:?}"))?;
		assert_eq!(assert_head_matches(repo.path())?, before);
		return Ok(());
	}

	// read_head reads HEAD to check it. When HEAD is absent or is a directory, the miss is still `NoHead`.
	#[test]
	fn read_head_reports_a_missing_or_directory_head_as_no_head() -> Result<(), String> {
		let repo: TempDir = committed_repo()?;
		let head: PathBuf = repo.path().join(".git/HEAD");
		fs::remove_file(&head).map_err(|err| format!("remove HEAD: {err:?}"))?;
		assert_eq!(expect_err(read_head(repo.path()), "read_head(no HEAD)")?, Miss::new(&head, MissReason::NoHead));
		fs::create_dir(&head).map_err(|err| format!("mkdir HEAD: {err:?}"))?;
		assert_eq!(expect_err(read_head(repo.path()), "read_head(HEAD dir)")?, Miss::new(&head, MissReason::NoHead));
		return Ok(());
	}

	#[test]
	fn ref_storage_config_falls_back() -> Result<(), String> {
		let repo: TempDir = committed_repo()?;
		let config: PathBuf = repo.path().join(".git/config");
		let text: String = fs::read_to_string(&config).map_err(|err| format!("read config: {err:?}"))?;
		fs::write(&config, format!("{text}[extensions]\n\trefStorage = files\n")).map_err(|err| format!("write config: {err:?}"))?;
		assert_eq!(expect_err(read_head(repo.path()), "read_head")?.reason, MissReason::Reftable);
		return Ok(());
	}

	#[test]
	fn malformed_loose_ref_falls_back_to_git() -> Result<(), String> {
		let repo: TempDir = committed_repo()?;
		let loose: PathBuf = repo.path().join(".git/refs/heads/main");
		let oid: String = fs::read_to_string(&loose).map_err(|err| format!("read loose ref: {err:?}"))?;
		fs::write(&loose, oid.trim().to_uppercase()).map_err(|err| format!("write loose ref: {err:?}"))?;
		let miss: Miss = assert_head_falls_back(repo.path(), MissReason::Malformed)?;
		assert!(miss.path.ends_with("refs/heads/main"));
		assert_eq!(head(repo.path(), GitEnv::NONE).map_err(|err| format!("head: {err:?}"))?.as_str(), oid.trim());
		return Ok(());
	}

	// git accepts whitespace after a loose ref's oid; past the reader's buffer it misses, and git still answers.
	#[test]
	fn a_loose_ref_longer_than_the_read_buffer_falls_back_to_git() -> Result<(), String> {
		let repo: TempDir = committed_repo()?;
		let loose: PathBuf = repo.path().join(".git/refs/heads/main");
		let oid: String = fs::read_to_string(&loose).map_err(|err| format!("read loose ref: {err:?}"))?;
		let padded: String = format!("{}\n{}", oid.trim(), " ".repeat(SMALL_FILE_MAX));
		fs::write(&loose, padded).map_err(|err| format!("write loose ref: {err:?}"))?;
		assert_head_falls_back(repo.path(), MissReason::Io(ErrorKind::FileTooLarge))?;
		assert_eq!(head(repo.path(), GitEnv::NONE).map_err(|err| format!("head: {err:?}"))?.as_str(), oid.trim());
		return Ok(());
	}

	#[test]
	fn relative_gitfile_resolves_against_its_folder() -> Result<(), String> {
		let root: TempDir = new_repo()?;
		let work: PathBuf = root.path().join("work");
		let store: PathBuf = root.path().join("store");
		fs::create_dir_all(&work).map_err(|err| format!("create work: {err:?}"))?;
		fs::create_dir_all(&store).map_err(|err| format!("create store: {err:?}"))?;
		git(&work, &["init", "-q", "-b", "main", "--separate-git-dir", utf8(&store.join("repo.git"))?])?;
		fs::write(work.join(".git"), "gitdir: ../store/repo.git\n").map_err(|err| format!("write gitfile: {err:?}"))?;
		commit(&work, "one")?;
		assert_git_dir_matches(&work)?;
		assert_head_matches(&work)?;
		return Ok(());
	}

	#[test]
	fn bare_repo_nested_in_a_work_tree_defers_to_git() -> Result<(), String> {
		let repo: TempDir = new_repo()?;
		let bare: PathBuf = repo.path().join("inner.git");
		git(repo.path(), &["init", "-q", "--bare", utf8(&bare)?])?;
		assert_eq!(expect_err(read_git_dir(&bare), "read_git_dir")?.reason, MissReason::BareCandidate);
		let slow: PathBuf = git_dir(&bare, GitEnv::NONE).map_err(|err| format!("git_dir(&bare): {err:?}"))?;
		assert!(same_dir(&slow, &bare)?);
		return Ok(());
	}

	// With no `.git` or `HEAD` in any ancestor both refuse, and the error relays git's own words.
	#[test]
	fn outside_any_repo_the_reader_and_git_both_refuse() -> Result<(), String> {
		let dir: TempDir = tempdir().map_err(|err| format!("tempdir(): {err:?}"))?;
		let git_stderr: String = git_fails(dir.path(), &["rev-parse", "--absolute-git-dir"])?;
		assert!(git_stderr.contains("not a git repository"), "{git_stderr}");
		assert_eq!(expect_err(read_git_dir(dir.path()), "read_git_dir")?, Miss::new(dir.path(), MissReason::NotARepo));
		assert_eq!(expect_err(read_head(dir.path()), "read_head")?.reason, MissReason::NotARepo);
		let err: Error = expect_err(git_dir(dir.path(), GitEnv::NONE), "git_dir")?;
		assert!(matches!(&err.cause, Cause::Exit { stderr, .. } if *stderr == git_stderr), "{err}");
		assert_eq!(err.miss.reason, MissReason::NotARepo);
		return Ok(());
	}

	#[test]
	fn missing_dir_reports_the_dir() -> Result<(), String> {
		let repo: TempDir = new_repo()?;
		let err: Error = expect_err(git_dir(&repo.path().join("missing"), GitEnv::NONE), "git_dir")?;
		assert!(matches!(err.cause, Cause::Spawn(_)), "{err}");
		assert!(err.to_string().contains("missing"), "{err}");
		return Ok(());
	}

	// A caller's GIT_DIR skips the file reader and reaches git, so both answers move to the other repo.
	#[test]
	fn a_callers_git_dir_moves_the_answer_to_that_repo() -> Result<(), String> {
		let here: TempDir = committed_repo()?;
		let there: TempDir = committed_repo()?;
		commit(there.path(), "two")?;
		let there_git: PathBuf = there.path().join(".git");
		let vars: [(&str, &OsStr); 1] = [("GIT_DIR", there_git.as_os_str())];
		let redirected: GitEnv<'_> = GitEnv::new(&vars);
		let without: PathBuf = git_dir(here.path(), GitEnv::NONE).map_err(|err| format!("git_dir: {err}"))?;
		let with: PathBuf = git_dir(here.path(), redirected).map_err(|err| format!("git_dir(GIT_DIR): {err}"))?;
		assert!(same_dir(&without, &here.path().join(".git"))? && same_dir(&with, &there_git)?, "{with:?}");
		assert!(!same_dir(&with, &without)?);
		let head_with: OidHex = head(here.path(), redirected).map_err(|err| format!("head(GIT_DIR): {err}"))?;
		assert_eq!(head_with.as_str(), git(there.path(), &["rev-parse", "HEAD"])?);
		assert_ne!(head_with, head(here.path(), GitEnv::NONE).map_err(|err| format!("head: {err}"))?);
		let gone: PathBuf = here.path().join("no-such.git");
		let bad: [(&str, &OsStr); 1] = [("GIT_DIR", gone.as_os_str())];
		let err: Error = expect_err(git_dir(here.path(), GitEnv::new(&bad)), "git_dir(bad GIT_DIR)")?;
		assert_eq!((err.miss.reason, err.miss.path.as_path()), (MissReason::GitEnv, Path::new("GIT_DIR")));
		return Ok(());
	}

	fn reader_answer() -> Result<u8, Miss> {
		return Ok(7);
	}

	// With no redirect the reader's answer comes back. A variable that redirects git replaces it with a `GitEnv` miss.
	#[test]
	fn unless_redirected_reads_only_without_a_redirect() {
		assert_eq!(unless_redirected(GitEnv::NONE, reader_answer), Ok(7));
		let harmless: [(&str, &OsStr); 1] = [("GIT_AUTHOR_NAME", OsStr::new("x"))];
		assert_eq!(unless_redirected(GitEnv::new(&harmless), reader_answer), Ok(7));
		let work_tree: [(&str, &OsStr); 2] = [("GIT_AUTHOR_NAME", OsStr::new("x")), ("GIT_WORK_TREE", OsStr::new("w"))];
		let got: Result<u8, Miss> = unless_redirected(GitEnv::new(&work_tree), reader_answer);
		assert_eq!(got, Err(Miss::new(Path::new("GIT_WORK_TREE"), MissReason::GitEnv)));
	}

	// A redirect to a git dir that does not exist skips the reader, and `head` relays git's own refusal.
	#[test]
	fn head_under_a_missing_git_dir_relays_gits_failure() -> Result<(), String> {
		let repo: TempDir = committed_repo()?;
		let gone: PathBuf = repo.path().join("no-such.git");
		let bad: [(&str, &OsStr); 1] = [("GIT_DIR", gone.as_os_str())];
		let err: Error = expect_err(head(repo.path(), GitEnv::new(&bad)), "head(bad GIT_DIR)")?;
		assert_eq!((err.miss.reason, err.miss.path.as_path()), (MissReason::GitEnv, Path::new("GIT_DIR")));
		assert_eq!(err.args, ["rev-parse", "HEAD"]);
		assert!(matches!(&err.cause, Cause::Exit { stderr, .. } if stderr.contains("not a git repository")), "{err}");
		return Ok(());
	}

	// `list_files` passes git's failure through its `?` and does not return an empty list.
	#[cfg(feature = "list")]
	#[test]
	fn list_files_under_a_missing_git_dir_relays_gits_failure() -> Result<(), String> {
		let repo: TempDir = committed_repo()?;
		let gone: PathBuf = repo.path().join("no-such.git");
		let bad: [(&str, &OsStr); 1] = [("GIT_DIR", gone.as_os_str())];
		let err: Error = expect_err(super::list_files(repo.path(), GitEnv::new(&bad)), "list_files(bad GIT_DIR)")?;
		assert_eq!((err.miss.reason, err.miss.path.as_path()), (MissReason::GitEnv, Path::new("GIT_DIR")));
		assert_eq!(err.args, ["ls-files", "-z", "--cached", "--others", "--exclude-standard"]);
		assert!(matches!(&err.cause, Cause::Exit { stderr, .. } if stderr.contains("not a git repository")), "{err}");
		return Ok(());
	}

	// A helper that cannot start git returns why, so the calling test fails with that text instead of a panic.
	#[test]
	fn git_in_a_missing_dir_returns_the_spawn_error() -> Result<(), String> {
		let repo = new_repo()?;
		let gone: PathBuf = repo.path().join("gone");
		let ran = git(&gone, &["status"]);
		assert!(matches!(&ran, Err(text) if text.starts_with("run: Command::new")), "{ran:?}");
		let failed = git_fails(&gone, &["status"]);
		assert!(matches!(&failed, Err(text) if text.starts_with("run: Command::new")), "{failed:?}");
		return Ok(());
	}

	// git echoes a config value byte for byte, so a non-UTF-8 value is an Err, not a lossy string.
	#[test]
	fn git_with_non_utf8_stdout_returns_an_err() -> Result<(), String> {
		let repo = new_repo()?;
		let mut config = OpenOptions::new().append(true).open(repo.path().join(".git/config")).map_err(|e| format!("open config: {e}"))?;
		config.write_all(b"[probe]\n\tvalue = caf\xe9\n").map_err(|e| format!("append config: {e}"))?;
		drop(config);
		let got = git(repo.path(), &["config", "probe.value"]);
		assert!(matches!(&got, Err(text) if text.starts_with("String::from_utf8(out.stdout)")), "{got:?}");
		return Ok(());
	}

	#[test]
	fn same_dir_sees_through_dot_dot_and_errs_on_a_missing_side() -> Result<(), String> {
		let repo = new_repo()?;
		let root: &Path = repo.path();
		assert!(same_dir(&root.join(".git").join(".."), root)?);
		assert!(!same_dir(&root.join(".git"), root)?);
		assert!(same_dir(&root.join("gone"), root).is_err());
		assert!(same_dir(root, &root.join("gone")).is_err());
		return Ok(());
	}

	#[test]
	fn is_oid_takes_only_40_lowercase_hex() {
		let oid: &str = "0123456789abcdef0123456789abcdef01234567";
		assert!(is_oid(oid));
		assert!(!is_oid(&oid.to_uppercase()) && !is_oid(&oid[1..]) && !is_oid(&format!("{oid}0")));
	}
}
