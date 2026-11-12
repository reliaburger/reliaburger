//! No blocking file call runs directly in async code (#1857), checked
//! mechanically.
//!
//! A blocking file call made directly in async code ties up a runtime
//! worker until the kernel answers, and under writeback that can take
//! seconds. Whatever task is queued on that worker waits too. In the 0.2.8
//! gate it was the agent loop: a log forwarder's `SEEK_DATA` lseek held the
//! worker, and the loop's turn deadline couldn't fire because the loop's
//! task couldn't run (#1857). So the rule covers every async body in the
//! crate, not only the loop's.
//!
//! The check parses `src/` with `syn` and walks each `async fn`, `async`
//! block and `async` closure. A call there is blocking if it isn't awaited
//! and it is one of:
//!
//! - a `std::fs` or `fs::` function, or `File::open` / `File::create`;
//! - `OpenOptions::new()..open(..)`;
//! - `.sync_all()`, `.sync_data()` or `.set_len(..)`;
//! - a raw `nix` file call (`lseek`, `fallocate`, `fsync`, `fdatasync`,
//!   `ftruncate`);
//! - `std::thread::sleep`, which blocks its thread for the whole sleep;
//! - a call of a synchronous crate function that is blocking (#1860): one
//!   whose own code makes one of the calls above, or calls another such
//!   function, however deep. The rule reads every file once to collect
//!   what each synchronous function calls, then marks functions blocking
//!   until a round adds none (a fixed point), then reads every file again
//!   for the calls. It resolves free functions and `Self::` and `Type::`
//!   calls, so a method called on a value (`store.load()`) needs listing
//!   in [`BLOCKING_HELPERS`].
//!
//! It reads macro arguments too (#1860): a `select!` arm's future,
//! precondition and body, a `join!`'s futures, and any other macro's
//! comma-separated arguments (`format!`, `assert!`), through
//! `crate::macro_args`, which the agent loop's await rule shares.
//!
//! "Isn't awaited" is what tells `std::fs` from `tokio::fs`: the tokio calls
//! are futures, so a call followed by `.await` is never blocking.
//!
//! Names are resolved through the file's `use` items first, renames
//! included, so `use std::fs as disk; disk::read(..)` is caught and
//! `use tokio::fs::File; File::open(..)` isn't. A name no `use` explains
//! (one from a glob import) is judged by its shape: `fs::read`,
//! `File::open`. For crate functions, a name no `use` explains is taken to
//! be the current module's own item or child module's.
//!
//! A call passes inside a closure handed to something that runs it off the
//! runtime, named by its resolved path ([`OFF_RUNTIME_FUNCTIONS`]): tokio's
//! `spawn_blocking` and `block_in_place`, std's thread `spawn`, and the
//! crate's own wrappers around `spawn_blocking`. `std::thread::scope` is
//! the opposite: its closure runs on the calling thread, which it blocks
//! until every thread spawned in it has finished, so the call itself counts
//! as blocking. A method passes on
//! its name only where the name can't mean anything else
//! ([`OFF_RUNTIME_METHODS`]), or on `self` in the one type that defines it
//! ([`OFF_RUNTIME_SELF_METHODS`]). A call also passes when its statement (or
//! the comment block above it) says `// BLOCKING-OK: <why>`.
//!
//! What it doesn't see: a method called on a value, unless its name is in
//! [`BLOCKING_HELPERS`]; a crate function reached through a glob import or
//! a `pub use` re-export; blocking calls in a `Drop` impl, which run where
//! the value is dropped; a macro whose arguments aren't expressions
//! (`vec![x; n]`, `json!({..})`); and `#[cfg(test)]` code. The sites that
//! were already there when the rule arrived, and the `relish` CLI's that
//! the fixed point found, are in `blocking_rule_baseline.json`. That list
//! may only shrink. The CLI's own two shared helpers,
//! `BunClient::default_local` and `BuildInfo::current`, are tagged instead
//! (a one-shot read before any long-lived work), so a new command doesn't
//! inherit their sites; `relish mcp`, which is long-lived, calls the first
//! off the runtime.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use syn::spanned::Spanned;
use syn::visit::Visit;

use crate::macro_args::{MacroArgs, macro_args};

/// The comment that allows a blocking call in async code.
const TAG: &str = "BLOCKING-OK:";

/// Crate functions that do blocking file I/O themselves, matched by name,
/// called as a function or as a method. The fixed point finds free
/// functions and `Type::` calls on its own (#1860); this list is for a
/// method called on a value, whose type the rule can't know, and for a
/// name behind a glob import.
const BLOCKING_HELPERS: &[&str] = &[
    "data_start",
    "read_retained",
    "reclaim_ingested",
    "atomic_write",
    "write_checkpoint",
];

/// Raw `nix` file calls.
const NIX_FILE_CALLS: &[&str] = &["lseek", "fallocate", "fsync", "fdatasync", "ftruncate"];

/// Blocking methods on a `std::fs::File`.
const BLOCKING_METHODS: &[&str] = &["sync_all", "sync_data", "set_len"];

/// Functions whose closure argument runs off the async runtime, by their
/// resolved paths: tokio's and std's, and the crate's own wrappers around
/// `spawn_blocking`.
const OFF_RUNTIME_FUNCTIONS: &[&str] = &[
    "tokio::task::spawn_blocking",
    "tokio::task::block_in_place",
    "std::thread::spawn",
    "crate::bun::reusable_executor::pool::blocking",
    "crate::council::durable_log::run_blocking",
];

/// Methods whose closure argument runs off the runtime on whatever
/// receiver: no type here gives these names to anything else. The crate's
/// `SyncClaim::spawn_blocking` is one, called on a claim, not on `self`.
const OFF_RUNTIME_METHODS: &[&str] = &["spawn_blocking", "block_in_place"];

/// Methods that run their closure off the runtime only as the named type's
/// own private wrapper, called on `self`: `(type, method)`.
const OFF_RUNTIME_SELF_METHODS: &[(&str, &str)] = &[("ProcessControl", "run")];

/// Path roots a `use` doesn't need to explain.
const KNOWN_ROOTS: &[&str] = &["std", "core", "alloc", "crate", "tokio", "nix"];

/// Where the sites the rule found when it arrived are listed.
const BASELINE: &str = "src/blocking_rule_baseline.json";

