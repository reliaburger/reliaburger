//! The expressions inside a macro call, for the source rules that walk
//! the crate with `syn` (#1860).
//!
//! `syn` parses a macro call's arguments no further than a token stream:
//! it can't know what grammar the macro expects. So a rule that visits
//! only parsed expressions never sees the code in a `tokio::select!` arm,
//! a `tokio::join!` argument or a `format!` argument. This module parses
//! the three shapes the crate's async code uses:
//!
//! - `select!`: arms of `pattern = future, if condition => body`, and
//!   `else => body`, after an optional `biased;`;
//! - `join!` and `try_join!`: futures separated by commas, all awaited;
//! - anything else: arguments separated by commas, each one that parses
//!   as an expression (`format!`, `assert!`, `vec![a, b]`, the first
//!   argument of `matches!(x, Ok(v) if v)`). A macro none of whose
//!   arguments do (`vec![x; n]`, `json!({..})`, `macro_rules!`) gives none.
//!
//! A comma at the top level isn't always a separator: `Ok::<(), E>(())`
//! has one inside its turbofish, which no bracket group holds. So a piece
//! that doesn't parse at its first comma is tried with the next one too
//! (#1887).
//!
//! Tokens keep their source spans through the reparse, so a rule can still
//! name the line a call is on and read the comments above it.

use proc_macro2::{Delimiter, Spacing, TokenStream, TokenTree};
use syn::parse::{ParseStream, Parser};

/// One arm of a `select!`.
pub(crate) struct SelectArm {
    /// The future the arm races, or `None` for the `else` arm.
    pub(crate) future: Option<syn::Expr>,
    /// The arm's `if` precondition, evaluated before anything is polled.
    pub(crate) condition: Option<syn::Expr>,
    /// What runs when the arm wins.
    pub(crate) body: syn::Expr,
}

/// What a macro call's arguments are, as far as they parse.
pub(crate) enum MacroArgs {
    /// A `select!`'s arms.
    Select(Vec<SelectArm>),
    /// A `join!`'s or `try_join!`'s futures, which the macro awaits.
    Join(Vec<syn::Expr>),
    /// Any other macro's comma-separated expressions.
    Other(Vec<syn::Expr>),
}

/// The expressions in `mac`'s arguments. An arm or a list that doesn't
/// parse is left out rather than failing: a rule sees less of that macro,
/// as it saw none of any macro before.
pub(crate) fn macro_args(mac: &syn::Macro) -> MacroArgs {
    let name = mac
        .path
        .segments
        .last()
        .map(|segment| segment.ident.to_string())
        .unwrap_or_default();
    match name.as_str() {
        "select" => MacroArgs::Select(select_arms(mac.tokens.clone())),
        "join" | "try_join" => MacroArgs::Join(comma_separated(skip_biased(mac.tokens.clone()))),
        "macro_rules" => MacroArgs::Other(Vec::new()),
        _ => MacroArgs::Other(comma_separated(mac.tokens.clone())),
    }
}

/// The expressions in `tokens`, separated by commas, as far as each
/// parses. A piece that never parses is skipped up to its first comma,
/// so one argument that isn't an expression doesn't hide the others.
fn comma_separated(tokens: TokenStream) -> Vec<syn::Expr> {
    let tokens: Vec<TokenTree> = tokens.into_iter().collect();
    let mut expressions = Vec::new();
    let mut start = 0;
    while start < tokens.len() {
        let rest = &tokens[start..];
        match first_expression(rest) {
            Some((expression, end)) => {
                expressions.push(expression);
                start += end + 1;
            }
            None => start += rest.iter().position(is_comma).unwrap_or(rest.len()) + 1,
        }
    }
    expressions
}

/// The shortest run of `tokens`, ending at a top-level comma or at the end,
/// that parses as an expression, and where it ends.
fn first_expression(tokens: &[TokenTree]) -> Option<(syn::Expr, usize)> {
    let ends = (0..tokens.len())
        .filter(|&index| is_comma(&tokens[index]))
        .chain(std::iter::once(tokens.len()));
    for end in ends {
        let candidate: TokenStream = tokens[..end].iter().cloned().collect();
        if let Ok(expression) = syn::parse2::<syn::Expr>(candidate) {
            return Some((expression, end));
        }
    }
    None
}

/// `tokens` without a leading `biased;`.
fn skip_biased(tokens: TokenStream) -> TokenStream {
    let tokens: Vec<TokenTree> = tokens.into_iter().collect();
    match tokens.as_slice() {
        [
            TokenTree::Ident(biased),
            TokenTree::Punct(semicolon),
            rest @ ..,
        ] if biased == "biased" && semicolon.as_char() == ';' => rest.iter().cloned().collect(),
        _ => tokens.into_iter().collect(),
    }
}

/// Whether `tokens` start with `=>`.
fn starts_with_arrow(tokens: &[TokenTree]) -> bool {
    matches!(
        tokens,
        [TokenTree::Punct(equals), TokenTree::Punct(greater), ..]
            if equals.as_char() == '='
                && equals.spacing() == Spacing::Joint
                && greater.as_char() == '>'
    )
}

