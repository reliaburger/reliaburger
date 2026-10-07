//! Every file lock in `src/` goes through [`super::FileLock`] (#613), checked
//! mechanically.
//!
//! Four flakes (#497, #500, #519, #606) came from locks that were fixed one at
//! a time, and #613 found three more. This test parses every source file with
//! `syn` and fails on any lock taken outside `src/file_lock.rs`:
//!
//! - an `flock`-family name anywhere: `flock`, `Flock`, `lockf`, `LOCK_EX`,
//!   `F_SETLK`, `TryLockError` and the like, or the `fs2`, `fs4` and
//!   `fd_lock` crates;
//! - a call to `.unlock()`, `.lock_shared()`, `.try_lock_shared()`,
//!   `.lock_exclusive()` or `.try_lock_exclusive()`;
//! - a `.try_lock()` call whose receiver isn't a field. In-memory mutexes are
//!   all struct fields (`self.rx.try_lock()`); a file lock is taken on a
//!   freshly opened local.
//!
//! Syntax can't see types, so this misses a file stored in a field and
//! locked there, and a blocking `.lock()` (which every `Mutex` has too).
//! `clippy.toml` covers those: it forbids `File`'s lock methods by type.
//! Macro bodies aren't parsed as expressions, so their tokens are checked
//! for the forbidden names and method calls only.

use std::path::{Path, PathBuf};

use proc_macro2::{TokenStream, TokenTree};
use syn::visit::Visit;

/// Names that only a file lock uses.
const LOCK_NAMES: &[&str] = &[
    "flock",
    "Flock",
    "FlockArg",
    "lockf",
    "LOCK_EX",
    "LOCK_SH",
    "LOCK_UN",
    "LOCK_NB",
    "F_GETLK",
    "F_SETLK",
    "F_SETLKW",
    "F_OFD_GETLK",
    "F_OFD_SETLK",
    "F_OFD_SETLKW",
    "TryLockError",
    "fs2",
    "fs4",
    "fd_lock",
];

/// Methods that only a file lock has.
const LOCK_METHODS: &[&str] = &[
    "unlock",
    "lock_shared",
    "try_lock_shared",
    "lock_exclusive",
    "try_lock_exclusive",
];

/// A lock taken outside the guard.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Violation {
    line: usize,
    what: String,
}

#[derive(Default)]
struct LockVisitor {
    violations: Vec<Violation>,
}

impl LockVisitor {
    fn flag(&mut self, span: proc_macro2::Span, what: String) {
        self.violations.push(Violation {
            line: span.start().line,
            what,
        });
    }

    fn check_tokens(&mut self, tokens: TokenStream) {
        let mut after_dot = false;
        for token in tokens {
            match &token {
                TokenTree::Ident(ident) => {
                    let name = ident.to_string();
                    if LOCK_NAMES.contains(&name.as_str()) {
                        self.flag(ident.span(), name);
                    } else if after_dot && LOCK_METHODS.contains(&name.as_str()) {
                        self.flag(ident.span(), format!(".{name}()"));
                    }
                    after_dot = false;
                }
                TokenTree::Punct(punct) => after_dot = punct.as_char() == '.',
                TokenTree::Group(group) => {
                    self.check_tokens(group.stream());
                    after_dot = false;
                }
                TokenTree::Literal(_) => after_dot = false,
            }
        }
    }
}

impl<'ast> Visit<'ast> for LockVisitor {
    fn visit_ident(&mut self, ident: &'ast proc_macro2::Ident) {
        let name = ident.to_string();
        if LOCK_NAMES.contains(&name.as_str()) {
            self.flag(ident.span(), name);
        }
    }

    fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
        let name = call.method.to_string();
        let field_receiver = matches!(call.receiver.as_ref(), syn::Expr::Field(_));
        if LOCK_METHODS.contains(&name.as_str()) || (name == "try_lock" && !field_receiver) {
            self.flag(call.method.span(), format!(".{name}()"));
        }
        syn::visit::visit_expr_method_call(self, call);
    }

    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        self.check_tokens(mac.tokens.clone());
        syn::visit::visit_macro(self, mac);
    }
}

/// Locks taken in one file's source.
fn violations(source: &str) -> Vec<Violation> {
    let file = syn::parse_file(source).unwrap_or_else(|error| panic!("unparsable: {error}"));
    let mut visitor = LockVisitor::default();
    visitor.visit_file(&file);
    visitor.violations.sort();
    visitor.violations
}

fn rust_sources(directory: &Path, sources: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(directory).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_sources(&path, sources);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            sources.push(path);
        }
    }
}

#[test]
fn every_file_lock_in_src_goes_through_the_guard() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let guard = root.join("file_lock");
    let mut sources = Vec::new();
    rust_sources(&root, &mut sources);
    sources.sort();
    let mut found = Vec::new();
    for path in sources {
        if path == root.join("file_lock.rs") || path.starts_with(&guard) {
            continue;
        }
        let source = std::fs::read_to_string(&path).unwrap();
        for violation in violations(&source) {
            found.push(format!(
                "{}:{}: {}",
                path.strip_prefix(&root).unwrap().display(),
                violation.line,
                violation.what
            ));
        }
    }
    assert!(
        found.is_empty(),
        "take file locks with crate::file_lock::FileLock, which unlocks in Drop \
         (#613):\n{}",
        found.join("\n")
    );
}

#[test]
fn a_try_lock_on_a_local_file_is_flagged() {
    let found = violations(
        "fn lock(path: &Path) -> io::Result<File> {
            let file = File::open(path)?;
            file.try_lock()?;
            Ok(file)
        }",
    );
    assert_eq!(
        found,
        vec![Violation {
            line: 3,
            what: ".try_lock()".into()
        }]
    );
}

#[test]
fn flock_names_and_unlock_calls_are_flagged() {
    let found = violations(
        "use nix::fcntl::Flock;
        fn f(file: File) {
            let _ = nix::fcntl::flock(file.as_raw_fd(), FlockArg::LockExclusive);
            file.unlock();
            match file.try_lock() { Err(std::fs::TryLockError::WouldBlock) => {} _ => {} }
            unsafe { libc::flock(fd, libc::LOCK_EX) };
        }",
    );
    let names: Vec<_> = found.iter().map(|v| v.what.as_str()).collect();
    assert_eq!(
        names,
        [
            "Flock",
            "FlockArg",
            "flock",
            ".unlock()",
            ".try_lock()",
            "TryLockError",
            "LOCK_EX",
            "flock",
        ]
    );
}

#[test]
fn locks_inside_macro_bodies_are_flagged() {
    let found = violations(
        "fn f(file: File) {
            assert!(file.unlock().is_ok());
            tokio::select! { _ = x => libc::flock(fd, libc::LOCK_UN), }
        }",
    );
    let names: Vec<_> = found.iter().map(|v| v.what.as_str()).collect();
    assert_eq!(names, [".unlock()", "LOCK_UN", "flock"]);
}

#[test]
fn a_mutex_field_try_lock_is_allowed() {
    assert!(violations("fn f(&self) { let _ = self.rx.try_lock(); }").is_empty());
}
