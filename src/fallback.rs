use std::path::Path;
use std::process::{Command, Output};

use crate::env::GitEnv;
use crate::error::{Cause, Error, Miss};

/// Runs `git <args>` in `dir` with `env` and returns stdout without the newline at its end.
pub fn rev_parse(dir: &Path, args: &'static [&'static str], env: GitEnv<'_>, miss: Miss) -> Result<String, Error> {
	return rev_parse_into(dir, args, env, miss, Ok);
}

/// [`rev_parse`], then `parse` turns that stdout into a `T`; a refusal fails with its [`Cause`].
pub fn rev_parse_into<T>(
	dir: &Path,
	args: &'static [&'static str],
	env: GitEnv<'_>,
	miss: Miss,
	parse: impl FnOnce(String) -> Result<T, Cause>,
) -> Result<T, Error> {
	let fail = |cause: Cause| -> Error { return Error { dir: dir.to_path_buf(), args, miss, cause } };
	let output: Output = match env.apply(&mut Command::new("git")).args(args).current_dir(dir).output() {
		Ok(output) => output,
		Err(source) => return Err(fail(Cause::Spawn(source))),
	};
	if !output.status.success() {
		let stderr: String = String::from_utf8_lossy(&output.stderr).trim().to_owned();
		return Err(fail(Cause::Exit { status: output.status, stderr }));
	}
	return match String::from_utf8(output.stdout) {
		Ok(stdout) => parse(stdout.trim_end_matches(['\r', '\n']).to_owned()).map_err(fail),
		Err(_) => Err(fail(Cause::NotUtf8)),
	};
}

#[cfg(test)]
mod tests {
	use std::fs::OpenOptions;
	use std::io::Write;
	use std::path::Path;

	use super::rev_parse;
	use crate::env::GitEnv;
	use crate::error::{Cause, Error, Miss, MissReason};
	use crate::testrepo::{expect_err, new_repo};

	fn miss() -> Miss {
		return Miss::new(Path::new("probe"), MissReason::NotARepo);
	}

	#[test]
	fn success_strips_only_the_newline() -> Result<(), String> {
		let repo = new_repo()?;
		let got_rev_parse: String = rev_parse(repo.path(), &["rev-parse", "--is-inside-work-tree"], GitEnv::NONE, miss())
			.map_err(|err| format!("rev_parse: {err:?}"))?;
		assert_eq!(got_rev_parse, "true");
		return Ok(());
	}

	#[test]
	fn failure_carries_git_stderr_and_the_miss() -> Result<(), String> {
		let repo = new_repo()?;
		let err: Error = expect_err(rev_parse(repo.path(), &["rev-parse", "--verify", "nope"], GitEnv::NONE, miss()), "rev_parse(--verify nope)")?;
		let Cause::Exit { stderr, .. } = &err.cause else {
			return Err(format!("{err}"));
		};
		assert!(stderr.contains("fatal"), "{stderr}");
		assert_eq!(err.miss.reason, MissReason::NotARepo);
		assert_eq!(err.args, ["rev-parse", "--verify", "nope"]);
		assert!(err.to_string().starts_with("`git rev-parse --verify nope` in "), "{err}");
		assert!(err.to_string().contains("NotARepo at probe"), "{err}");
		return Ok(());
	}

	// git echoes a config value byte for byte, so a non-UTF-8 value reaches stdout unchanged.
	#[test]
	fn non_utf8_stdout_is_a_not_utf8_failure() -> Result<(), String> {
		let repo = new_repo()?;
		let mut config = OpenOptions::new()
			.append(true)
			.open(repo.path().join(".git").join("config"))
			.map_err(|err| format!("open .git/config: {err:?}"))?;
		config.write_all(b"[probe]\n\tvalue = caf\xe9\n").map_err(|err| format!("append config: {err:?}"))?;
		drop(config);
		let err: Error = expect_err(rev_parse(repo.path(), &["config", "probe.value"], GitEnv::NONE, miss()), "config probe.value")?;
		assert!(matches!(err.cause, Cause::NotUtf8), "{err:?}");
		assert_eq!(err.args, ["config", "probe.value"]);
		return Ok(());
	}

	#[test]
	fn a_missing_working_dir_is_a_spawn_failure() -> Result<(), String> {
		let repo = new_repo()?;
		let err: Error = expect_err(rev_parse(&repo.path().join("gone"), &["rev-parse", "--git-dir"], GitEnv::NONE, miss()), "rev_parse(gone)")?;
		assert!(matches!(err.cause, Cause::Spawn(_)), "{err:?}");
		assert!(err.dir.ends_with("gone"), "{err:?}");
		return Ok(());
	}
}