fn is_comma(token: &TokenTree) -> bool {
    matches!(token, TokenTree::Punct(comma) if comma.as_char() == ',')
}

/// A `select!`'s arms. A group (`(..)`, `[..]`, `{..}`) is one token tree,
/// so walking the top-level tokens splits at depth 0 without counting
/// brackets: each `=>` ends an arm's head, and the body runs to the first
/// comma after which it parses, or is one `{ .. }` block with an optional
/// comma after it.
fn select_arms(tokens: TokenStream) -> Vec<SelectArm> {
    let tokens: Vec<TokenTree> = skip_biased(tokens).into_iter().collect();
    let mut rest = tokens.as_slice();
    let mut arms = Vec::new();
    while !rest.is_empty() {
        let Some(arrow) = (0..rest.len()).find(|&index| starts_with_arrow(&rest[index..])) else {
            break;
        };
        let head = &rest[..arrow];
        let after = &rest[arrow + 2..];
        let body_end = match after.first() {
            Some(TokenTree::Group(group)) if group.delimiter() == Delimiter::Brace => 1,
            _ => first_expression(after).map_or_else(
                || after.iter().position(is_comma).unwrap_or(after.len()),
                |(_, end)| end,
            ),
        };
        let body = &after[..body_end];
        rest = &after[body_end..];
        if rest.first().is_some_and(is_comma) {
            rest = &rest[1..];
        }
        if let Some(arm) = select_arm(head, body) {
            arms.push(arm);
        }
    }
    arms
}

fn select_arm(head: &[TokenTree], body: &[TokenTree]) -> Option<SelectArm> {
    let body = syn::parse2::<syn::Expr>(body.iter().cloned().collect()).ok()?;
    if matches!(head, [TokenTree::Ident(word)] if word == "else") {
        return Some(SelectArm {
            future: None,
            condition: None,
            body,
        });
    }
    // `|input: ParseStream| ..` is a closure, and syn's `Parser` trait is
    // implemented for any closure of that shape, so `.parse2` runs it over
    // the head's tokens.
    let head_parser = |input: ParseStream| {
        syn::Pat::parse_multi_with_leading_vert(input)?;
        input.parse::<syn::Token![=]>()?;
        let future: syn::Expr = input.parse()?;
        let mut condition = None;
        if input.peek(syn::Token![,]) {
            input.parse::<syn::Token![,]>()?;
            input.parse::<syn::Token![if]>()?;
            condition = Some(input.parse::<syn::Expr>()?);
        }
        Ok((future, condition))
    };
    let (future, condition) = head_parser.parse2(head.iter().cloned().collect()).ok()?;
    Some(SelectArm {
        future: Some(future),
        condition,
        body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use quote::ToTokens;

    fn text(expr: &syn::Expr) -> String {
        expr.to_token_stream().to_string()
    }

    fn mac(source: &str) -> syn::Macro {
        syn::parse_str(source).unwrap()
    }

    #[test]
    fn select_arms_split_at_depth_zero() {
        let MacroArgs::Select(arms) = macro_args(&mac(r#"tokio::select! {
            biased;
            _ = shutdown.cancelled() => { stop(a, b); }
            Some(x) = rx.recv(), if ready(a, b) => handle(x, y),
            (a, b) = pair() => {}
            else => last()
        }"#)) else {
            panic!("not a select");
        };
        let shapes: Vec<(Option<String>, Option<String>, String)> = arms
            .iter()
            .map(|arm| {
                (
                    arm.future.as_ref().map(text),
                    arm.condition.as_ref().map(text),
                    text(&arm.body),
                )
            })
            .collect();
        assert_eq!(
            shapes,
            vec![
                (
                    Some("shutdown . cancelled ()".to_string()),
                    None,
                    "{ stop (a , b) ; }".to_string()
                ),
                (
                    Some("rx . recv ()".to_string()),
                    Some("ready (a , b)".to_string()),
                    "handle (x , y)".to_string()
                ),
                (Some("pair ()".to_string()), None, "{ }".to_string()),
                (None, None, "last ()".to_string()),
            ]
        );
    }

    #[test]
    fn join_and_other_macros_give_their_comma_separated_expressions() {
        let MacroArgs::Join(futures) = macro_args(&mac("tokio::join!(read(a), async { b })"))
        else {
            panic!("not a join");
        };
        assert_eq!(
            futures.iter().map(text).collect::<Vec<_>>(),
            vec!["read (a)", "async { b }"]
        );
        let MacroArgs::Other(args) = macro_args(&mac(r#"format!("{} {x}", load(a), x = 1)"#))
        else {
            panic!("not another macro");
        };
        assert_eq!(args.len(), 3);
        let MacroArgs::Other(args) = macro_args(&mac("vec![0u8; size]")) else {
            panic!("not another macro");
        };
        assert!(args.is_empty());
    }
}
