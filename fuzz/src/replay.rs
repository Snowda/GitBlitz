//! Stable replay: every seed and regression input runs through its harness on each plain test run. A unit-test
//! module, not tests/, so `--lib` coverage runs it and cargo never builds the fuzz_targets bins for it.

use std::fs;
use std::path::{Path, PathBuf};

use super::{check_pack, head, index, index_roundtrip, pack, TestResult};

// The fuzz crate's own root, which holds seeds/ and regressions/.
fn fuzz_root() -> PathBuf {
	return Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf();
}

// Run `harness` over each non-hidden file in seeds/<target>/ and regressions/<target>/ under `root`. Returns the count.
fn replay(root: &Path, target: &str, harness: fn(&[u8])) -> TestResult<usize> {
	let mut ran: usize = 0;
	for dir in ["seeds", "regressions"] {
		let Ok(entries) = fs::read_dir(root.join(dir).join(target)) else {
			continue;
		};
		for entry in entries.flatten() {
			let is_hidden: bool = entry.file_name().to_string_lossy().starts_with('.');
			if is_hidden || !entry.path().is_file() {
				continue;
			}
			let bytes: Vec<u8> = fs::read(entry.path()).map_err(|e| format!("read {}: {e}", entry.path().display()))?;
			harness(&bytes);
			ran += 1;
		}
	}
	return Ok(ran);
}

// Replay one target and fail if its seed directory held nothing to run.
fn replay_nonempty(root: &Path, target: &str, harness: fn(&[u8])) -> TestResult {
	return match replay(root, target, harness) {
		Ok(0) => Err(format!("fuzz/seeds/{target}/ holds no seed inputs")),
		Ok(_) => Ok(()),
		Err(e) => Err(e),
	};
}

#[test]
fn fuzz_replay_index() -> TestResult {
	return replay_nonempty(&fuzz_root(), "index", index);
}

#[test]
fn fuzz_replay_index_roundtrip() -> TestResult {
	return replay_nonempty(&fuzz_root(), "index_roundtrip", index_roundtrip);
}

#[test]
fn fuzz_replay_head() -> TestResult {
	return replay_nonempty(&fuzz_root(), "head", head);
}

#[test]
fn fuzz_replay_pack() -> TestResult {
	return replay_nonempty(&fuzz_root(), "pack", pack);
}

// The pack seeds are git's own REF and OFS delta packs: all but the one absent id must read whole.
#[test]
fn pack_seeds_read_whole_objects() -> TestResult {
	const ABSENT_SEEDS: usize = 1;
	let dir: PathBuf = fuzz_root().join("seeds").join("pack");
	let mut answered: usize = 0;
	let mut total: usize = 0;
	for entry in fs::read_dir(&dir).map_err(|e| format!("read {}: {e}", dir.display()))?.flatten() {
		let bytes: Vec<u8> = fs::read(entry.path()).map_err(|e| format!("read {}: {e}", entry.path().display()))?;
		answered += usize::from(check_pack(&bytes));
		total += 1;
	}
	assert_eq!(answered + ABSENT_SEEDS, total, "{answered} of {total} pack seeds answered");
	return Ok(());
}

mod tests {
	use std::fs;
	use std::io::Result as IoResult;
	use std::path::{Path, PathBuf};

	use tempfile::{tempdir, TempDir};

	use super::{replay, replay_nonempty, TestResult};

	const SKIPPED: &[u8] = b"skipped";

	fn refuse_skipped(bytes: &[u8]) {
		assert_ne!(bytes, SKIPPED, "a hidden file reached the harness");
	}

	fn write(dir: &Path, name: &str, bytes: &[u8]) -> IoResult<PathBuf> {
		fs::create_dir_all(dir)?;
		let path: PathBuf = dir.join(name);
		fs::write(&path, bytes)?;
		return Ok(path);
	}

	#[test]
	fn a_target_with_no_inputs_is_an_error_naming_it() -> IoResult<()> {
		let root: TempDir = tempdir()?;
		assert_eq!(replay(root.path(), "absent", refuse_skipped), Ok(0));
		let got: TestResult = replay_nonempty(root.path(), "absent", refuse_skipped);
		assert_eq!(got, Err(String::from("fuzz/seeds/absent/ holds no seed inputs")));
		return Ok(());
	}

	#[test]
	fn runs_seeds_and_regressions_but_skips_hidden_files_and_subdirs() -> IoResult<()> {
		let root: TempDir = tempdir()?;
		let seeds: PathBuf = root.path().join("seeds/t");
		write(&seeds, "a", b"a")?;
		write(&seeds, ".gitkeep", SKIPPED)?;
		write(&seeds.join("sub"), "inner", SKIPPED)?;
		write(&root.path().join("regressions/t"), "b", b"b")?;
		assert_eq!(replay(root.path(), "t", refuse_skipped), Ok(2));
		assert_eq!(replay_nonempty(root.path(), "t", refuse_skipped), Ok(()));
		return Ok(());
	}

	// A regression input alone is enough; the seeds/ directory may be absent.
	#[test]
	fn regressions_alone_count_as_inputs() -> IoResult<()> {
		let root: TempDir = tempdir()?;
		write(&root.path().join("regressions/t"), "only", b"x")?;
		assert_eq!(replay(root.path(), "t", refuse_skipped), Ok(1));
		return Ok(());
	}

	// Another handle that shares nothing makes the read fail; the error names the file and stops the replay.
	// Unix has no mandatory lock, so this read failure is tested on Windows only.
	#[cfg(windows)]
	#[test]
	fn an_unreadable_input_is_an_error_naming_it() -> IoResult<()> {
		use std::fs::{File, OpenOptions};
		use std::os::windows::fs::OpenOptionsExt;

		let root: TempDir = tempdir()?;
		let locked: PathBuf = write(&root.path().join("seeds/t"), "locked", b"x")?;
		let hold: File = OpenOptions::new().read(true).share_mode(0).open(&locked)?;
		let got: TestResult = replay_nonempty(root.path(), "t", refuse_skipped);
		drop(hold);
		assert!(matches!(&got, Err(text) if text.starts_with("read ") && text.contains("locked")), "{got:?}");
		return Ok(());
	}
}
