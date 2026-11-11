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
//! - a crate helper in [`BLOCKING_HELPERS`] that does those itself.
//!
//! "Isn't awaited" is what tells `std::fs` from `tokio::fs`: the tokio calls
//! are futures, so a call followed by `.await` is never blocking.
//!
//! A call passes inside a closure handed to `spawn_blocking`,
//! `block_in_place` or a thread `spawn`, or when its statement (or the
//! comment block above it) says `// BLOCKING-OK: <why>`.
//!
//! What it doesn't see: a call into a synchronous function that does the
//! I/O itself, unless that function is listed in [`BLOCKING_HELPERS`];
//! code inside macro arguments (`select!`, `format!`); and `#[cfg(test)]`
//! code. The sites that were already there when the rule arrived are in
//! `blocking_rule_baseline.json`. That list may only shrink.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use syn::spanned::Spanned;
use syn::visit::Visit;

/// The comment that allows a blocking call in async code.
const TAG: &str = "BLOCKING-OK:";

/// Crate functions that do blocking file I/O themselves, so a direct call
/// from async code is as blocking as the I/O.
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

/// Calls whose closure argument runs off the async runtime: tokio's and
/// std's, and the crate's own wrappers around `spawn_blocking`
/// (`ProcessControl::run`, the executor pool's `blocking`, the Raft log's
/// `run_blocking`).
const OFF_RUNTIME_CALLS: &[&str] = &[
    "spawn_blocking",
    "block_in_place",
    "spawn",
    "scope",
    "run",
    "blocking",
    "run_blocking",
];

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

fn last_segment(path: &syn::Path) -> String {
    path.segments
        .last()
        .map(|segment| segment.ident.to_string())
        .unwrap_or_default()
}

fn path_text(path: &syn::Path) -> String {
    path.segments
        .iter()
        .map(|segment| segment.ident.to_string())
        .collect::<Vec<_>>()
        .join("::")
}

/// What a call names, if it is a blocking file call.
fn blocking_path_call(function: &syn::Path) -> Option<String> {
    let segments: Vec<String> = function
        .segments
        .iter()
        .map(|segment| segment.ident.to_string())
        .collect();
    if segments.first().is_some_and(|first| first == "tokio") {
        return None;
    }
    let last = segments.last()?.as_str();
    let text = path_text(function);
    let in_fs = segments.len() >= 2 && segments[segments.len() - 2] == "fs";
    let file_open = segments.len() >= 2
        && segments[segments.len() - 2] == "File"
        && matches!(last, "open" | "create");
    let nix_call = NIX_FILE_CALLS.contains(&last)
        && (segments.len() == 1 || segments.iter().any(|segment| segment == "nix"));
    let helper = BLOCKING_HELPERS.contains(&last);
    (in_fs || file_open || nix_call || helper).then_some(text)
}

/// Whether a method chain starts at `OpenOptions::new()`.
fn starts_at_open_options(expr: &syn::Expr) -> bool {
    match expr {
        syn::Expr::MethodCall(call) => starts_at_open_options(&call.receiver),
        syn::Expr::Call(call) => match call.func.as_ref() {
            syn::Expr::Path(function) => {
                let text = path_text(&function.path);
                text.ends_with("OpenOptions::new") && !text.starts_with("tokio")
            }
            _ => false,
        },
        syn::Expr::Paren(inner) => starts_at_open_options(&inner.expr),
        _ => false,
    }
}

/// Walks one file, collecting blocking calls in async code.
struct FileVisitor<'a> {
    path: &'a str,
    lines: &'a [String],
    /// The enclosing functions, innermost last.
    functions: Vec<String>,
    /// Whether the code being visited runs on the async runtime.
    in_async: bool,
    /// Start lines of the enclosing statements.
    statements: Vec<usize>,
    /// The call directly under an `.await`, which is never blocking.
    awaited: Option<proc_macro2::Span>,
    found: Vec<(Site, usize)>,
}

impl FileVisitor<'_> {
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

    fn record(&mut self, call: String, span: proc_macro2::Span) {
        if !self.in_async {
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
        let function = self.functions.join("::");
        self.found.push((
            Site {
                path: self.path.to_string(),
                function,
                call,
            },
            end,
        ));
    }

    fn with_function(&mut self, name: String, is_async: bool, visit: impl FnOnce(&mut Self)) {
        let outer = self.in_async;
        self.functions.push(name);
        self.in_async = is_async;
        visit(self);
        self.in_async = outer;
        self.functions.pop();
    }

    /// Visit closure arguments of an off-runtime call as synchronous code.
    fn visit_args_off_runtime<'ast>(
        &mut self,
        args: impl Iterator<Item = &'ast syn::Expr>,
        off_runtime: bool,
    ) {
        for arg in args {
            if off_runtime
                && matches!(arg, syn::Expr::Closure(closure) if closure.asyncness.is_none())
            {
                let outer = self.in_async;
                self.in_async = false;
                self.visit_expr(arg);
                self.in_async = outer;
            } else {
                self.visit_expr(arg);
            }
        }
    }
}

