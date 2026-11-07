use std::io::{Error as IoError, ErrorKind};
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};

use arrayvec::ArrayString;

use crate::config::{self, RepoConfig, CONFIG_FILE};
use crate::discover::{locate, no_head, HEAD_FILE};
use crate::error::{Miss, MissReason};
use crate::lines::scan_text_lines;
use crate::object_format::ObjectFormat;
use crate::small_file::{read_small, SmallBuf, SMALL_FILE_MAX};

const SYMREF_PREFIX: &str = "ref: ";
const BRANCH_PREFIX: &str = "refs/heads/";
const COMMONDIR_FILE: &str = "commondir";
const PACKED_REFS_FILE: &str = "packed-refs";
const LOCK_SUFFIX: &str = ".lock";
/// Hex digits in a SHA-256 object id, the longest oid git prints.
pub const OID_HEX_MAX: usize = ObjectFormat::Sha256.hex_len();

/// A commit id in hex, held inline: 40 digits, or 64 in a SHA-256 repo.
pub type OidHex = ArrayString<OID_HEX_MAX>;

/// HEAD's commit id from `HEAD`, the loose branch ref or `packed-refs`, without spawning `git`: 40 hex, or 64
/// when the config selects SHA-256.
///
/// # Errors
/// A [`Miss`] whenever the files cannot answer for sure; [`crate::head`] then asks git.
pub fn read_head(dir: &Path) -> Result<OidHex, Miss> {
	let git_dir: PathBuf = locate(dir)?.git_dir;
	// The read of HEAD also proves that HEAD exists, so discovery skips its own check.
	let head_path: PathBuf = git_dir.join(HEAD_FILE);
	let mut head_buf: SmallBuf = [0; SMALL_FILE_MAX];
	let head: &str =
		read_small(&head_path, &mut head_buf).map(str::trim_end).map_err(|err: IoError| return no_head(&head_path, &err))?;
	let common: PathBuf = common_dir(&git_dir)?;
	let format: ObjectFormat = files_backend(&common)?.object_format;
	let Some(refname) = head.strip_prefix(SYMREF_PREFIX) else {
		return oid(&head_path, head, format);
	};
	if !is_plain_branch(refname) {
		return Err(Miss::new(&head_path, MissReason::UnsupportedRef));
	}
	return branch_oid(&common, refname, format);
}

// A linked worktree's `commondir` names the shared repo, relative to its own git dir.
pub fn common_dir(git_dir: &Path) -> Result<PathBuf, Miss> {
	let path: PathBuf = git_dir.join(COMMONDIR_FILE);
	let mut buf: SmallBuf = [0; SMALL_FILE_MAX];
	return match read_small(&path, &mut buf).map(str::trim_end) {
		Ok(text) if !text.is_empty() => Ok(git_dir.join(text)),
		Ok(_) => Err(Miss::new(&path, MissReason::Malformed)),
		Err(err) if err.kind() == ErrorKind::NotFound => Ok(git_dir.to_path_buf()),
		Err(err) => Err(Miss::io(&path, &err)),
	};
}

// The repo's config, unless refs may live outside the files backend. git reads a reftable only when
// `extensions.refStorage` says so, so a stray `reftable/` changes nothing.
fn files_backend(common: &Path) -> Result<RepoConfig, Miss> {
	let path: PathBuf = common.join(CONFIG_FILE);
	let config: RepoConfig = config::read(&path)?.ok_or_else(|| return Miss::new(&path, MissReason::Io(ErrorKind::NotFound)))?;
	if config.ref_storage {
		return Err(Miss::new(&path, MissReason::Reftable));
	}
	return Ok(config);
}

fn branch_oid(common: &Path, refname: &str, format: ObjectFormat) -> Result<OidHex, Miss> {
	let loose: PathBuf = common.join(refname);
	let mut buf: SmallBuf = [0; SMALL_FILE_MAX];
	return match read_small(&loose, &mut buf) {
		Ok(text) if text.starts_with(SYMREF_PREFIX) => Err(Miss::new(&loose, MissReason::SymbolicRef)),
		Ok(text) => oid(&loose, text.trim_end(), format),
		Err(err) if err.kind() == ErrorKind::NotFound => packed_oid(common, refname, &loose, format),
		Err(err) => Err(Miss::io(&loose, &err)),
	};
}

fn packed_oid(common: &Path, refname: &str, loose: &Path, format: ObjectFormat) -> Result<OidHex, Miss> {
	let path: PathBuf = common.join(PACKED_REFS_FILE);
	let found = scan_text_lines(&path, |line: &str| {
		return packed_entry(line, refname).map_or(ControlFlow::Continue(()), |hex: &str| return ControlFlow::Break(oid(&path, hex, format)));
	});
	return match found {
		Ok(Some(oid)) => oid,
		Ok(None) => Err(Miss::new(loose, MissReason::Unborn)),
		Err(err) if err.kind() == ErrorKind::NotFound => Err(Miss::new(loose, MissReason::Unborn)),
		Err(err) => Err(Miss::io(&path, &err)),
	};
}

