//! The agent loop's inline-await rule (#351), checked mechanically.
//!
//! Every `.await` a turn of `run_loop` can reach must either sit under a
//! deadline (`tokio::time::timeout`) or carry a `// LOOP-INLINE: <why>`
//! comment on the statement that awaits. The check parses `agent.rs` and its
//! submodules with `syn`, starts at `run_loop`, and follows every directly
//! awaited `self.method(..)` call into that method's body. Any other await it
//! meets is a leaf: a runtime call, a council write, a channel send, a
//! `spawn_blocking` join. A leaf without a deadline or a tag fails the test.
//!
//! What it deliberately doesn't look at: bodies of `async` blocks and
//! closures (they are spawned, or awaited by a leaf that is itself tagged),
//! `#[cfg(test)]` code, and awaits on an in-process lock (`.lock()`,
//! `.read()`, `.write()` with no arguments), which only wait for other
//! holders of the same lock.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use syn::spanned::Spanned;
use syn::visit::Visit;

/// The comment that allowlists an inline await.
const TAG: &str = "LOOP-INLINE:";

/// Async methods a turn doesn't run: `select!` awaits them to *find* work.
const BRANCH_FUTURES: &[&str] = &["recv_snapshot"];

/// One parsed source file.
struct Source {
    path: PathBuf,
    lines: Vec<String>,
    file: syn::File,
}

/// An await that breaks the rule.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Violation {
    path: String,
    line: usize,
    awaited: String,
}

impl std::fmt::Display for Violation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}: {}", self.path, self.line, self.awaited)
    }
}

fn is_cfg_test(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|attr| {
        attr.path().is_ident("cfg")
            && attr
                .meta
                .require_list()
                .is_ok_and(|list| list.tokens.to_string() == "test")
    })
}

/// `impl ... BunAgent<..>` blocks' async methods, by name, across all files.
fn agent_methods(sources: &[Source]) -> HashMap<String, Vec<(usize, &syn::ImplItemFn)>> {
    let mut methods: HashMap<String, Vec<(usize, &syn::ImplItemFn)>> = HashMap::new();
    for (index, source) in sources.iter().enumerate() {
        for item in &source.file.items {
            let syn::Item::Impl(block) = item else {
                continue;
            };
            let syn::Type::Path(self_type) = block.self_ty.as_ref() else {
                continue;
            };
            let is_agent = self_type
                .path
                .segments
                .last()
                .is_some_and(|segment| segment.ident == "BunAgent");
            if !is_agent || is_cfg_test(&block.attrs) {
                continue;
            }
            for impl_item in &block.items {
                if let syn::ImplItem::Fn(method) = impl_item
                    && method.sig.asyncness.is_some()
                    && !is_cfg_test(&method.attrs)
                {
                    methods
                        .entry(method.sig.ident.to_string())
                        .or_default()
                        .push((index, method));
                }
            }
        }
    }
    methods
}

/// The agent method a directly awaited expression calls, if it's one.
fn called_method(expr: &syn::Expr) -> Option<String> {
    match expr {
        syn::Expr::MethodCall(call) => match call.receiver.as_ref() {
            syn::Expr::Path(receiver) if receiver.path.is_ident("self") => {
                Some(call.method.to_string())
            }
            _ => None,
        },
        syn::Expr::Call(call) => match call.func.as_ref() {
            syn::Expr::Path(function)
                if function.path.segments.len() == 2
                    && function.path.segments[0].ident == "Self" =>
            {
                Some(function.path.segments[1].ident.to_string())
            }
            _ => None,
        },
        _ => None,
    }
}

/// Whether an awaited expression carries its own deadline or is a lock.
fn is_bounded(expr: &syn::Expr) -> bool {
    match expr {
        syn::Expr::Call(call) => {
            match call.func.as_ref() {
                syn::Expr::Path(function) => function.path.segments.last().is_some_and(|segment| {
                    segment.ident == "timeout" || segment.ident == "timeout_at"
                }),
                _ => false,
            }
        }
        syn::Expr::MethodCall(call) => {
            call.args.is_empty()
                && matches!(call.method.to_string().as_str(), "lock" | "read" | "write")
        }
        _ => false,
    }
}

/// Walks one method body, collecting edges to other agent methods and
/// untagged leaf awaits.
struct BodyVisitor<'a> {
    source: &'a Source,
    methods: &'a HashMap<String, Vec<(usize, &'a syn::ImplItemFn)>>,
    /// Start lines of the statements enclosing the current expression.
    statements: Vec<usize>,
    calls: Vec<String>,
    violations: Vec<Violation>,
}

