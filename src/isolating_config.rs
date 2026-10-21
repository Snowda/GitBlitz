// Pinned per call so the user's global config (signing, hooks, default branch) cannot skew a fixture.
pub const ISOLATING_CONFIG: [&str; 8] = [
	"-c",
	"user.name=gitblitz",
	"-c",
	"user.email=gitblitz@test.invalid",
	"-c",
	"commit.gpgsign=false",
	"-c",
	"core.hooksPath=.no-hooks",
];