/// One blocking call in async code: the file, the function it's in, and
/// what it calls. Line numbers move with every edit, so they aren't part
/// of the key.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Site {
    path: String,
    function: String,
    call: String,
}

impl std::fmt::Display for Site {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} in {}: {}", self.path, self.function, self.call)
    }
}

/// Whether `attrs` make an item test-only: a `cfg` that names `test`
/// (`#[cfg(test)]`, `#[cfg(all(test, target_os = "linux"))]`), or a test
/// function's own attribute (`#[test]`, `#[tokio::test]`).
fn is_cfg_test(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|attr| {
        let names_test = attr.path().is_ident("cfg")
            && attr.meta.require_list().is_ok_and(|list| {
                list.tokens
                    .to_string()
                    .split(|c: char| !c.is_alphanumeric() && c != '_')
                    .any(|word| word == "test")
            });
        names_test
            || attr
                .path()
                .segments
                .last()
                .is_some_and(|segment| segment.ident == "test")
    })
}

fn path_text(path: &syn::Path) -> String {
    path.segments
        .iter()
        .map(|segment| segment.ident.to_string())
        .collect::<Vec<_>>()
        .join("::")
}

/// The module a source file is, as a path from the crate root:
/// `src/grill/capture.rs` is `crate::grill::capture`.
fn module_of(path: &str) -> Vec<String> {
    let relative = path.strip_prefix("src/").unwrap_or(path);
    let relative = relative.strip_suffix(".rs").unwrap_or(relative);
    let mut module = vec!["crate".to_string()];
    module.extend(
        relative
            .split('/')
            .filter(|part| !matches!(*part, "lib" | "mod" | "main"))
            .map(str::to_string),
    );
    module
}

/// The names a file's non-test `use` items bring into scope, each with the
/// path it stands for. `use std::fs::{self as disk, File};` gives
/// `disk -> std::fs` and `File -> std::fs::File`. Globs bring no names.
fn use_aliases(file: &syn::File) -> std::collections::HashMap<String, Vec<String>> {
    fn walk(
        tree: &syn::UseTree,
        prefix: &mut Vec<String>,
        aliases: &mut std::collections::HashMap<String, Vec<String>>,
    ) {
        match tree {
            syn::UseTree::Path(path) => {
                prefix.push(path.ident.to_string());
                walk(&path.tree, prefix, aliases);
                prefix.pop();
            }
            syn::UseTree::Name(name) if name.ident == "self" => {
                if let Some(last) = prefix.last() {
                    aliases.insert(last.clone(), prefix.clone());
                }
            }
            syn::UseTree::Name(name) => {
                let mut full = prefix.clone();
                full.push(name.ident.to_string());
                aliases.insert(name.ident.to_string(), full);
            }
            syn::UseTree::Rename(rename) => {
                let mut full = prefix.clone();
                if rename.ident != "self" {
                    full.push(rename.ident.to_string());
                }
                aliases.insert(rename.rename.to_string(), full);
            }
            syn::UseTree::Glob(_) => {}
            syn::UseTree::Group(group) => {
                for item in &group.items {
                    walk(item, prefix, aliases);
                }
            }
        }
    }
    struct Uses(std::collections::HashMap<String, Vec<String>>);
    impl<'ast> Visit<'ast> for Uses {
        fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
            if !is_cfg_test(&item.attrs) {
                walk(&item.tree, &mut Vec::new(), &mut self.0);
            }
        }
        fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
            if !is_cfg_test(&item.attrs) {
                syn::visit::visit_item_mod(self, item);
            }
        }
        fn visit_item_fn(&mut self, item: &'ast syn::ItemFn) {
            if !is_cfg_test(&item.attrs) {
                syn::visit::visit_item_fn(self, item);
            }
        }
    }
    let mut uses = Uses(std::collections::HashMap::new());
    uses.visit_file(file);
    uses.0
}

/// A path as the file's `use` items resolve it.
struct Resolved {
    segments: Vec<String>,
    /// No `use` explains its first segment, and it isn't a known root: a
    /// glob import's name, or a local item's.
    unexplained: bool,
}

/// Why a crate function is blocking.
#[derive(Debug, Clone)]
enum Reason {
    /// It makes this blocking call itself.
    Makes(String),
    /// It calls this other blocking crate function.
    Calls(String),
}

/// What one synchronous crate function does, as far as the rule cares.
#[derive(Debug, Default)]
struct FnFacts {
    /// The first blocking call it makes itself, untagged.
    makes: Option<String>,
    /// The crate functions it calls, by resolved path.
    calls: Vec<String>,
}

/// One blocking call the rule found, with its line and, for a call into a
/// blocking crate function, the chain down to the I/O.
struct Hit {
    site: Site,
    line: usize,
    chain: Option<String>,
}

/// Walks one file. Run first with no blocking functions known, it collects
/// [`FnFacts`] for the file's synchronous functions. Run again with the
/// crate's blocking functions, it collects blocking calls in async code.
struct FileVisitor<'a> {
    path: &'a str,
    lines: &'a [String],
    /// What the file's `use` items bring into scope.
    aliases: std::collections::HashMap<String, Vec<String>>,
    /// The module being visited: the file's, plus any inline `mod`s.
    module: Vec<String>,
    /// The enclosing `impl` blocks' types, resolved, innermost last.
    impls: Vec<Vec<String>>,
    /// The enclosing functions, innermost last.
    functions: Vec<String>,
    /// Whether the code being visited runs on the async runtime.
    in_async: bool,
    /// The synchronous crate function whose own code is being visited:
    /// not inside an `async` block, nor a closure handed off the runtime.
    owner: Option<String>,
    /// Start lines of the enclosing statements.
    statements: Vec<usize>,
    /// The call directly under an `.await`, which is never blocking.
    awaited: Option<proc_macro2::Span>,
    /// The crate's blocking functions, by resolved path.
    blocking_functions: &'a BTreeMap<String, Reason>,
    /// Where each struct and enum name is defined in the crate.
    types: &'a BTreeMap<String, Vec<String>>,
    facts: BTreeMap<String, FnFacts>,
    found: Vec<Hit>,
}