impl BodyVisitor<'_> {
    /// Whether the statement starting at `start` (and running to `end`), or
    /// the comment block right above it, carries a justified tag.
    fn tagged(&self, start: usize, end: usize) -> bool {
        let justified = |line: &str| {
            line.split_once(TAG)
                .is_some_and(|(_, why)| !why.trim().is_empty())
        };
        let line = |number: usize| self.source.lines.get(number - 1).map(String::as_str);
        if (start..=end).filter_map(line).any(justified) {
            return true;
        }
        let mut above = start - 1;
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
}

impl<'ast> Visit<'ast> for BodyVisitor<'_> {
    fn visit_stmt(&mut self, stmt: &'ast syn::Stmt) {
        let attrs = match stmt {
            syn::Stmt::Local(local) => local.attrs.as_slice(),
            syn::Stmt::Expr(expr, _) => expr_attrs(expr),
            syn::Stmt::Macro(mac) => mac.attrs.as_slice(),
            syn::Stmt::Item(_) => return,
        };
        if is_cfg_test(attrs) {
            return;
        }
        self.statements.push(stmt.span().start().line);
        syn::visit::visit_stmt(self, stmt);
        self.statements.pop();
    }

    fn visit_expr_await(&mut self, expr: &'ast syn::ExprAwait) {
        if is_cfg_test(&expr.attrs) {
            return;
        }
        // `async { .. }.await` runs its body inline, as part of this turn.
        if let syn::Expr::Async(block) = expr.base.as_ref() {
            self.visit_block(&block.block);
            return;
        }
        if let Some(method) = called_method(&expr.base)
            && self.methods.contains_key(&method)
        {
            self.calls.push(method);
            syn::visit::visit_expr(self, &expr.base);
            return;
        }
        if is_bounded(&expr.base) {
            return;
        }
        let end = expr.await_token.span.start().line;
        let start = self.statements.last().copied().unwrap_or(end);
        if !self.tagged(start, end) {
            let awaited = self
                .source
                .lines
                .get(end - 1)
                .map(|line| line.trim().to_string())
                .unwrap_or_default();
            self.violations.push(Violation {
                path: self.source.path.display().to_string(),
                line: end,
                awaited,
            });
        }
        syn::visit::visit_expr(self, &expr.base);
    }

    fn visit_expr_async(&mut self, _: &'ast syn::ExprAsync) {}

    fn visit_expr_closure(&mut self, _: &'ast syn::ExprClosure) {}

    fn visit_item(&mut self, _: &'ast syn::Item) {}

    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        // `select!` arms are macro tokens, not parsed expressions. The only
        // thing a turn does inside one is call agent methods, so follow those.
        use proc_macro2::TokenTree;
        let tokens = flatten(mac.tokens.clone());
        for window in tokens.windows(4) {
            if let [
                TokenTree::Ident(this),
                TokenTree::Punct(dot),
                TokenTree::Ident(method),
                TokenTree::Group(_),
            ] = window
                && this == "self"
                && dot.as_char() == '.'
                && self.methods.contains_key(&method.to_string())
            {
                self.calls.push(method.to_string());
            }
        }
    }
}

/// Attributes on the expression kinds a statement can start with.
fn expr_attrs(expr: &syn::Expr) -> &[syn::Attribute] {
    match expr {
        syn::Expr::Await(inner) => &inner.attrs,
        syn::Expr::If(inner) => &inner.attrs,
        syn::Expr::Call(inner) => &inner.attrs,
        syn::Expr::MethodCall(inner) => &inner.attrs,
        syn::Expr::Block(inner) => &inner.attrs,
        syn::Expr::Match(inner) => &inner.attrs,
        syn::Expr::Assign(inner) => &inner.attrs,
        syn::Expr::Macro(inner) => &inner.attrs,
        syn::Expr::Try(inner) => &inner.attrs,
        _ => &[],
    }
}

fn flatten(stream: proc_macro2::TokenStream) -> Vec<proc_macro2::TokenTree> {
    let mut tokens = Vec::new();
    for token in stream {
        match token {
            proc_macro2::TokenTree::Group(ref group)
                if group.delimiter() != proc_macro2::Delimiter::Parenthesis =>
            {
                tokens.extend(flatten(group.stream()));
            }
            other => tokens.push(other),
        }
    }
    tokens
}

/// Every await reachable from `run_loop` that has neither a deadline nor a tag.
fn violations(sources: &[Source]) -> Vec<Violation> {
    let methods = agent_methods(sources);
    let mut seen = BTreeSet::new();
    let mut queue = vec!["run_loop".to_string()];
    let mut found = BTreeSet::new();
    while let Some(name) = queue.pop() {
        if BRANCH_FUTURES.contains(&name.as_str()) || !seen.insert(name.clone()) {
            continue;
        }
        for (index, method) in methods.get(&name).into_iter().flatten() {
            let mut visitor = BodyVisitor {
                source: &sources[*index],
                methods: &methods,
                statements: Vec::new(),
                calls: Vec::new(),
                violations: Vec::new(),
            };
            visitor.visit_block(&method.block);
            queue.extend(visitor.calls);
            found.extend(visitor.violations);
        }
    }
    found.into_iter().collect()
}

fn parse(path: PathBuf, text: &str) -> Source {
    let file = syn::parse_file(text)
        .unwrap_or_else(|error| panic!("{} does not parse: {error}", path.display()));
    Source {
        path,
        lines: text.lines().map(str::to_string).collect(),
        file,
    }
}

fn agent_sources() -> Vec<Source> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut paths = vec![PathBuf::from("src/bun/agent.rs")];
    let mut submodules: Vec<PathBuf> = std::fs::read_dir(root.join("src/bun/agent"))
        .unwrap()
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().is_some_and(|extension| extension == "rs"))
        .map(|path| path.strip_prefix(root).unwrap().to_path_buf())
        .collect();
    submodules.sort();
    paths.extend(submodules);
    let sources: Vec<Source> = paths
        .into_iter()
        .map(|path| {
            let text = std::fs::read_to_string(root.join(&path)).unwrap();
            parse(path, &text)
        })
        .collect();
    // The agent's methods live in its child modules. One the walk can't
    // read (a nested `agent/x/mod.rs`, say) would drop its awaits silently.
    for item in &sources[0].file.items {
        if let syn::Item::Mod(module) = item
            && module.content.is_none()
            && !is_cfg_test(&module.attrs)
        {
            let file = format!("src/bun/agent/{}.rs", module.ident);
            assert!(
                sources.iter().any(|source| source.path == Path::new(&file)),
                "agent module `{}` isn't in {file}, so the loop rule can't check it",
                module.ident
            );
        }
    }
    sources
}

