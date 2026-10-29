//! The fuzz targets on stable Rust: every body over its seeds and its
//! checked-in corpus, plus a small mutation run, so `make ci` checks the
//! same assertions the nightly fuzzer does (#1398).

use std::path::{Path, PathBuf};

use proptest::prelude::*;

use super::{Outcome, TARGETS, seeds};

fn fuzz_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fuzz")
}

fn corpus_files(target: &str) -> Vec<PathBuf> {
    let directory = fuzz_dir().join("corpus").join(target);
    let Ok(entries) = std::fs::read_dir(&directory) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .map(|entry| entry.expect("corpus entry reads").path())
        .filter(|path| path.is_file())
        .collect();
    files.sort();
    files
}

#[test]
fn every_fixture_seed_is_accepted_by_its_target() {
    for target in TARGETS {
        let seeds = seeds::for_target(target.name);
        assert!(!seeds.is_empty(), "{} has no seeds", target.name);
        for seed in seeds {
            assert_eq!(
                (target.run)(&seed.bytes),
                Outcome::Accepted,
                "{} ({}) refused its seed {}",
                target.name,
                target.decoder,
                seed.name
            );
        }
    }
}

#[test]
fn every_target_survives_its_checked_in_corpus() {
    for target in TARGETS {
        let files = corpus_files(target.name);
        assert!(
            !files.is_empty(),
            "fuzz/corpus/{}/ is empty: run make fuzz-corpus-seed",
            target.name
        );
        for file in files {
            let bytes = std::fs::read(&file).expect("corpus file reads");
            // A panic names the file through the test's output.
            eprintln!("{}: {}", target.name, file.display());
            let _ = (target.run)(&bytes);
        }
    }
}

#[test]
fn every_target_has_a_cargo_fuzz_binary() {
    let manifest = std::fs::read_to_string(fuzz_dir().join("Cargo.toml")).expect("fuzz manifest");
    let mut binaries: Vec<String> = std::fs::read_dir(fuzz_dir().join("fuzz_targets"))
        .expect("fuzz_targets reads")
        .map(|entry| {
            let path = entry.expect("fuzz_targets entry").path();
            path.file_stem()
                .expect("a target has a name")
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    binaries.sort();
    let mut names: Vec<String> = TARGETS.iter().map(|t| t.name.to_string()).collect();
    names.sort();
    assert_eq!(binaries, names, "fuzz/fuzz_targets and TARGETS differ");
    for target in TARGETS {
        let source = std::fs::read_to_string(
            fuzz_dir()
                .join("fuzz_targets")
                .join(format!("{}.rs", target.name)),
        )
        .expect("target source reads");
        assert!(
            source.contains(&format!("reliaburger::fuzz::{}(", target.name)),
            "fuzz_targets/{}.rs doesn't call its body",
            target.name
        );
        assert!(
            manifest.contains(&format!("name = \"{}\"", target.name)),
            "fuzz/Cargo.toml has no [[bin]] for {}",
            target.name
        );
    }
}

/// `make fuzz-corpus-seed` sets this to write the fixture seeds into
/// `fuzz/corpus/`. Unset, the test only checks they are already there.
const UPDATE_CORPUS: &str = "RELIABURGER_UPDATE_FUZZ_CORPUS";

#[test]
fn fixture_seeds_are_in_the_checked_in_corpus() {
    let update = std::env::var_os(UPDATE_CORPUS).is_some();
    for target in TARGETS {
        let directory = fuzz_dir().join("corpus").join(target.name);
        for seed in seeds::for_target(target.name) {
            let path = directory.join(&seed.name);
            if update {
                std::fs::create_dir_all(&directory).expect("corpus directory creates");
                std::fs::write(&path, &seed.bytes).expect("seed writes");
            }
            // Seeds with random keys or nonces differ on every run, so this
            // checks presence, not bytes.
            assert!(
                path.is_file(),
                "{} is missing: run make fuzz-corpus-seed",
                path.display()
            );
        }
    }
}

/// Overwrite, insert or delete a few bytes: a crude libFuzzer.
fn mutate(mut bytes: Vec<u8>, edits: &[(u8, usize, u8)]) -> Vec<u8> {
    for &(kind, position, value) in edits {
        let at = if bytes.is_empty() {
            0
        } else {
            position % bytes.len()
        };
        match kind % 3 {
            0 if !bytes.is_empty() => bytes[at] = value,
            1 => bytes.insert(at, value),
            _ if !bytes.is_empty() => {
                bytes.remove(at);
            }
            _ => bytes.push(value),
        }
    }
    bytes
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// Not a fuzzer, but it runs on stable in every `make ci`: mutated
    /// seeds must be refused or round-trip, never panic.
    #[test]
    fn mutated_seeds_never_panic(
        target in 0..TARGETS.len(),
        which in any::<usize>(),
        edits in proptest::collection::vec((any::<u8>(), any::<usize>(), any::<u8>()), 1..8),
    ) {
        let target = &TARGETS[target];
        let seeds = seeds::for_target(target.name);
        let seed = &seeds[which % seeds.len()];
        let _ = (target.run)(&mutate(seed.bytes.clone(), &edits));
    }
}