impl FileVisitor<'_> {
    /// `path` with its first segment expanded through the file's `use`
    /// items, `Self` made the enclosing `impl`'s type, and `self` and
    /// `super` made absolute. A path no `use` explains is taken to start in
    /// the current module: a local item, or a child module's.
    fn resolve(&self, path: &syn::Path) -> Resolved {
        let mut segments: Vec<String> = path
            .segments
            .iter()
            .map(|segment| segment.ident.to_string())
            .collect();
        let first = segments.first().cloned().unwrap_or_default();
        if first == "Self"
            && let Some(ty) = self.impls.last()
        {
            segments.splice(0..1, ty.iter().cloned());
            return Resolved {
                segments,
                unexplained: false,
            };
        }
        let mut unexplained = false;
        if path.leading_colon.is_none()
            && let Some(full) = self.aliases.get(&first)
        {
            segments.splice(0..1, full.iter().cloned());
        } else if !KNOWN_ROOTS.contains(&first.as_str()) && first != "self" && first != "super" {
            unexplained = true;
        }
        if segments.first().is_some_and(|first| first == "self") {
            segments.splice(0..1, self.module.iter().cloned());
        }
        let mut module = self.module.clone();
        let mut up = false;
        while segments.first().is_some_and(|first| first == "super") {
            segments.remove(0);
            module.pop();
            up = true;
        }
        if up {
            segments.splice(0..0, module);
        }
        if unexplained {
            // A glob import (`use super::*`) leaves no alias, so a type
            // defined once, in another module, is taken to be that one:
            // otherwise each file splitting an `impl` names a different
            // type (#1887).
            let mut local = self.module.clone();
            local.push(first.clone());
            match self.types.get(&first).map(Vec::as_slice) {
                Some([defined]) if *defined != local.join("::") => {
                    segments.splice(0..1, defined.split("::").map(str::to_string));
                }
                _ => {
                    segments.splice(0..0, self.module.iter().cloned());
                }
            }
        }
        Resolved {
            segments,
            unexplained,
        }
    }

    /// Whether a call of `function` is a blocking file call.
    fn blocking_path(&self, function: &syn::Path) -> bool {
        let resolved = self.resolve(function);
        let segments = &resolved.segments;
        let Some(last) = segments.last().map(String::as_str) else {
            return false;
        };
        if segments.first().is_some_and(|first| first == "tokio") {
            return false;
        }
        let joined = segments.join("::");
        // `OpenOptions::new`, `DirBuilder::new` and `Permissions::from_mode`
        // only build values; the call that uses them does the I/O.
        let constructor = matches!(last, "new" | "from_mode");
        if (joined.starts_with("std::fs::") || joined.starts_with("std::os::unix::fs::"))
            && !constructor
        {
            return true;
        }
        // A scope blocks until its threads finish; a sleep blocks for its
        // whole duration.
        if joined == "std::thread::scope" || joined == "std::thread::sleep" {
            return true;
        }
        if NIX_FILE_CALLS.contains(&last)
            && (segments.iter().any(|segment| segment == "nix")
                || (resolved.unexplained && function.segments.len() == 1))
        {
            return true;
        }
        if BLOCKING_HELPERS.contains(&last) {
            return true;
        }
        // A name no `use` explains: judge it by its shape.
        let second_last = segments
            .len()
            .checked_sub(2)
            .map(|index| segments[index].as_str());
        resolved.unexplained
            && function.segments.len() >= 2
            && (second_last == Some("fs")
                || (second_last == Some("File") && matches!(last, "open" | "create")))
    }

    /// Whether `function` runs a closure argument off the runtime.
    fn off_runtime_function(&self, function: &syn::Path) -> bool {
        let joined = self.resolve(function).segments.join("::");
        OFF_RUNTIME_FUNCTIONS.contains(&joined.as_str())
    }

    /// Whether a method chain starts at a `std::fs::OpenOptions::new()`.
    fn starts_at_open_options(&self, expr: &syn::Expr) -> bool {
        match expr {
            syn::Expr::MethodCall(call) => self.starts_at_open_options(&call.receiver),
            syn::Expr::Call(call) => match call.func.as_ref() {
                syn::Expr::Path(function) => {
                    let resolved = self.resolve(&function.path);
                    let joined = resolved.segments.join("::");
                    joined == "std::fs::OpenOptions::new"
                        || (resolved.unexplained && joined.ends_with("OpenOptions::new"))
                }
                _ => false,
            },
            syn::Expr::Paren(inner) => self.starts_at_open_options(&inner.expr),
            _ => false,
        }
    }

    /// Whether a method chain starts at `std::thread::Builder::new()`.
    fn starts_at_thread_builder(&self, expr: &syn::Expr) -> bool {
        match expr {
            syn::Expr::MethodCall(call) => self.starts_at_thread_builder(&call.receiver),
            syn::Expr::Call(call) => match call.func.as_ref() {
                syn::Expr::Path(function) => {
                    self.resolve(&function.path).segments.join("::") == "std::thread::Builder::new"
                }
                _ => false,
            },
            _ => false,
        }
    }

    /// Whether a method call runs its closure argument off the runtime.
    fn off_runtime_method(&self, call: &syn::ExprMethodCall) -> bool {
        let method = call.method.to_string();
        if OFF_RUNTIME_METHODS.contains(&method.as_str()) {
            return true;
        }
        if method == "spawn" && self.starts_at_thread_builder(&call.receiver) {
            return true;
        }
        let on_self = matches!(call.receiver.as_ref(), syn::Expr::Path(receiver) if receiver.path.is_ident("self"));
        on_self
            && self
                .impls
                .last()
                .and_then(|ty| ty.last())
                .is_some_and(|ty| {
                    OFF_RUNTIME_SELF_METHODS.contains(&(ty.as_str(), method.as_str()))
                })
    }

    fn tagged(&self, start: usize, end: usize) -> bool {
        let justified = |line: &str| {
            line.split_once(TAG)
                .is_some_and(|(_, why)| !why.trim().is_empty())
        };
        let line = |number: usize| self.lines.get(number - 1).map(String::as_str);
        if (start..=end).filter_map(line).any(justified) {
            return true;
        }
        let mut above = start.saturating_sub(1);
        while above > 0 {
            let Some(text) = line(above) else { break };
            if !text.trim_start().starts_with("//") {
                break;
            }
            if justified(text) {
                return true;
            }
            above -= 1;
        }
        false
    }

    /// Note a call that blocks if it isn't awaited or tagged: `call` as
    /// written, and `callee`, the blocking crate function it calls, if
    /// that's what it is. In async code it's a hit. In a synchronous crate
    /// function's own code it makes that function blocking.
    fn record(&mut self, call: String, span: proc_macro2::Span, callee: Option<String>) {
        if !self.in_async && self.owner.is_none() {
            return;
        }
        if self
            .awaited
            .is_some_and(|awaited| format!("{awaited:?}") == format!("{span:?}"))
        {
            return;
        }
        let end = span.start().line;
        let start = self.statements.last().copied().unwrap_or(end);
        if self.tagged(start, end) {
            return;
        }
        if !self.in_async {
            if let Some(owner) = &self.owner {
                let facts = self.facts.entry(owner.clone()).or_default();
                match callee {
                    Some(callee) => facts.calls.push(callee),
                    None => {
                        facts.makes.get_or_insert(call);
                    }
                }
            }
            return;
        }
        if callee
            .as_ref()
            .is_some_and(|callee| !self.blocking_functions.contains_key(callee))
        {
            return;
        }
        let chain = callee.map(|callee| explain(self.blocking_functions, &callee));
        let function = self.functions.join("::");
        self.found.push(Hit {
            site: Site {
                path: self.path.to_string(),
                function,
                call,
            },
            line: end,
            chain,
        });
    }

    /// Visit a function's body: `key` names it if it is a synchronous
    /// crate function the fixed point should know about.
    fn with_function(
        &mut self,
        name: String,
        is_async: bool,
        key: Option<String>,
        visit: impl FnOnce(&mut Self),
    ) {
        let outer = (self.in_async, self.owner.take());
        self.functions.push(name);
        self.in_async = is_async;
        self.owner = if is_async { None } else { key };
        visit(self);
        (self.in_async, self.owner) = outer;
        self.functions.pop();
    }

    /// Visit async code: an `async` block or closure, which belongs to no
    /// synchronous function even when one builds it.
    fn as_async(&mut self, visit: impl FnOnce(&mut Self)) {
        let outer = (self.in_async, self.owner.take());
        self.in_async = true;
        visit(self);
        (self.in_async, self.owner) = outer;
    }

    /// Visit closure arguments of an off-runtime call as synchronous code
    /// that no function on the runtime runs.
    fn visit_args_off_runtime<'ast>(
        &mut self,
        args: impl Iterator<Item = &'ast syn::Expr>,
        off_runtime: bool,
    ) {
        for arg in args {
            if off_runtime
                && matches!(arg, syn::Expr::Closure(closure) if closure.asyncness.is_none())
            {
                let outer = (self.in_async, self.owner.take());
                self.in_async = false;
                self.visit_expr(arg);
                (self.in_async, self.owner) = outer;
            } else {
                self.visit_expr(arg);
            }
        }
    }

    /// Visit an expression something awaits: an `.await`'s base, a
    /// `select!` arm's future, a `join!` argument.
    fn visit_awaited(&mut self, base: &syn::Expr) {
        let outer = self.awaited;
        self.awaited = match base {
            syn::Expr::Call(call) => Some(call.span()),
            syn::Expr::MethodCall(call) => Some(call.span()),
            _ => None,
        };
        self.visit_expr(base);
        self.awaited = outer;
    }
}

