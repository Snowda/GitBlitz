//! Fuzz harnesses for GitBlitz's hostile-file boundaries: `.git/index`, and the HEAD, ref, packed-refs and config
//! files. Each writes its input into a scratch repo, calls the public reader there, and asserts an invariant of
//! git's formats. The libFuzzer targets (fuzz_targets/) and the stable replay test (src/replay.rs) both call them.

mod dirc;
#[cfg(test)]
mod replay;

use std::fs;
use std::path::PathBuf;

use dirc::Layout;
use gitblitz::{read_files, read_head, Miss, OidHex};
use tempfile::TempDir;

const DOT_GIT: &str = ".git";
const HEAD_FILE: &str = "HEAD";
const INDEX_FILE: &str = "index";
const PACKED_REFS_FILE: &str = "packed-refs";
const CONFIG_FILE: &str = "config";
const MAIN_BRANCH: &str = "refs/heads/main";
const HEAD_ON_MAIN: &[u8] = b"ref: refs/heads/main\n";
const SYMREF_PREFIX: &str = "ref: ";
const REF_STORAGE_KEY: &str = "refstorage";
const INDEX_SIGNATURE: &[u8] = b"DIRC";
const INDEX_HEADER_LEN: usize = 12;
const INDEX_CHECKSUM_LEN: usize = 20;
const INDEX_MIN_VERSION: u32 = 2;
const INDEX_MAX_VERSION: u32 = 4;
const INDEX_VERSIONS: u8 = 3;
const OID_HEX_LEN: usize = 40;
// 0xFF never occurs in UTF-8, so it splits the head input into texts and puts no limit on what they hold.
const FIELD_SEP: u8 = 0xFF;
const HAS_LOOSE: u8 = 1;
const HAS_PACKED: u8 = 2;
const HAS_CONFIG: u8 = 4;
const IS_EXTENDED: u8 = 1;
const HAS_TREE: u8 = 2;

/// Raw bytes as `.git/index`.
///
/// An answer needs a DIRC header of version 2–4 and room for the checksum. It holds at most the header's entry
/// count of paths (the work tree has no other file), sorted and without duplicates.
pub fn index(data: &[u8]) {
	let Some(repo) = scratch_repo(&[(HEAD_FILE, HEAD_ON_MAIN), (INDEX_FILE, data)]) else {
		return;
	};
	let Ok(files) = read_files(repo.path()) else {
		return;
	};
	assert!(data.starts_with(INDEX_SIGNATURE), "a file without the DIRC signature listed {files:?}");
	let version: Option<u32> = be_u32(data, 4);
	let is_known: bool = version.is_some_and(|v| (INDEX_MIN_VERSION..=INDEX_MAX_VERSION).contains(&v));
	assert!(is_known, "index version {version:?} listed");
	let count: Option<usize> = be_u32(data, 8).and_then(|c| usize::try_from(c).ok());
	assert!(count.is_some_and(|c| files.len() <= c), "{} paths from an index of {count:?} entries", files.len());
	assert!(data.len() >= INDEX_HEADER_LEN + INDEX_CHECKSUM_LEN, "an index too short for its checksum listed");
	assert!(files.windows(2).all(|w| w[0] < w[1]), "the listing is not sorted and deduplicated: {files:?}");
	assert_eq!(read_files(repo.path()).ok(), Some(files), "a second read of the same index differs");
}

/// Names written as a well-formed index read back exactly.
///
/// Byte 0 picks index version 2, 3 or 4. Byte 1 sets the extended flag and a TREE extension. The rest is the
/// entry names, split at NUL. `read_files` lists exactly those names, sorted and without duplicates.
pub fn index_roundtrip(data: &[u8]) {
	let Some(([pick, options], rest)) = data.split_first_chunk::<2>() else {
		return;
	};
	let version: u32 = INDEX_MIN_VERSION + u32::from(pick % INDEX_VERSIONS);
	let names: Vec<String> = if rest.is_empty() {
		Vec::new()
	} else {
		rest.split(|b| *b == 0).map(|name| String::from_utf8_lossy(name).into_owned()).collect()
	};
	let layout = Layout {
		version,
		names: &names,
		is_extended: version > INDEX_MIN_VERSION && options & IS_EXTENDED != 0,
		has_tree: options & HAS_TREE != 0,
	};
	let Some(repo) = scratch_repo(&[(HEAD_FILE, HEAD_ON_MAIN), (INDEX_FILE, &dirc::write(&layout))]) else {
		return;
	};
	let listed: Result<Vec<PathBuf>, Miss> = read_files(repo.path());
	let mut expected: Vec<PathBuf> = names.iter().map(PathBuf::from).collect();
	expected.sort();
	expected.dedup();
	assert_eq!(listed.as_ref().ok(), Some(&expected), "v{version} index of {names:?} read back as {listed:?}");
}

