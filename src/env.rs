use std::ffi::OsStr;
use std::process::Command;

// Each of these redirects git's discovery or ref lookup, so the files alone no longer decide the answer.
pub const REDIRECTING: [&str; 6] =
	["GIT_DIR", "GIT_COMMON_DIR", "GIT_WORK_TREE", "GIT_CEILING_DIRECTORIES", "GIT_DISCOVERY_ACROSS_FILESYSTEM", "GIT_NAMESPACE"];

/// The `GIT_*` variables the caller sets for git. GitBlitz never reads its own process environment.
#[derive(Debug, Clone, Copy)]
pub struct GitEnv<'a> {
	vars: &'a [(&'a str, &'a OsStr)],
}

impl<'a> GitEnv<'a> {
	/// No variables: the repo is the one found on disk from the queried directory.
	pub const NONE: Self = Self { vars: &[] };

	#[must_use]
	pub const fn new(vars: &'a [(&'a str, &'a OsStr)]) -> Self {
		return Self { vars };
	}

	/// The first caller variable that redirects discovery or refs; the file reader cannot honour one.
	#[must_use]
	pub fn redirect(&self) -> Option<&'a str> {
		return self.vars.iter().map(|&(name, _)| name).find(|name: &&str| return REDIRECTING.contains(name));
	}

	/// Gives `command` exactly the caller's variables: a redirect inherited from this process is removed.
	pub fn apply<'c>(&self, command: &'c mut Command) -> &'c mut Command {
		REDIRECTING.iter().for_each(|name: &&str| {
			command.env_remove(name);
		});
		return command.envs(self.vars.iter().copied());
	}
}

#[cfg(test)]
mod tests {
	use std::collections::HashMap;
	use std::ffi::OsStr;
	use std::process::Command;

	use super::{GitEnv, REDIRECTING};

	fn applied(env: GitEnv<'_>) -> HashMap<String, Option<String>> {
		let mut command: Command = Command::new("git");
		env.apply(&mut command);
		return command
			.get_envs()
			.map(|(k, v)| return (k.to_string_lossy().into_owned(), v.map(|v| return v.to_string_lossy().into_owned())))
			.collect();
	}

	#[test]
	fn only_a_redirecting_name_is_a_redirect() {
		let author: [(&str, &OsStr); 1] = [("GIT_AUTHOR_NAME", OsStr::new("a"))];
		let both: [(&str, &OsStr); 2] = [("GIT_AUTHOR_NAME", OsStr::new("a")), ("GIT_WORK_TREE", OsStr::new("w"))];
		assert_eq!(GitEnv::NONE.redirect(), None);
		assert_eq!(GitEnv::new(&author).redirect(), None);
		assert_eq!(GitEnv::new(&both).redirect(), Some("GIT_WORK_TREE"));
	}

	// No variables removes every inherited redirect; a caller's variable is set over its removal.
	#[test]
	fn apply_removes_inherited_redirects_and_sets_the_callers() {
		let none: HashMap<String, Option<String>> = applied(GitEnv::NONE);
		assert_eq!(none.len(), REDIRECTING.len());
		assert!(REDIRECTING.iter().all(|name| none.get(*name) == Some(&None)), "{none:?}");
		let vars: [(&str, &OsStr); 2] = [("GIT_DIR", OsStr::new("x/.git")), ("GIT_AUTHOR_NAME", OsStr::new("a"))];
		let set: HashMap<String, Option<String>> = applied(GitEnv::new(&vars));
		assert_eq!(set.get("GIT_DIR"), Some(&Some("x/.git".to_owned())));
		assert_eq!(set.get("GIT_AUTHOR_NAME"), Some(&Some("a".to_owned())));
		assert_eq!(set.get("GIT_WORK_TREE"), Some(&None));
	}
}