/// The chain from a blocking crate function down to its blocking call:
/// `crate::a::load -> crate::a::read_raw -> std::fs::read`.
fn explain(blocking: &BTreeMap<String, Reason>, function: &str) -> String {
    let mut chain = vec![function.to_string()];
    let mut next = function.to_string();
    // Each function was marked blocking after the one it calls, so the
    // chain ends; the bound is only a guard.
    for _ in 0..=blocking.len() {
        match blocking.get(&next) {
            Some(Reason::Calls(callee)) => {
                chain.push(callee.clone());
                next = callee.clone();
            }
            Some(Reason::Makes(call)) => {
                chain.push(call.clone());
                break;
            }
            None => break,
        }
    }
    chain.join(" -> ")
}

/// The crate's blocking functions: those that make a blocking call
/// themselves, then, until nothing changes, those that call one. This is a
/// fixed point: each round can only add functions, and there are finitely
/// many, so it stops.
fn blocking_functions(facts: &BTreeMap<String, FnFacts>) -> BTreeMap<String, Reason> {
    let mut blocking: BTreeMap<String, Reason> = facts
        .iter()
        .filter_map(|(function, facts)| {
            let call = facts.makes.clone()?;
            Some((function.clone(), Reason::Makes(call)))
        })
        .collect();
    loop {
        let mut added = Vec::new();
        for (function, facts) in facts {
            if blocking.contains_key(function) {
                continue;
            }
            if let Some(callee) = facts
                .calls
                .iter()
                .find(|callee| blocking.contains_key(*callee))
            {
                added.push((function.clone(), Reason::Calls(callee.clone())));
            }
        }
        if added.is_empty() {
            return blocking;
        }
        blocking.extend(added);
    }
}

