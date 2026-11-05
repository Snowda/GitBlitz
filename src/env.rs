use std::ffi::OsStr;
use std::process::Command;

// Each of these redirects git's discovery or ref lookup, so the files alone no longer decide the answer.
pub const REDIRECTING: [&str; 6] =
	["GIT_DIR", "GIT_COMMON_DIR", "GIT_WORK_TREE", "GIT_CEILING_DIRECTORIES", "GIT_DISCOVERY_ACROSS_FILESYSTEM", "GIT_NAMESPACE"];
// Each of these moves the object store or changes which object an id reads.
pub const OBJECT_REDIRECTING: [&str; 4] =
	["GIT_OBJECT_DIRECTORY", "GIT_ALTERNATE_OBJECT_DIRECTORIES", "GIT_REPLACE_REF_BASE", "GIT_NO_REPLACE_OBJECTS"];

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

	/// [`Self::redirect`], or the first caller variable that moves the object store or replaces objects.
	#[must_use]
	pub fn object_redirect(&self) -> Option<&'a str> {
		let is_object_var = |name: &&str| -> bool { return OBJECT_REDIRECTING.contains(name) };
		return self.redirect().or_else(|| return self.vars.iter().map(|&(name, _)| name).find(is_object_var));
	}

	/// Gives `command` exactly the caller's variables: a redirect inherited from this process is removed.
	pub fn apply<'c>(&self, command: &'c mut Command) -> &'c mut Command {
		REDIRECTING.iter().for_each(|name: &&str| {
			command.env_remove(name);
		});
		return command.envs(self.vars.iter().copied());
	}

	/// [`Self::apply`] for an object command: an inherited object-store variable is removed too.
	pub fn apply_for_objects<'c>(&self, command: &'c mut Command) -> &'c mut Command {
		OBJECT_REDIRECTING.iter().for_each(|name: &&str| {
			command.env_remove(name);
		});
		return self.apply(command);
	}
}

#[cfg(test)]
mod tests {
	use std::ffi::OsStr;
	use std::process::Command;

	use foldhash::HashMap;

	use super::{GitEnv, REDIRECTING};

	fn applied<'c>(command: &'c mut Command, env: GitEnv<'_>) -> HashMap<&'c OsStr, Option<&'c OsStr>> {
		env.apply(command);
		return command.get_envs().collect();
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
		let (mut bare, mut with_vars): (Command, Command) = (Command::new("git"), Command::new("git"));
		let none: HashMap<&OsStr, Option<&OsStr>> = applied(&mut bare, GitEnv::NONE);
		assert_eq!(none.len(), REDIRECTING.len());
		assert!(REDIRECTING.iter().all(|name| none.get(OsStr::new(name)) == Some(&None)), "{none:?}");
		let vars: [(&str, &OsStr); 2] = [("GIT_DIR", OsStr::new("x/.git")), ("GIT_AUTHOR_NAME", OsStr::new("a"))];
		let set: HashMap<&OsStr, Option<&OsStr>> = applied(&mut with_vars, GitEnv::new(&vars));
		assert_eq!(set.get(OsStr::new("GIT_DIR")), Some(&Some(OsStr::new("x/.git"))));
		assert_eq!(set.get(OsStr::new("GIT_AUTHOR_NAME")), Some(&Some(OsStr::new("a"))));
		assert_eq!(set.get(OsStr::new("GIT_WORK_TREE")), Some(&None));
	}
}