// packed-refs: `#` header, `<oid> <refname>` entries, `^<oid>` peeled tags under the entry they peel.
fn packed_entry<'a>(line: &'a str, refname: &str) -> Option<&'a str> {
	if line.starts_with(['#', '^']) {
		return None;
	}
	return line.split_once(' ').filter(|&(_, name)| return name == refname).map(|(hex, _)| return hex);
}

fn oid(path: &Path, text: &str, format: ObjectFormat) -> Result<OidHex, Miss> {
	if text.len() == format.hex_len() && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
		// Every format's width fits; the map_err only satisfies the fallible constructor.
		return OidHex::from(text).map_err(|_| Miss::new(path, MissReason::Malformed));
	}
	return Err(Miss::new(path, MissReason::Malformed));
}

// Deliberately narrower than git's refname rules: anything unusual goes to git rather than to a guessed path.
fn is_plain_branch(refname: &str) -> bool {
	let Some(name) = refname.strip_prefix(BRANCH_PREFIX) else {
		return false;
	};
	let is_safe_char = |c: char| -> bool { return c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+') };
	return name.split('/').all(|part| {
		return !part.is_empty()
			&& !part.starts_with('.')
			&& !part.ends_with('.')
			&& !part.ends_with(LOCK_SUFFIX)
			&& part.chars().all(is_safe_char);
	});
}

#[cfg(test)]
mod tests {
	use std::fs;
	use std::io::ErrorKind;
	use std::path::{Path, PathBuf};

	use super::{common_dir, is_plain_branch, oid, packed_entry, read_head};
	use crate::error::MissReason;
	use crate::object_format::ObjectFormat;
	use crate::testrepo::{commit, expect_err, git, new_repo, rev, TestResult};

	const A: &str = "0123456789abcdef0123456789abcdef01234567";

	#[test]
	fn packed_entry_skips_header_and_peeled_lines() {
		let entry: String = format!("{A} refs/heads/main");
		assert_eq!(packed_entry(&entry, "refs/heads/main"), Some(A));
		assert_eq!(packed_entry(&entry, "refs/heads/other"), None);
		assert_eq!(packed_entry(&format!("^{A} refs/heads/main"), "refs/heads/main"), None);
		assert_eq!(packed_entry("# pack-refs with: refs/heads/main", "with:"), None, "the header is no entry");
	}

	// With no loose ref left, the oid comes from the packed-refs that `git pack-refs` writes.
	#[test]
	fn a_packed_branch_reads_its_oid() -> TestResult {
		let repo = new_repo()?;
		commit(repo.path(), "one")?;
		git(repo.path(), &["pack-refs", "--all"])?;
		let heads: usize = fs::read_dir(repo.path().join(".git/refs/heads")).map_err(|err| format!("{err:?}"))?.count();
		assert_eq!(heads, 0, "pack-refs pruned the loose branch");
		let got = read_head(repo.path()).map_err(|err| format!("read_head: {err:?}"))?;
		assert_eq!(got.as_str(), rev(repo.path(), "HEAD")?.as_str());
		return Ok(());
	}

	// A packed-refs that is not UTF-8 anywhere is an I/O miss, as `read_to_string` made it.
	#[test]
	fn non_utf8_packed_refs_is_an_io_miss() -> TestResult {
		let repo = new_repo()?;
		commit(repo.path(), "one")?;
		git(repo.path(), &["pack-refs", "--all"])?;
		let packed: PathBuf = repo.path().join(".git/packed-refs");
		let mut bytes: Vec<u8> = fs::read(&packed).map_err(|err| format!("read packed-refs: {err:?}"))?;
		bytes.extend_from_slice(b"0000000000000000000000000000000000000000 refs/tags/caf\xe9\n");
		fs::write(&packed, bytes).map_err(|err| format!("write packed-refs: {err:?}"))?;
		let miss = read_head(repo.path()).err();
		assert_eq!(miss.map(|m| return m.reason), Some(MissReason::Io(ErrorKind::InvalidData)));
		return Ok(());
	}

	#[test]
	fn oid_accepts_only_40_lowercase_hex() -> TestResult {
		let path: &Path = Path::new("x");
		let got_oid = oid(path, A, ObjectFormat::Sha1).map_err(|err| format!("oid(path, A): {err:?}"))?;
		assert_eq!(got_oid.as_str(), A);
		let got_oid_2 = expect_err(oid(path, &A.to_uppercase(), ObjectFormat::Sha1), "oid(path, &A.to_uppercase())")?;
		assert_eq!(got_oid_2.reason, MissReason::Malformed);
		let got_oid_3 = expect_err(oid(path, &A[1..], ObjectFormat::Sha1), "oid(path, &A[1..])")?;
		assert_eq!(got_oid_3.reason, MissReason::Malformed);
		let got_oid_4 = expect_err(oid(path, &format!("{A}{A}")[..64], ObjectFormat::Sha1), "oid(path, 64 digits)")?;
		assert_eq!(got_oid_4.reason, MissReason::Malformed);
		return Ok(());
	}

	// The width comes from the repo's object format: SHA-256 takes 64 digits and refuses 40.
	#[test]
	fn a_sha256_oid_is_64_lowercase_hex() -> TestResult {
		let path: &Path = Path::new("x");
		let mut long: String = format!("{A}{A}");
		long.truncate(64);
		let got = oid(path, &long, ObjectFormat::Sha256).map_err(|err| format!("oid(path, 64 digits): {err:?}"))?;
		assert_eq!(got.as_str(), long);
		assert_eq!(expect_err(oid(path, A, ObjectFormat::Sha256), "oid(path, A)")?.reason, MissReason::Malformed);
		return Ok(());
	}

	#[test]
	fn plain_branch_names_only() {
		assert!(is_plain_branch("refs/heads/main"));
		assert!(is_plain_branch("refs/heads/feat/x-1.2_y+z"));
		let rejected: [&str; 10] = [
			"refs/tags/v1",
			"refs/heads/",
			"refs/heads/a//b",
			"refs/heads/../x",
			"refs/heads/.hidden",
			"refs/heads/a.lock",
			"refs/heads/a.",
			"refs/heads/sp ace",
			"refs/heads/a\\b",
			"main",
		];
		rejected.iter().for_each(|name| assert!(!is_plain_branch(name), "{name}"));
	}

	#[test]
	fn empty_commondir_is_malformed() -> TestResult {
		let repo = new_repo()?;
		let git_dir: PathBuf = repo.path().join(".git");
		fs::write(git_dir.join("commondir"), "\n").map_err(|err| format!("write commondir: {err:?}"))?;
		let got_common_dir = expect_err(common_dir(&git_dir), "common_dir(&git_dir)")?;
		assert_eq!(got_common_dir.reason, MissReason::Malformed);
		return Ok(());
	}

	#[test]
	fn head_on_a_non_branch_ref_is_unsupported() -> TestResult {
		let repo = new_repo()?;
		commit(repo.path(), "one")?;
		git(repo.path(), &["symbolic-ref", "HEAD", "refs/remotes/origin/main"])?;
		let miss = expect_err(read_head(repo.path()), "read_head(repo.path())")?;
		assert_eq!(miss.reason, MissReason::UnsupportedRef);
		assert!(miss.path.ends_with("HEAD"));
		return Ok(());
	}

	#[test]
	fn packed_refs_without_the_branch_is_unborn() -> TestResult {
		let repo = new_repo()?;
		commit(repo.path(), "one")?;
		git(repo.path(), &["pack-refs", "--all"])?;
		git(repo.path(), &["symbolic-ref", "HEAD", "refs/heads/fresh"])?;
		let got_read_head = expect_err(read_head(repo.path()), "read_head(repo.path())")?;
		assert_eq!(got_read_head.reason, MissReason::Unborn);
		return Ok(());
	}

	fn is_io_but_not_absent(reason: MissReason) -> bool {
		return matches!(reason, MissReason::Io(kind) if kind != ErrorKind::NotFound);
	}

	// A `commondir` that does not read as a file is an I/O miss, not a repo without one.
	#[test]
	fn a_commondir_directory_is_an_io_miss() -> TestResult {
		let repo = new_repo()?;
		let git_dir: PathBuf = repo.path().join(".git");
		fs::create_dir(git_dir.join("commondir")).map_err(|err| format!("mkdir commondir: {err:?}"))?;
		let miss = common_dir(&git_dir).err();
		assert!(miss.as_ref().is_some_and(|m| m.path.ends_with("commondir") && is_io_but_not_absent(m.reason)), "{miss:?}");
		return Ok(());
	}

	// An unreadable `packed-refs` must not pass for an unborn branch.
	#[test]
	fn a_packed_refs_directory_is_an_io_miss_not_unborn() -> TestResult {
		let repo = new_repo()?;
		fs::create_dir(repo.path().join(".git/packed-refs")).map_err(|err| format!("mkdir packed-refs: {err:?}"))?;
		let miss = read_head(repo.path()).err();
		assert!(miss.as_ref().is_some_and(|m| m.path.ends_with("packed-refs") && is_io_but_not_absent(m.reason)), "{miss:?}");
		return Ok(());
	}

	#[test]
	fn missing_config_is_an_io_miss() -> TestResult {
		let repo = new_repo()?;
		commit(repo.path(), "one")?;
		fs::remove_file(repo.path().join(".git/config")).map_err(|err| format!("rm .git/config: {err:?}"))?;
		let got_read_head_2 = expect_err(read_head(repo.path()), "read_head(repo.path())")?;
		assert_eq!(got_read_head_2.reason, MissReason::Io(ErrorKind::NotFound));
		return Ok(());
	}
}
