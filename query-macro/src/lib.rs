//! Compile-time checked inline queries for Unionid Rust applications.
//!
//! `queries!` binds one or more inline Unionid operations against an explicit
//! declarative schema, then expands to the same typed Rust bindings produced by
//! `unionid query rust --dir`.

use std::path::{Path, PathBuf};

use proc_macro::TokenStream;
use proc_macro2::{Delimiter, LineColumn, Span, TokenStream as TokenStream2, TokenTree};
use quote::quote;
use syn::parse::{Parse, ParseStream};
use syn::{Ident, LitStr, Result, braced, parse_macro_input};

mod keyword {
    syn::custom_keyword!(schema);
    syn::custom_keyword!(query);
}

struct QueriesInput {
    schema: LitStr,
    queries: Vec<InlineQuery>,
}

struct InlineQuery {
    name: Ident,
    source: String,
}

impl Parse for QueriesInput {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        input.parse::<keyword::schema>()?;
        let schema = input.parse::<LitStr>()?;
        let mut queries = Vec::new();
        while !input.is_empty() {
            input.parse::<keyword::query>()?;
            let name = input.parse::<Ident>()?;
            let content;
            let braces = braced!(content in input);
            let tokens = content.parse::<TokenStream2>()?;
            let source = source_from_tokens(&tokens, braces.span.join())?;
            if source.trim().is_empty() {
                return Err(syn::Error::new(name.span(), "query body cannot be empty"));
            }
            queries.push(InlineQuery { name, source });
        }
        if queries.is_empty() {
            return Err(syn::Error::new(
                schema.span(),
                "expected at least one `query name { ... }` entry",
            ));
        }
        Ok(Self { schema, queries })
    }
}

fn source_from_tokens(tokens: &TokenStream2, fallback_span: Span) -> Result<String> {
    if tokens.is_empty() {
        return Err(syn::Error::new(
            fallback_span,
            "inline query body cannot be empty",
        ));
    }
    render_stream(tokens).ok_or_else(|| {
        syn::Error::new(
            fallback_span,
            "cannot recover inline query token positions; write the query directly in this macro invocation",
        )
    })
}

fn render_stream(tokens: &TokenStream2) -> Option<String> {
    let mut output = String::new();
    let mut previous_end = None;
    for tree in tokens.clone() {
        let start = tree.span().start();
        if start.line == 0 {
            return None;
        }
        if let Some(end) = previous_end {
            write_gap(&mut output, end, start);
        }
        render_tree(&mut output, tree.clone())?;
        previous_end = Some(tree.span().end());
    }
    Some(output.trim().to_string())
}

fn write_gap(output: &mut String, previous: LineColumn, next: LineColumn) {
    if next.line > previous.line {
        for _ in previous.line..next.line {
            output.push('\n');
        }
    } else if next.column > previous.column {
        output.push(' ');
    }
}

fn render_tree(output: &mut String, tree: TokenTree) -> Option<()> {
    match tree {
        TokenTree::Group(group) => {
            let delimiters = match group.delimiter() {
                Delimiter::Parenthesis => Some(('(', ')')),
                Delimiter::Brace => Some(('{', '}')),
                Delimiter::Bracket => Some(('[', ']')),
                Delimiter::None => None,
            };
            if let Some((open, _)) = delimiters {
                output.push(open);
            }
            output.push_str(&render_stream(&group.stream())?);
            if let Some((_, close)) = delimiters {
                output.push(close);
            }
        }
        TokenTree::Ident(ident) => output.push_str(&ident.to_string()),
        TokenTree::Punct(punct) => output.push(punct.as_char()),
        TokenTree::Literal(literal) => output.push_str(&literal.to_string()),
    }
    Some(())
}

fn schema_path(path: &LitStr) -> Result<PathBuf> {
    let relative = PathBuf::from(path.value());
    if relative.as_os_str().is_empty() || relative.is_absolute() {
        return Err(syn::Error::new(
            path.span(),
            "schema path must be a non-empty path relative to CARGO_MANIFEST_DIR",
        ));
    }
    let manifest = std::env::var_os("CARGO_MANIFEST_DIR").ok_or_else(|| {
        syn::Error::new(
            path.span(),
            "CARGO_MANIFEST_DIR is unavailable while expanding Unionid queries",
        )
    })?;
    Ok(PathBuf::from(manifest).join(relative))
}

fn read_schema(path: &Path, span: Span) -> Result<String> {
    const MAX_SCHEMA_BYTES: u64 = 1024 * 1024;
    let metadata = std::fs::metadata(path).map_err(|error| {
        syn::Error::new(
            span,
            format!("cannot inspect schema '{}': {error}", path.display()),
        )
    })?;
    if !metadata.is_file() {
        return Err(syn::Error::new(
            span,
            format!("schema '{}' is not a regular file", path.display()),
        ));
    }
    if metadata.len() > MAX_SCHEMA_BYTES {
        return Err(syn::Error::new(
            span,
            format!("schema '{}' exceeds the 1 MiB source limit", path.display()),
        ));
    }
    std::fs::read_to_string(path).map_err(|error| {
        syn::Error::new(
            span,
            format!("cannot read UTF-8 schema '{}': {error}", path.display()),
        )
    })
}

fn expand(input: QueriesInput) -> Result<TokenStream2> {
    let path = schema_path(&input.schema)?;
    let schema = read_schema(&path, input.schema.span())?;
    let queries = input
        .queries
        .iter()
        .map(|query| (query.name.to_string(), query.source.clone()))
        .collect::<Vec<_>>();
    let generated = unionid::codegen::rust_query_bundle(&schema, &queries).map_err(|error| {
        syn::Error::new(
            input.schema.span(),
            format!("Unionid inline query binding failed: {error}"),
        )
    })?;
    let generated = generated.parse::<TokenStream2>().map_err(|error| {
        syn::Error::new(
            input.schema.span(),
            format!("Unionid generated invalid Rust tokens: {error}"),
        )
    })?;
    let tracked_path = LitStr::new(
        path.to_str().ok_or_else(|| {
            syn::Error::new(input.schema.span(), "schema path must be valid UTF-8")
        })?,
        input.schema.span(),
    );
    Ok(quote! {
        const _: &str = include_str!(#tracked_path);
        #generated
    })
}

/// Bind inline Unionid queries against a declarative schema and generate typed
/// Rust models, parameters, result rows, and execution functions.
#[proc_macro]
pub fn queries(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as QueriesInput);
    expand(input)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}
