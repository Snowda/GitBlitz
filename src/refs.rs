use std::fs;
use std::io::{Error as IoError, ErrorKind};
use std::path::{Path, PathBuf};

use arrayvec::ArrayString;

use crate::discover::{locate, no_head, HEAD_FILE};
use crate::error::{Miss, MissReason};
use crate::object_format::ObjectFormat;
use crate::small_file::{read_small, SmallBuf, SMALL_FILE_MAX};

const SYMREF_PREFIX: &str = "ref: ";
const BRANCH_PREFIX: &str = "refs/heads/";
const COMMONDIR_FILE: &str = "commondir";
const PACKED_REFS_FILE: &str = "packed-refs";
pub const CONFIG_FILE: &str = "config";
const REF_STORAGE_KEY: &str = "refstorage";
const LOCK_SUFFIX: &str = ".lock";
/// Hex digits in a SHA-256 object id, the longest oid git prints.
pub const OID_HEX_MAX: usize = ObjectFormat::Sha256.hex_len();

/// A commit id in hex, held inline: 40 digits from the file reader, up to 64 from git for SHA-256 repos.
pub type OidHex = ArrayString<OID_HEX_MAX>;

/// HEAD's 40-hex commit id from `HEAD`, the loose branch ref or `packed-refs`, without spawning `git`.
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
	reject_reftable(&common)?;
	let Some(refname) = head.strip_prefix(SYMREF_PREFIX) else {
		return oid(&head_path, head);
	};
	if !is_plain_branch(refname) {
		return Err(Miss::new(&head_path, MissReason::UnsupportedRef));
	}
	return branch_oid(&common, refname);
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

// git reads refs from a reftable only when `extensions.refStorage` says so; a stray `reftable/` changes nothing.
fn reject_reftable(common: &Path) -> Result<(), Miss> {
	let config: PathBuf = common.join(CONFIG_FILE);
	let text: Vec<u8> = fs::read(&config).map_err(|err| Miss::io(&config, &err))?;
	if mentions(&text, REF_STORAGE_KEY) {
		return Err(Miss::new(&config, MissReason::Reftable));
	}
	return Ok(());
}

// Whether `text` holds `key` in any ASCII case; `key` is lowercase.
fn mentions(text: &[u8], key: &str) -> bool {
	return text.windows(key.len()).any(|window: &[u8]| return window.eq_ignore_ascii_case(key.as_bytes()));
}

fn branch_oid(common: &Path, refname: &str) -> Result<OidHex, Miss> {
	let loose: PathBuf = common.join(refname);
	let mut buf: SmallBuf = [0; SMALL_FILE_MAX];
	return match read_small(&loose, &mut buf) {
		Ok(text) if text.starts_with(SYMREF_PREFIX) => Err(Miss::new(&loose, MissReason::SymbolicRef)),
		Ok(text) => oid(&loose, text.trim_end()),
		Err(err) if err.kind() == ErrorKind::NotFound => packed_oid(common, refname, &loose),
		Err(err) => Err(Miss::io(&loose, &err)),
	};
}

fn packed_oid(common: &Path, refname: &str, loose: &Path) -> Result<OidHex, Miss> {
	let path: PathBuf = common.join(PACKED_REFS_FILE);
	let text: String = match fs::read_to_string(&path) {
		Ok(text) => text,
		Err(err) if err.kind() == ErrorKind::NotFound => return Err(Miss::new(loose, MissReason::Unborn)),
		Err(err) => return Err(Miss::io(&path, &err)),
	};
	return match packed_entry(&text, refname) {
		Some(hex) => oid(&path, hex),
		None => Err(Miss::new(loose, MissReason::Unborn)),
	};
}

// packed-refs: `#` header, `<oid> <refname>` entries, `^<oid>` peeled tags under the entry they peel.
fn packed_entry<'a>(text: &'a str, refname: &str) -> Option<&'a str> {
	return text
		.lines()
		.filter(|line| !line.starts_with('#') && !line.starts_with('^'))
		.find_map(|line| line.split_once(' ').filter(|&(_, name)| name == refname).map(|(hex, _)| hex));
}

