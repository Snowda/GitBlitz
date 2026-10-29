// Real-git fixtures: every assertion compares the reader against `git` itself, the oracle.

use std::fmt::Debug;
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use tempfile::TempDir;

use crate::isolating_config::ISOLATING_CONFIG;
use crate::object_format::ObjectFormat;

fn run(dir: &Path, args: &[&str]) -> Result<Output, String> {
	let got_new = match Command::new("git")
		.args(ISOLATING_CONFIG)
		.args(args)
		.current_dir(dir)
		.output()
	{
		Ok(value) => value,
		Err(err) => {
			return Err(format!(
				"Command::new: {err:?}"
			));
		}
	};
	return Ok(
		got_new,
	);
}

/// Runs git in `dir`, asserts success, and returns trimmed stdout.
pub fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
	let out: Output = match run(dir, args) {
		Ok(value) => value,
		Err(err) => return Err(format!("run: {err}")),
	};
	assert!(out.status.success(), "git {args:?} in {}: {}", dir.display(), String::from_utf8_lossy(&out.stderr));
	let got_from_utf8 = match String::from_utf8(out.stdout) {
		Ok(value) => value,
		Err(err) => return Err(format!("String::from_utf8(out.stdout): {err:?}")),
	};
	return Ok(got_from_utf8
	.trim_end()
	.to_owned());
}

/// Runs git in `dir`, asserts failure, and returns trimmed stderr.
pub fn git_fails(dir: &Path, args: &[&str]) -> Result<String, String> {
	let out: Output = match run(dir, args) {
		Ok(value) => value,
		Err(err) => return Err(format!("run: {err}")),
	};
	assert!(!out.status.success(), "git {args:?} unexpectedly succeeded");
	let got_from_utf8_2 = match String::from_utf8(out.stderr) {
		Ok(value) => value,
		Err(err) => return Err(format!("String::from_utf8(out.stderr): {err:?}")),
	};
	return Ok(got_from_utf8_2
	.trim()
	.to_owned());
}

pub fn new_repo() -> Result<TempDir, String> {
	let dir: TempDir = match tempfile::tempdir() {
		Ok(value) => value,
		Err(err) => return Err(format!("tempfile::tempdir(): {err:?}")),
	};
	match git(dir.path(), &["init", "-q", "-b", "main"]) {
		Ok(_) => {}
		Err(err) => return Err(format!("git: {err}")),
	};
	return Ok(dir);
}

pub fn commit(dir: &Path, message: &str) -> Result<(), String> {
	match git(dir, &["commit", "-q", "--allow-empty", "--no-verify", "-m", message]) {
		Ok(_) => {}
		Err(err) => return Err(format!("git: {err}")),
	};
	return Ok(());
}

/// Same directory on disk, ignoring separator style, `..` and 8.3 short names.
pub fn same_dir(a: &Path, b: &Path) -> Result<bool, String> {
	let got_canonicalize = match fs::canonicalize(a) {
		Ok(value) => value,
		Err(err) => return Err(format!("fs::canonicalize(a): {err:?}")),
	};
	let got_canonicalize_2 = match fs::canonicalize(b) {
		Ok(value) => value,
		Err(err) => return Err(format!("fs::canonicalize(b): {err:?}")),
	};
	return Ok(got_canonicalize == got_canonicalize_2);
}

pub fn expect_err<T: Debug, E>(result: Result<T, E>, what: &str) -> Result<E, String> {
	return match result {
		Ok(value) => Err(format!("{what}: expected Err, got Ok({value:?})")),
		Err(err) => Ok(err),
	};
}

pub fn is_oid(text: &str) -> bool {
	return text.len() == ObjectFormat::Sha1.hex_len() && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
}
