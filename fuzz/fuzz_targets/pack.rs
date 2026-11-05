// A `.idx` and `.pack` read through the object API: sizes, delta chains and tree walks stay consistent.
// Without `--cfg fuzzing` this is an empty main, so the workspace builds and tests on stable.
#![cfg_attr(fuzzing, no_main)]

#[cfg(fuzzing)]
libfuzzer_sys::fuzz_target!(|data: &[u8]| {
	gitblitz_fuzz::pack(data);
});

#[cfg(not(fuzzing))]
fn main() {}