impl<'ast> Visit<'ast> for FileVisitor<'_> {
    fn visit_item_fn(&mut self, item: &'ast syn::ItemFn) {
        if is_cfg_test(&item.attrs) {
            return;
        }
        let is_async = item.sig.asyncness.is_some();
        let name = item.sig.ident.to_string();
        let mut key = self.module.clone();
        key.push(name.clone());
        self.with_function(name, is_async, Some(key.join("::")), |this| {
            this.visit_block(&item.block);
        });
    }

    fn visit_impl_item_fn(&mut self, item: &'ast syn::ImplItemFn) {
        if is_cfg_test(&item.attrs) {
            return;
        }
        let is_async = item.sig.asyncness.is_some();
        let name = item.sig.ident.to_string();
        let key = self
            .impls
            .last()
            .map(|ty| format!("{}::{name}", ty.join("::")));
        self.with_function(name, is_async, key, |this| {
            this.visit_block(&item.block);
        });
    }

    fn visit_trait_item_fn(&mut self, item: &'ast syn::TraitItemFn) {
        if is_cfg_test(&item.attrs) {
            return;
        }
        let Some(block) = &item.default else {
            return;
        };
        let is_async = item.sig.asyncness.is_some();
        self.with_function(item.sig.ident.to_string(), is_async, None, |this| {
            this.visit_block(block);
        });
    }

    fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
        if is_cfg_test(&item.attrs) {
            return;
        }
        self.module.push(item.ident.to_string());
        syn::visit::visit_item_mod(self, item);
        self.module.pop();
    }

    fn visit_item_impl(&mut self, item: &'ast syn::ItemImpl) {
        if is_cfg_test(&item.attrs) {
            return;
        }
        let ty = match item.self_ty.as_ref() {
            syn::Type::Path(path) => self.resolve(&path.path).segments,
            _ => Vec::new(),
        };
        self.impls.push(ty);
        syn::visit::visit_item_impl(self, item);
        self.impls.pop();
    }

    fn visit_stmt(&mut self, stmt: &'ast syn::Stmt) {
        let attrs = match stmt {
            syn::Stmt::Local(local) => local.attrs.as_slice(),
            syn::Stmt::Macro(mac) => mac.attrs.as_slice(),
            _ => &[],
        };
        if is_cfg_test(attrs) {
            return;
        }
        self.statements.push(stmt.span().start().line);
        syn::visit::visit_stmt(self, stmt);
        self.statements.pop();
    }

    fn visit_expr(&mut self, expr: &'ast syn::Expr) {
        let attrs: &[syn::Attribute] = match expr {
            syn::Expr::Block(inner) => &inner.attrs,
            syn::Expr::If(inner) => &inner.attrs,
            syn::Expr::Call(inner) => &inner.attrs,
            syn::Expr::MethodCall(inner) => &inner.attrs,
            syn::Expr::Await(inner) => &inner.attrs,
            _ => &[],
        };
        if is_cfg_test(attrs) {
            return;
        }
        syn::visit::visit_expr(self, expr);
    }

    fn visit_expr_async(&mut self, block: &'ast syn::ExprAsync) {
        self.as_async(|this| syn::visit::visit_expr_async(this, block));
    }

    fn visit_expr_closure(&mut self, closure: &'ast syn::ExprClosure) {
        if closure.asyncness.is_some() {
            self.as_async(|this| syn::visit::visit_expr_closure(this, closure));
        } else {
            // A synchronous closure in async code usually runs inline (an
            // iterator adapter, say), so it inherits its context.
            syn::visit::visit_expr_closure(self, closure);
        }
    }

    fn visit_expr_await(&mut self, expr: &'ast syn::ExprAwait) {
        self.visit_awaited(&expr.base);
    }

    fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
        let awaited = self.awaited.take();
        let mut off_runtime = false;
        if let syn::Expr::Path(function) = call.func.as_ref() {
            off_runtime = self.off_runtime_function(&function.path);
            let callee = self.resolve(&function.path).segments;
            let blocking = self.blocking_path(&function.path);
            if blocking || callee.first().is_some_and(|root| root == "crate") {
                self.awaited = awaited;
                let callee = (!blocking).then(|| callee.join("::"));
                self.record(path_text(&function.path), call.span(), callee);
                self.awaited = None;
            }
        }
        self.visit_expr(&call.func);
        self.visit_args_off_runtime(call.args.iter(), off_runtime);
    }

    fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
        let awaited = self.awaited.take();
        let method = call.method.to_string();
        let blocking = BLOCKING_METHODS.contains(&method.as_str())
            || BLOCKING_HELPERS.contains(&method.as_str())
            || (method == "open" && self.starts_at_open_options(&call.receiver));
        let on_self = matches!(call.receiver.as_ref(), syn::Expr::Path(receiver) if receiver.path.is_ident("self"));
        if blocking {
            self.awaited = awaited;
            self.record(format!(".{method}()"), call.span(), None);
            self.awaited = None;
        } else if on_self && let Some(ty) = self.impls.last().filter(|ty| !ty.is_empty()) {
            // The enclosing `impl` names the receiver's type, so `self.m()`
            // is `<impl type>::m`, like `Self::m(self)` (#1887).
            let callee = format!("{}::{method}", ty.join("::"));
            self.awaited = awaited;
            self.record(format!("self.{method}()"), call.span(), Some(callee));
            self.awaited = None;
        }
        self.visit_expr(&call.receiver);
        let off_runtime = self.off_runtime_method(call);
        self.visit_args_off_runtime(call.args.iter(), off_runtime);
    }

    /// `syn` leaves a macro's arguments as tokens; `macro_args` parses the
    /// ones it can. The parsed expressions are owned by this function, not
    /// by the file's syntax tree, so they don't live for `'ast`. That's
    /// fine: this impl is for every `'ast`, and visiting them picks the
    /// shorter lifetime of the local borrow.
    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        match macro_args(mac) {
            MacroArgs::Select(arms) => {
                for arm in &arms {
                    if let Some(future) = &arm.future {
                        self.visit_awaited(future);
                    }
                    if let Some(condition) = &arm.condition {
                        self.visit_expr(condition);
                    }
                    self.visit_expr(&arm.body);
                }
            }
            MacroArgs::Join(futures) => {
                for future in &futures {
                    self.visit_awaited(future);
                }
            }
            MacroArgs::Other(args) => {
                for arg in &args {
                    self.visit_expr(arg);
                }
            }
        }
    }
}

/// One source file, parsed.
struct ParsedFile<'a> {
    path: &'a str,
    lines: Vec<String>,
    file: syn::File,
}

/// Collects where each struct and enum is defined, outside test code.
struct TypeIndex {
    module: Vec<String>,
    types: BTreeMap<String, Vec<String>>,
}

impl TypeIndex {
    fn define(&mut self, name: &syn::Ident) {
        let mut path = self.module.clone();
        path.push(name.to_string());
        self.types
            .entry(name.to_string())
            .or_default()
            .push(path.join("::"));
    }
}

impl<'ast> Visit<'ast> for TypeIndex {
    fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
        if is_cfg_test(&item.attrs) {
            return;
        }
        self.module.push(item.ident.to_string());
        syn::visit::visit_item_mod(self, item);
        self.module.pop();
    }

    fn visit_item_struct(&mut self, item: &'ast syn::ItemStruct) {
        if !is_cfg_test(&item.attrs) {
            self.define(&item.ident);
        }
    }

    fn visit_item_enum(&mut self, item: &'ast syn::ItemEnum) {
        if !is_cfg_test(&item.attrs) {
            self.define(&item.ident);
        }
    }
}