impl<'ast> Visit<'ast> for FileVisitor<'_> {
    fn visit_item_fn(&mut self, item: &'ast syn::ItemFn) {
        if is_cfg_test(&item.attrs) {
            return;
        }
        let is_async = item.sig.asyncness.is_some();
        self.with_function(item.sig.ident.to_string(), is_async, |this| {
            this.visit_block(&item.block);
        });
    }

    fn visit_impl_item_fn(&mut self, item: &'ast syn::ImplItemFn) {
        if is_cfg_test(&item.attrs) {
            return;
        }
        let is_async = item.sig.asyncness.is_some();
        self.with_function(item.sig.ident.to_string(), is_async, |this| {
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
        self.with_function(item.sig.ident.to_string(), is_async, |this| {
            this.visit_block(block);
        });
    }

    fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
        if is_cfg_test(&item.attrs) {
            return;
        }
        syn::visit::visit_item_mod(self, item);
    }

    fn visit_item_impl(&mut self, item: &'ast syn::ItemImpl) {
        if is_cfg_test(&item.attrs) {
            return;
        }
        syn::visit::visit_item_impl(self, item);
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
        let outer = self.in_async;
        self.in_async = true;
        syn::visit::visit_expr_async(self, block);
        self.in_async = outer;
    }

    fn visit_expr_closure(&mut self, closure: &'ast syn::ExprClosure) {
        if closure.asyncness.is_some() {
            let outer = self.in_async;
            self.in_async = true;
            syn::visit::visit_expr_closure(self, closure);
            self.in_async = outer;
        } else {
            // A synchronous closure in async code usually runs inline (an
            // iterator adapter, say), so it inherits its context.
            syn::visit::visit_expr_closure(self, closure);
        }
    }

    fn visit_expr_await(&mut self, expr: &'ast syn::ExprAwait) {
        let outer = self.awaited;
        self.awaited = match expr.base.as_ref() {
            syn::Expr::Call(call) => Some(call.span()),
            syn::Expr::MethodCall(call) => Some(call.span()),
            _ => None,
        };
        self.visit_expr(&expr.base);
        self.awaited = outer;
    }

    fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
        let awaited = self.awaited.take();
        let mut off_runtime = false;
        if let syn::Expr::Path(function) = call.func.as_ref() {
            off_runtime = OFF_RUNTIME_CALLS.contains(&last_segment(&function.path).as_str());
            if let Some(name) = blocking_path_call(&function.path) {
                self.awaited = awaited;
                self.record(name, call.span());
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
            || (method == "open" && starts_at_open_options(&call.receiver));
        if blocking {
            self.awaited = awaited;
            self.record(format!(".{method}()"), call.span());
            self.awaited = None;
        }
        self.visit_expr(&call.receiver);
        let off_runtime = OFF_RUNTIME_CALLS.contains(&method.as_str());
        self.visit_args_off_runtime(call.args.iter(), off_runtime);
    }

    fn visit_macro(&mut self, _: &'ast syn::Macro) {}
}

/// Every blocking call in async code in one parsed file, with its line.
fn sites_in(path: &str, text: &str) -> Vec<(Site, usize)> {
    let file =
        syn::parse_file(text).unwrap_or_else(|error| panic!("{path} does not parse: {error}"));
    let lines: Vec<String> = text.lines().map(str::to_string).collect();
    let mut visitor = FileVisitor {
        path,
        lines: &lines,
        functions: Vec::new(),
        in_async: false,
        statements: Vec::new(),
        awaited: None,
        found: Vec::new(),
    };
    visitor.visit_file(&file);
    visitor.found
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

/// How many of each site the whole crate has.
fn crate_sites() -> (BTreeMap<Site, usize>, BTreeMap<Site, Vec<usize>>) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut counts = BTreeMap::new();
    let mut lines: BTreeMap<Site, Vec<usize>> = BTreeMap::new();
    for file in source_files(root) {
        let relative = file.strip_prefix(root).unwrap().display().to_string();
        let text = std::fs::read_to_string(&file).unwrap();
        for (site, line) in sites_in(&relative, &text) {
            *counts.entry(site.clone()).or_default() += 1;
            lines.entry(site).or_default().push(line);
        }
    }
    (counts, lines)
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
    let (found, lines) = crate_sites();
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
        .map(|(site, _)| format!("{site} (line {:?})", lines[site]))
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
    let found: Vec<(usize, String, String)> = sites_in("x.rs", source)
        .into_iter()
        .map(|(site, line)| (line, site.function, site.call))
        .collect();
    let expected: Vec<(usize, &str, &str)> = vec![
        (3, "reads", "std::fs::read"),
        (6, "reads", "File::open"),
        (7, "reads", ".open()"),
        (8, "reads", ".sync_all()"),
        (10, "reads", "data_start"),
        (11, "reads", "nix::unistd::lseek"),
        // A tag with no reason doesn't count.
        (16, "reads", "std::fs::metadata"),
        // An iterator's closure runs inline.
        (17, "reads", "std::fs::read"),
        (19, "reads", "std::fs::write"),
        (27, "Store::method", "read_retained"),
    ]
    .into_iter()
    .collect();
    let expected: Vec<(usize, String, String)> = expected
        .into_iter()
        .map(|(line, function, call)| (line, function.replace("Store::", ""), call.to_string()))
        .collect();
    assert_eq!(found, expected);
}