#[test]
fn every_inline_await_on_the_agent_loop_has_a_deadline_or_a_reason() {
    let violations = violations(&agent_sources());
    assert!(
        violations.is_empty(),
        "{} awaits on the agent loop have no deadline and no `// {TAG} <why>` comment. \
         Move the work off the loop, wrap it in `tokio::time::timeout`, or say why it \
         may run inline (see the rule in src/bun/agent.rs's module doc):\n{}",
        violations.len(),
        violations
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    );
}

#[test]
fn the_rule_follows_agent_methods_and_flags_only_untagged_leaves() {
    let source = r#"
        impl<G: Grill> BunAgent<G> {
            async fn run_loop(&mut self) {
                loop {
                    tokio::select! {
                        Some(cmd) = self.command_rx.recv() => { self.handle(cmd).await; }
                    }
                }
            }
            async fn handle(&mut self, cmd: Command) {
                self.grill.state(&id).await;
                // LOOP-INLINE: an fsync'd persist, bounded by the harness
                persist(&dir).await;
                tokio::time::timeout(LIMIT, self.grill.kill(&id)).await;
                let _guard = self.lock.lock().await;
                tokio::spawn(async move { never_flagged().await });
                // LOOP-INLINE:
                self.council.write(request).await;
                #[cfg(test)]
                self.stalls.hold(Stall::Persist).await;
                self.helper().await;
                let result = async { self.grill.start(&id).await }.await;
            }
            async fn helper(&self) {
                self.grill.logs(&id).await;
            }
            async fn unreachable(&self) {
                self.grill.exec(&id).await;
            }
        }
    "#;
    let found = violations(&[parse(PathBuf::from("agent.rs"), source)]);
    let lines: Vec<(usize, &str)> = found
        .iter()
        .map(|violation| (violation.line, violation.awaited.as_str()))
        .collect();
    assert_eq!(
        lines,
        vec![
            (11, "self.grill.state(&id).await;"),
            // A tag with no reason doesn't count.
            (18, "self.council.write(request).await;"),
            // An async block awaited in place is inline code.
            (
                22,
                "let result = async { self.grill.start(&id).await }.await;"
            ),
            (25, "self.grill.logs(&id).await;"),
        ]
    );
}