fn visit_file<'a>(
    parsed: &'a ParsedFile<'a>,
    blocking_functions: &'a BTreeMap<String, Reason>,
    types: &'a BTreeMap<String, Vec<String>>,
) -> FileVisitor<'a> {
    let mut visitor = FileVisitor {
        path: parsed.path,
        lines: &parsed.lines,
        aliases: use_aliases(&parsed.file),
        module: module_of(parsed.path),
        impls: Vec::new(),
        functions: Vec::new(),
        in_async: false,
        owner: None,
        statements: Vec::new(),
        awaited: None,
        blocking_functions,
        types,
        facts: BTreeMap::new(),
        found: Vec::new(),
    };
    visitor.visit_file(&parsed.file);
    visitor
}

/// Every blocking call in async code in a set of `(path, text)` source
/// files, read as one crate: a first pass finds the blocking functions, a
/// second the calls.
fn sites_in_files(files: &[(String, String)]) -> Vec<Hit> {
    let parsed: Vec<ParsedFile> = files
        .iter()
        .map(|(path, text)| ParsedFile {
            path,
            lines: text.lines().map(str::to_string).collect(),
            file: syn::parse_file(text)
                .unwrap_or_else(|error| panic!("{path} does not parse: {error}")),
        })
        .collect();
    let mut index = TypeIndex {
        module: Vec::new(),
        types: BTreeMap::new(),
    };
    for file in &parsed {
        index.module = module_of(file.path);
        index.visit_file(&file.file);
    }
    let types = index.types;
    let none = BTreeMap::new();
    let mut facts = BTreeMap::new();
    for file in &parsed {
        facts.extend(visit_file(file, &none, &types).facts);
    }
    let blocking = blocking_functions(&facts);
    parsed
        .iter()
        .flat_map(|file| visit_file(file, &blocking, &types).found)
        .collect()
}

/// Every `.rs` file under `src/` that isn't test-only by its path.
fn source_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut directories = vec![root.join("src")];
    while let Some(directory) = directories.pop() {
        for entry in std::fs::read_dir(&directory).unwrap() {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            if path.is_dir() {
                if name != "tests" {
                    directories.push(path);
                }
            } else if path.extension().is_some_and(|extension| extension == "rs")
                && name != "tests.rs"
                && !name.ends_with("_tests.rs")
            {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

/// How many of each site the whole crate has, and where each one is: its
/// lines, and the chain down to the I/O for a call into a blocking crate
/// function.
fn crate_sites() -> (BTreeMap<Site, usize>, BTreeMap<Site, Vec<String>>) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let files: Vec<(String, String)> = source_files(root)
        .into_iter()
        .map(|file| {
            let relative = file.strip_prefix(root).unwrap().display().to_string();
            (relative, std::fs::read_to_string(&file).unwrap())
        })
        .collect();
    let mut counts = BTreeMap::new();
    let mut places: BTreeMap<Site, Vec<String>> = BTreeMap::new();
    for hit in sites_in_files(&files) {
        *counts.entry(hit.site.clone()).or_default() += 1;
        let place = match hit.chain {
            Some(chain) => format!("line {}, via {chain}", hit.line),
            None => format!("line {}", hit.line),
        };
        places.entry(hit.site).or_default().push(place);
    }
    (counts, places)
}

/// The baseline file's form: one entry per site, with how many there are.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct BaselineEntry {
    path: String,
    function: String,
    call: String,
    count: usize,
}

fn read_baseline(root: &Path) -> BTreeMap<Site, usize> {
    let text = std::fs::read_to_string(root.join(BASELINE)).unwrap();
    let entries: Vec<BaselineEntry> = serde_json::from_str(&text).unwrap();
    entries
        .into_iter()
        .map(|entry| {
            (
                Site {
                    path: entry.path,
                    function: entry.function,
                    call: entry.call,
                },
                entry.count,
            )
        })
        .collect()
}

#[test]
fn no_new_blocking_file_call_runs_directly_in_async_code() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let (found, places) = crate_sites();
    let baseline = read_baseline(root);
    if std::env::var_os("RELIABURGER_WRITE_BLOCKING_BASELINE").is_some() {
        let entries: Vec<BaselineEntry> = found
            .iter()
            .map(|(site, count)| BaselineEntry {
                path: site.path.clone(),
                function: site.function.clone(),
                call: site.call.clone(),
                count: *count,
            })
            .collect();
        let mut text = serde_json::to_string_pretty(&entries).unwrap();
        text.push('\n');
        std::fs::write(root.join(BASELINE), text).unwrap();
        return;
    }
    let new: Vec<String> = found
        .iter()
        .filter(|(site, count)| baseline.get(*site).is_none_or(|allowed| *count > allowed))
        .map(|(site, _)| format!("{site} ({})", places[site].join("; ")))
        .collect();
    assert!(
        new.is_empty(),
        "{} blocking file call(s) run directly in async code, on a runtime worker. \
         Under writeback one can hold the worker for seconds, and whatever task is \
         queued there waits too, the agent loop included (#1857). Move the call into \
         `tokio::task::spawn_blocking(move || ..)`, use the `tokio::fs` equivalent, or, \
         if it can't block for long, tag its statement `// {TAG} <why>`:\n{}",
        new.len(),
        new.join("\n")
    );
    let fixed: Vec<String> = baseline
        .iter()
        .filter(|(site, allowed)| found.get(*site).is_none_or(|count| count < *allowed))
        .map(|(site, _)| site.to_string())
        .collect();
    assert!(
        fixed.is_empty(),
        "{} baseline site(s) in {BASELINE} are gone or fewer. The list may only \
         shrink: rerun with RELIABURGER_WRITE_BLOCKING_BASELINE=1 to drop them:\n{}",
        fixed.len(),
        fixed.join("\n")
    );
}

/// `(function, call)` for each site the rule finds in `source`, read as
/// the file at `path`.
fn found_in(path: &str, source: &str) -> Vec<(String, String)> {
    sites_in_files(&[(path.to_string(), source.to_string())])
        .into_iter()
        .map(|hit| (hit.site.function, hit.site.call))
        .collect()
}

fn pairs(expected: &[(&str, &str)]) -> Vec<(String, String)> {
    expected
        .iter()
        .map(|(function, call)| (function.to_string(), call.to_string()))
        .collect()
}