/// HEAD resolves to 40 lowercase hex from the file git reads, or not at all.
///
/// Byte 0 says which of the loose `refs/heads/main`, `packed-refs` and `config` exist; the rest is HEAD, then
/// their texts, split at 0xFF. An answer is 40 lowercase hex from the file git reads: HEAD itself when detached,
/// else the loose ref, else the branch's `packed-refs` line. It needs a config that does not name `refStorage`.
pub fn head(data: &[u8]) {
	let Some((&mask, rest)) = data.split_first() else {
		return;
	};
	let mut fields = rest.split(|b| *b == FIELD_SEP);
	let head_text: &[u8] = fields.next().unwrap_or_default();
	let loose: Option<&[u8]> = fields.next().filter(|_| mask & HAS_LOOSE != 0);
	let packed: Option<&[u8]> = fields.next().filter(|_| mask & HAS_PACKED != 0);
	let config: Option<&[u8]> = fields.next().filter(|_| mask & HAS_CONFIG != 0);
	let mut files: Vec<(&str, &[u8])> = vec![(HEAD_FILE, head_text)];
	for (rel, text) in [(MAIN_BRANCH, loose), (PACKED_REFS_FILE, packed), (CONFIG_FILE, config)] {
		if let Some(text) = text {
			files.push((rel, text));
		}
	}
	let Some(repo) = scratch_repo(&files) else {
		return;
	};
	let Ok(oid) = read_head(repo.path()) else {
		return;
	};
	let oid: &str = OidHex::as_str(&oid);
	assert!(oid.len() == OID_HEX_LEN && oid.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')), "{oid:?}");
	assert!(config.is_some(), "HEAD resolved to {oid} without a config file");
	let config_text: String = text(config.unwrap_or_default()).to_ascii_lowercase();
	assert!(!config_text.contains(REF_STORAGE_KEY), "a refStorage config still answered {oid}");
	let head: String = text(head_text);
	let Some(refname) = head.trim_end().strip_prefix(SYMREF_PREFIX) else {
		assert_eq!(oid, head.trim_end(), "a detached HEAD is its own answer");
		return;
	};
	let loose_text: Option<String> = loose.map(text);
	// A case-insensitive file system (Windows) opens the loose `main` for any case of the name, as git does.
	let is_loose: bool =
		refname.eq_ignore_ascii_case(MAIN_BRANCH) && loose_text.as_deref().is_some_and(|t| t.trim_end() == oid);
	if refname == MAIN_BRANCH && loose.is_some() {
		assert!(is_loose, "{oid} is not the loose ref {loose_text:?}, which wins over packed-refs");
		return;
	}
	let line: String = format!("{oid} {refname}");
	let is_packed: bool = text(packed.unwrap_or_default()).lines().any(|l| l == line);
	assert!(is_packed || is_loose, "{oid} for {refname} is in no packed-refs line");
}

// A scratch work tree whose `.git` holds `files`. A write that fails is a harness fault, so it asserts.
fn scratch_repo(files: &[(&str, &[u8])]) -> Option<TempDir> {
	let made: Result<TempDir, String> = write_repo(files);
	assert!(made.is_ok(), "scratch repo: {:?}", made.as_ref().err());
	return made.ok();
}

fn write_repo(files: &[(&str, &[u8])]) -> Result<TempDir, String> {
	let repo: TempDir = tempfile::tempdir().map_err(|e| format!("tempdir: {e}"))?;
	let git_dir: PathBuf = repo.path().join(DOT_GIT);
	for (rel, bytes) in files {
		let path: PathBuf = git_dir.join(rel);
		if let Some(parent) = path.parent() {
			fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
		}
		fs::write(&path, bytes).map_err(|e| format!("write {}: {e}", path.display()))?;
	}
	return Ok(repo);
}

// The reader accepts only UTF-8, so wherever it answered, the lossy text is the exact text.
fn text(bytes: &[u8]) -> String {
	return String::from_utf8_lossy(bytes).into_owned();
}

fn be_u32(bytes: &[u8], pos: usize) -> Option<u32> {
	let word: [u8; 4] = bytes.get(pos..pos + 4)?.try_into().ok()?;
	return Some(u32::from_be_bytes(word));
}
