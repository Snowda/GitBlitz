// Real-git fixtures: every assertion compares the reader against `git` itself, the oracle.

use std::fmt::{Debug, Display, Formatter, Result as FmtResult, Write as _};
use std::fs;
use std::io;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::thread;

use tempfile::TempDir;

use crate::fallback::trim_in_place;
use crate::isolating_config::ISOLATING_CONFIG;
use crate::object_format::ObjectFormat;
use crate::refs::OidHex;

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
	assert!(out.status.success(), "git {args:?} in {}: {}", dir.display(), Lossy(&out.stderr));
	let mut stdout: String = match String::from_utf8(out.stdout) {
		Ok(value) => value,
		Err(err) => return Err(format!("String::from_utf8(out.stdout): {err:?}")),
	};
	stdout.truncate(stdout.trim_end().len());
	return Ok(stdout);
}

/// Runs `git rev-parse <name>` in `dir` and keeps the printed id on the stack.
pub fn rev(dir: &Path, name: &str) -> Result<OidHex, String> {
	let printed: String = git(dir, &["rev-parse", name])?;
	return OidHex::from(&printed).map_err(|e| format!("rev-parse {name}: {e}"));
}

/// Runs git in `dir`, asserts failure, and returns trimmed stderr.
pub fn git_fails(dir: &Path, args: &[&str]) -> Result<String, String> {
	let out: Output = match run(dir, args) {
		Ok(value) => value,
		Err(err) => return Err(format!("run: {err}")),
	};
	assert!(!out.status.success(), "git {args:?} unexpectedly succeeded");
	let mut stderr: String = match String::from_utf8(out.stderr) {
		Ok(value) => value,
		Err(err) => return Err(format!("String::from_utf8(out.stderr): {err:?}")),
	};
	trim_in_place(&mut stderr);
	return Ok(stderr);
}

/// Runs git in `dir` with `input` on stdin, asserts success, and returns stdout byte for byte.
pub fn git_bytes(dir: &Path, args: &[&str], input: &[u8]) -> Result<Vec<u8>, String> {
	let mut child = Command::new("git")
		.args(ISOLATING_CONFIG)
		.args(args)
		.current_dir(dir)
		.stdin(Stdio::piped())
		.stdout(Stdio::piped())
		.stderr(Stdio::piped())
		.spawn()
		.map_err(|err| format!("spawn git {args:?}: {err:?}"))?;
	let mut stdin = child.stdin.take().ok_or("git stdin not piped")?;
	let out: Output = thread::scope(|scope| {
		scope.spawn(move || io::copy(&mut &input[..], &mut stdin));
		return child.wait_with_output();
	})
	.map_err(|err| format!("wait git {args:?}: {err:?}"))?;
	assert!(out.status.success(), "git {args:?} in {}: {}", dir.display(), Lossy(&out.stderr));
	return Ok(out.stdout);
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

/// `result`'s value, or its error as text that names the step `what`.
pub fn ok<T, E: Display>(result: Result<T, E>, what: &str) -> Result<T, String> {
	return match result {
		Ok(value) => Ok(value),
		Err(err) => Err(format!("{what}: {err}")),
	};
}

pub fn is_oid(text: &str) -> bool {
	return text.len() == ObjectFormat::Sha1.hex_len() && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
}

/// Bytes shown as `String::from_utf8_lossy` shows them, borrowed: each bad sequence prints as U+FFFD.
pub struct Lossy<'b>(pub &'b [u8]);

impl Display for Lossy<'_> {
	fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
		for chunk in self.0.utf8_chunks() {
			f.write_str(chunk.valid())?;
			if !chunk.invalid().is_empty() {
				f.write_char(char::REPLACEMENT_CHARACTER)?;
			}
		}
		return Ok(());
	}
}

#[cfg(test)]
mod tests {
	use super::Lossy;

	#[test]
	fn lossy_prints_what_from_utf8_lossy_builds() {
		let cases: [&[u8]; 6] = [b"", b"plain", b"a\xffb", b"\xc3", b"\xc3\xa9\xa9", b"\xf0\x9f\x98"];
		for bytes in cases {
			assert_eq!(Lossy(bytes).to_string(), String::from_utf8_lossy(bytes), "{bytes:?}");
		}
		// OFF baseline: a bad byte does not print as itself.
		assert_ne!(Lossy(b"a\xffb").to_string().as_bytes(), b"a\xffb");
	}
}