#[test]
fn the_rule_flags_unawaited_file_calls_in_async_code_only() {
    let source = r#"
        async fn reads(path: &Path) {
            let bytes = std::fs::read(path);
            let file = tokio::fs::File::open(path).await;
            let text = tokio::fs::read_to_string(path).await;
            let opened = File::open(path);
            let options = OpenOptions::new().write(true).open(path);
            file.sync_all();
            other.sync_all().await;
            data_start(&file, 0);
            nix::unistd::lseek(&file, 0, Whence::SeekData);
            let joined = tokio::task::spawn_blocking(move || std::fs::read(path)).await;
            // BLOCKING-OK: a sysfs read, answered from memory
            let tagged = std::fs::read_to_string("/sys/x");
            // BLOCKING-OK:
            let untagged = std::fs::metadata(path);
            let inline: Vec<_> = paths.iter().map(|p| std::fs::read(p)).collect();
            std::thread::spawn(move || std::fs::remove_file(path));
            let block = async { std::fs::write(path, b"x") };
        }
        fn sync_code(path: &Path) {
            std::fs::read(path);
        }
        impl Store {
            async fn method(&self) {
                self.helper();
                read_retained(&self.path);
            }
        }
        #[cfg(test)]
        mod tests {
            async fn ignored() { std::fs::read("x"); }
        }
    "#;
    assert_eq!(
        found_in("src/x.rs", source),
        pairs(&[
            ("reads", "std::fs::read"),
            ("reads", "File::open"),
            ("reads", ".open()"),
            ("reads", ".sync_all()"),
            ("reads", "data_start"),
            ("reads", "nix::unistd::lseek"),
            // A tag with no reason doesn't count.
            ("reads", "std::fs::metadata"),
            // An iterator's closure runs inline.
            ("reads", "std::fs::read"),
            ("reads", "std::fs::write"),
            ("method", "read_retained"),
        ])
    );
}

/// Each way a blocking call used to slip past the rule, by renaming what
/// it calls or by borrowing an exempt name.
#[test]
fn the_rule_sees_through_use_aliases_and_borrowed_names() {
    let source = r#"
        use std::fs as filesystem;
        use std::fs::read as slurp;
        use std::fs::{self as disk, File as Handle, OpenOptions as Options};
        use tokio::fs::File as AsyncFile;
        use tokio::task::spawn_blocking as offload;
        use nix::unistd::lseek as seek;
        use std::thread;
        async fn dodges(path: &Path) {
            filesystem::read(path);
            slurp(path);
            disk::write(path, b"x");
            Handle::open(path);
            Options::new().read(true).open(path);
            seek(&file, 0, Whence::SeekData);
            AsyncFile::open(path);
            offload(move || std::fs::read(path)).await;
            thread::spawn(move || std::fs::read(path));
            thread::Builder::new().spawn(move || std::fs::read(path));
            thread::scope(|s| { s.spawn(|| std::fs::read(path)); });
            claim.spawn_blocking(move || std::fs::read(path)).await;
            scope(|| std::fs::read(path));
            run(|| std::fs::read(path));
            blocking(|| std::fs::read(path)).await;
            self.run(|| std::fs::read(path)).await;
            other.run(|| std::fs::read(path)).await;
        }
        impl ProcessControl {
            async fn forget(&self) {
                self.run(|| std::fs::read(path)).await;
            }
        }
    "#;
    assert_eq!(
        found_in("src/x.rs", source),
        pairs(&[
            ("dodges", "filesystem::read"),
            ("dodges", "slurp"),
            ("dodges", "disk::write"),
            ("dodges", "Handle::open"),
            ("dodges", ".open()"),
            ("dodges", "seek"),
            // A scope blocks its caller until its threads finish, and its
            // closure runs on the caller.
            ("dodges", "thread::scope"),
            ("dodges", "std::fs::read"),
            // A local function named like an exemption is no exemption.
            ("dodges", "std::fs::read"),
            ("dodges", "std::fs::read"),
            ("dodges", "std::fs::read"),
            // `run` is exempt only as ProcessControl's own, on `self`.
            ("dodges", "std::fs::read"),
            ("dodges", "std::fs::read"),
        ])
    );

    // The executor pool's own `blocking` is a real wrapper, there and
    // wherever it is imported.
    let pool = "async fn start(path: &Path) { blocking(move || std::fs::read(path)).await; }";
    assert!(found_in("src/bun/reusable_executor/pool.rs", pool).is_empty());
    let importer = "use crate::bun::reusable_executor::pool::blocking;\n\
                    async fn go(path: &Path) { blocking(move || std::fs::read(path)).await; }";
    assert!(found_in("src/bun/other.rs", importer).is_empty());
    let parent = "async fn go(path: &Path) { super::blocking(move || std::fs::read(path)).await; }";
    assert!(found_in("src/bun/reusable_executor/pool/child.rs", parent).is_empty());
}

/// A sync function that does blocking I/O makes every caller blocking,
/// however many calls deep the I/O is (#1860).
#[test]
fn the_rule_follows_sync_helpers_that_block() {
    let source = r#"
        use std::fs;
        fn bottom(path: &Path) -> Vec<u8> { fs::read(path).unwrap() }
        fn middle(path: &Path) -> Vec<u8> { bottom(path) }
        fn pure(x: u32) -> u32 { x + 1 }
        fn offloaded(path: PathBuf) { std::thread::spawn(move || std::fs::read(path)); }
        fn cheap() {
            // BLOCKING-OK: a sysfs read, answered from memory
            std::fs::read("/sys/x");
        }
        fn makes_future(path: PathBuf) -> impl Future {
            async move { std::fs::read(path) }
        }
        struct Store;
        impl Store {
            fn load(path: &Path) -> Vec<u8> { Self::read_raw(path) }
            fn read_raw(path: &Path) -> Vec<u8> { std::fs::read(path).unwrap() }
            fn stored(path: &Path) -> Vec<u8> { Store::read_raw(path) }
            async fn open(path: &Path) { Self::load(path); }
        }
        mod inner {
            pub fn remove(path: &Path) { std::fs::remove_file(path); }
        }
        async fn reads(path: &Path) {
            middle(path);
            self::middle(path);
            Store::load(path);
            Store::stored(path);
            inner::remove(path);
            pure(1);
            offloaded(path.into());
            cheap();
            tokio::task::spawn_blocking(move || middle(&path)).await;
            // BLOCKING-OK: tagged at the call
            middle(path);
        }
    "#;
    assert_eq!(
        found_in("src/x.rs", source),
        pairs(&[
            ("makes_future", "std::fs::read"),
            ("open", "Self::load"),
            ("reads", "middle"),
            ("reads", "self::middle"),
            ("reads", "Store::load"),
            ("reads", "Store::stored"),
            ("reads", "inner::remove"),
        ])
    );

    // A thread sleep blocks the worker as surely as a disk does: a retry
    // loop's `std::thread::sleep` once hid behind a "cgroupfs" tag.
    let sleeper = "fn nap() { std::thread::sleep(PAUSE); }\n\
                   async fn go() { nap(); std::thread::sleep(PAUSE); tokio::time::sleep(PAUSE).await; }";
    assert_eq!(
        found_in("src/x.rs", sleeper),
        pairs(&[("go", "nap"), ("go", "std::thread::sleep")])
    );

    // A tag inside a helper clears its callers, the way the CLI's context
    // read is cleared, and nothing else: a direct call beside it is still
    // flagged.
    let cli = r#"
        fn default_local() -> Client {
            // BLOCKING-OK: relish CLI: a one-shot read before any long-lived work
            let context = std::fs::read_to_string(path());
            Client::from(context)
        }
        async fn run() {
            let client = default_local();
            let manifest = std::fs::read_to_string("app.toml");
        }
    "#;
    assert_eq!(
        found_in("src/relish/x.rs", cli),
        pairs(&[("run", "std::fs::read_to_string")])
    );
}