fn oid(path: &Path, text: &str) -> Result<OidHex, Miss> {
	if text.len() == ObjectFormat::Sha1.hex_len() && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
		// 40 digits always fit; the map_err only satisfies the fallible constructor.
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
	use crate::testrepo::{commit, expect_err, git, new_repo};

	const A: &str = "0123456789abcdef0123456789abcdef01234567";

	#[test]
	fn packed_entry_skips_header_and_peeled_lines() {
		let text: String = format!("# pack-refs with: peeled fully-peeled sorted\n^{A}\n{A} refs/heads/main\n");
		assert_eq!(packed_entry(&text, "refs/heads/main"), Some(A));
		assert_eq!(packed_entry(&text, "refs/heads/other"), None);
		assert_eq!(packed_entry(&format!("^{A} refs/heads/main\n"), "refs/heads/main"), None);
	}

	#[test]
	fn oid_accepts_only_40_lowercase_hex() -> Result<(), String> {
		let path: &Path = Path::new("x");
		let got_oid = oid(path, A).map_err(|err| format!("oid(path, A): {err:?}"))?;
		assert_eq!(got_oid.as_str(), A);
		let got_oid_2 = expect_err(oid(path, &A.to_uppercase()), "oid(path, &A.to_uppercase())")?;
		assert_eq!(got_oid_2.reason, MissReason::Malformed);
		let got_oid_3 = expect_err(oid(path, &A[1..]), "oid(path, &A[1..])")?;
		assert_eq!(got_oid_3.reason, MissReason::Malformed);
		let got_oid_4 = expect_err(oid(path, &format!("{A}{A}")[..64]), "oid(path, 64 digits)")?;
		assert_eq!(got_oid_4.reason, MissReason::Malformed);
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
	fn empty_commondir_is_malformed() -> Result<(), String> {
		let repo = new_repo()?;
		let git_dir: PathBuf = repo.path().join(".git");
		fs::write(git_dir.join("commondir"), "\n").map_err(|err| format!("write commondir: {err:?}"))?;
		let got_common_dir = expect_err(common_dir(&git_dir), "common_dir(&git_dir)")?;
		assert_eq!(got_common_dir.reason, MissReason::Malformed);
		return Ok(());
	}

	#[test]
	fn head_on_a_non_branch_ref_is_unsupported() -> Result<(), String> {
		let repo = new_repo()?;
		commit(repo.path(), "one")?;
		git(repo.path(), &["symbolic-ref", "HEAD", "refs/remotes/origin/main"])?;
		let miss = expect_err(read_head(repo.path()), "read_head(repo.path())")?;
		assert_eq!(miss.reason, MissReason::UnsupportedRef);
		assert!(miss.path.ends_with("HEAD"));
		return Ok(());
	}

	#[test]
	fn packed_refs_without_the_branch_is_unborn() -> Result<(), String> {
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
	fn a_commondir_directory_is_an_io_miss() -> Result<(), String> {
		let repo = new_repo()?;
		let git_dir: PathBuf = repo.path().join(".git");
		fs::create_dir(git_dir.join("commondir")).map_err(|err| format!("mkdir commondir: {err:?}"))?;
		let miss = common_dir(&git_dir).err();
		assert!(miss.as_ref().is_some_and(|m| m.path.ends_with("commondir") && is_io_but_not_absent(m.reason)), "{miss:?}");
		return Ok(());
	}

	// An unreadable `packed-refs` must not pass for an unborn branch.
	#[test]
	fn a_packed_refs_directory_is_an_io_miss_not_unborn() -> Result<(), String> {
		let repo = new_repo()?;
		fs::create_dir(repo.path().join(".git/packed-refs")).map_err(|err| format!("mkdir packed-refs: {err:?}"))?;
		let miss = read_head(repo.path()).err();
		assert!(miss.as_ref().is_some_and(|m| m.path.ends_with("packed-refs") && is_io_but_not_absent(m.reason)), "{miss:?}");
		return Ok(());
	}

	#[test]
	fn missing_config_is_an_io_miss() -> Result<(), String> {
		let repo = new_repo()?;
		commit(repo.path(), "one")?;
		fs::remove_file(repo.path().join(".git/config")).map_err(|err| format!("rm .git/config: {err:?}"))?;
		let got_read_head_2 = expect_err(read_head(repo.path()), "read_head(repo.path())")?;
		assert_eq!(got_read_head_2.reason, MissReason::Io(ErrorKind::NotFound));
		return Ok(());
	}
}