/// The fixed point runs over the whole crate, through each file's `use`
/// items.
#[test]
fn the_rule_follows_sync_helpers_across_files() {
    let store = r#"
        pub fn load(path: &Path) -> Vec<u8> { read_raw(path) }
        fn read_raw(path: &Path) -> Vec<u8> { std::fs::read(path).unwrap() }
        pub struct Journal;
        impl Journal {
            pub fn open(path: &Path) -> Self { load(path); Journal }
        }
    "#;
    let caller = r#"
        use crate::store::{self, Journal};
        use crate::store::load as fetch;
        async fn go(path: &Path) {
            store::load(path);
            fetch(path);
            crate::store::load(path);
            Journal::open(path);
        }
    "#;
    let hits: Vec<(String, String)> = sites_in_files(&[
        ("src/store.rs".to_string(), store.to_string()),
        ("src/bun/caller.rs".to_string(), caller.to_string()),
    ])
    .into_iter()
    .map(|hit| (hit.site.function, hit.site.call))
    .collect();
    assert_eq!(
        hits,
        pairs(&[
            ("go", "store::load"),
            ("go", "fetch"),
            ("go", "crate::store::load"),
            ("go", "Journal::open"),
        ])
    );
}

/// `select!` arms and `join!` futures are code the task runs, and so are
/// other macros' arguments (#1860).
#[test]
fn the_rule_looks_inside_macro_arguments() {
    let source = r#"
        async fn turns(path: &Path) {
            tokio::select! {
                biased;
                _ = shutdown.cancelled() => { std::fs::remove_file(path); }
                Some(x) = rx.recv(), if std::fs::metadata(path).is_ok() => std::fs::write(path, x),
                bytes = tokio::fs::read(path) => {}
                else => { std::fs::read(path); }
            }
            let (a, b) = tokio::join!(tokio::fs::read(path), async { std::fs::copy(path, path) });
            let message = format!("{}", std::fs::read_to_string(path).unwrap());
            // BLOCKING-OK: the tag reaches into macros too
            eprintln!("{:?}", std::fs::read_link(path));
        }
        fn sync_select(path: &Path) {
            tokio::select! { _ = never() => { std::fs::read(path); } }
        }
    "#;
    assert_eq!(
        found_in("src/x.rs", source),
        pairs(&[
            ("turns", "std::fs::remove_file"),
            ("turns", "std::fs::metadata"),
            ("turns", "std::fs::write"),
            ("turns", "std::fs::read"),
            ("turns", "std::fs::copy"),
            ("turns", "std::fs::read_to_string"),
        ])
    );
}

/// `self.method()` resolves through the enclosing `impl`, as `Self::method()`
/// does (#1887).
#[test]
fn the_rule_follows_self_method_calls() {
    let source = r#"
        struct Store { path: PathBuf }
        impl Store {
            fn read_raw(&self) -> Vec<u8> { std::fs::read(&self.path).unwrap() }
            fn load(&self) -> Vec<u8> { self.read_raw() }
            fn pure(&self) -> u32 { 1 }
            async fn fetch(&self) -> u32 { 2 }
            async fn open(&self) {
                self.load();
                self.pure();
                self.fetch().await;
                tokio::task::spawn_blocking(move || store.load()).await;
            }
        }
    "#;
    assert_eq!(
        found_in("src/x.rs", source),
        pairs(&[("open", "self.load()")])
    );
}

/// A file that brings its type in with `use super::*` names the same
/// `impl` the defining file does, so a `Self::` call between the split
/// impls of one type is followed (#1887).
#[test]
fn the_rule_follows_calls_between_split_impls() {
    let agent = r#"
        pub struct BunAgent;
        impl BunAgent {
            fn read_raw(&self) -> Vec<u8> { std::fs::read("x").unwrap() }
        }
    "#;
    let launch = r#"
        use super::*;
        impl BunAgent {
            async fn go(&self) {
                Self::read_raw(self);
                self.read_raw();
            }
        }
    "#;
    let hits: Vec<(String, String)> = sites_in_files(&[
        ("src/bun/agent.rs".to_string(), agent.to_string()),
        ("src/bun/agent/launch.rs".to_string(), launch.to_string()),
    ])
    .into_iter()
    .map(|hit| (hit.site.function, hit.site.call))
    .collect();
    assert_eq!(
        hits,
        pairs(&[("go", "Self::read_raw"), ("go", "self.read_raw()")])
    );
}

/// A `select!` arm whose body has a turbofish comma, and a macro with one
/// argument that isn't an expression, used to hide every call in them
/// (#1887).
#[test]
fn the_rule_reads_select_arms_and_macro_arguments_that_partly_parse() {
    let source = r#"
        async fn turns(path: &Path) {
            tokio::select! {
                _ = first() => Ok::<(), Error>(()),
                _ = second() => { std::fs::remove_file(path); }
            }
            let empty = matches!(std::fs::read(path), Ok(v) if v.is_empty());
        }
    "#;
    assert_eq!(
        found_in("src/x.rs", source),
        pairs(&[
            ("turns", "std::fs::remove_file"),
            ("turns", "std::fs::read"),
        ])
    );
}
