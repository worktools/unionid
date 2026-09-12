//! Derive unionid schema declarations from Rust types.
//!
//! `#[derive(UnionidSchema)]` maps a Rust struct or enum to a unionid `type`
//! declaration and, when `#[unionid(table = "...", key = "...")]` is present, a
//! `table` declaration. The generated code implements `unionid::UnionidSchema`
//! so declarations can be collected with `unionid::SchemaBuilder`.

use std::collections::BTreeSet;

use proc_macro::TokenStream;
use proc_macro2::Span;
use quote::quote;
use syn::punctuated::Punctuated;
use syn::{
    Attribute, Data, DeriveInput, Expr, Fields, GenericArgument, Lit, LitStr, Meta,
    PathArguments, Token, Type,
};
use syn::{parse_macro_input, spanned::Spanned};

#[proc_macro_derive(UnionidSchema, attributes(unionid, serde))]
pub fn derive_unionid_schema(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    match expand(&input) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.to_compile_error().into(),
    }
}

#[derive(Default)]
struct ContainerOptions {
    table: Option<String>,
    key: Option<String>,
}

#[derive(Debug, Clone, Copy, Default)]
struct SerdeContainerOptions {
    rename_all: Option<RenameRule>,
    rename_all_fields: Option<RenameRule>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RenameRule {
    Lower,
    Upper,
    Pascal,
    Camel,
    Snake,
    ScreamingSnake,
    Kebab,
    ScreamingKebab,
}

fn expand(input: &DeriveInput) -> syn::Result<proc_macro2::TokenStream> {
    if !input.generics.params.is_empty() {
        return Err(syn::Error::new(
            input.generics.span(),
            "unionid schema types cannot be generic",
        ));
    }
    let options = container_options(&input.attrs)?;
    validate_table_options(input, &options)?;
    let serde = serde_container_options(&input.attrs, matches!(&input.data, Data::Enum(_)))?;
    let type_name = input.ident.to_string();
    let mut dependencies = BTreeSet::new();
    let mut table_key = None;
    let type_ddl = match &input.data {
        Data::Struct(data) => {
            let fields = named_fields(&data.fields, "unionid records require named fields")?;
            let mut lines = Vec::new();
            let mut schema_names = BTreeSet::new();
            for field in fields {
                let rust_name = field.ident.as_ref().expect("named field").to_string();
                let name = serde_field_name(&rust_name, &field.attrs, serde.rename_all)?;
                insert_schema_name(&mut schema_names, &name, field.span(), "record field")?;
                if options.key.as_deref() == Some(rust_name.as_str()) {
                    table_key = Some(name.clone());
                }
                let ty = field_type(&field.ty, &field.attrs, &mut dependencies)?;
                lines.push(format!("  {name} {ty},"));
            }
            format!("type {type_name} = {{\n{}\n}}", lines.join("\n"))
        }
        Data::Enum(data) => {
            let mut lines = Vec::new();
            let mut schema_names = BTreeSet::new();
            for variant in &data.variants {
                let rust_name = variant.ident.to_string();
                let variant_serde = serde_variant_options(&variant.attrs)?;
                let name = serde_name(
                    &rust_name,
                    variant_serde.rename.as_deref(),
                    serde.rename_all,
                    NameKind::Variant,
                    variant.span(),
                )?;
                insert_schema_name(&mut schema_names, &name, variant.span(), "enum variant")?;
                let rendered = match &variant.fields {
                    Fields::Unit => name.clone(),
                    Fields::Named(fields) => {
                        let mut inner = Vec::new();
                        let mut field_names = BTreeSet::new();
                        let rename_all = variant_serde.rename_all.or(serde.rename_all_fields);
                        for field in &fields.named {
                            let rust_name = field.ident.as_ref().expect("named field").to_string();
                            let field_name =
                                serde_field_name(&rust_name, &field.attrs, rename_all)?;
                            insert_schema_name(
                                &mut field_names,
                                &field_name,
                                field.span(),
                                "variant field",
                            )?;
                            let ty = field_type(&field.ty, &field.attrs, &mut dependencies)?;
                            inner.push(format!("{field_name} {ty}"));
                        }
                        format!("{name} {{{}}}", inner.join(", "))
                    }
                    Fields::Unnamed(fields) => {
                        let mut rendered = Vec::new();
                        for field in &fields.unnamed {
                            reject_unnamed_field_serde_shape(&field.attrs)?;
                            rendered.push(field_type(&field.ty, &field.attrs, &mut dependencies)?);
                        }
                        if rendered.len() == 1 {
                            format!("{name} {}", rendered[0])
                        } else {
                            format!("{name} ({})", rendered.join(", "))
                        }
                    }
                };
                lines.push(rendered);
            }
            let mut body = String::new();
            for (index, line) in lines.iter().enumerate() {
                if index == 0 {
                    body.push_str(&format!("  {line}"));
                } else {
                    body.push_str(&format!("\n  | {line}"));
                }
            }
            format!("type {type_name} =\n{body}")
        }
        Data::Union(_) => {
            return Err(syn::Error::new(
                input.span(),
                "unionid schema types must be structs or enums",
            ));
        }
    };

    let table_ddl = match &options.table {
        Some(table) => {
            let mut ddl = format!("table {table} {type_name}");
            if let Some(key) = &table_key {
                ddl.push_str(&format!("\n  key {key}"));
            }
            Some(ddl)
        }
        None => None,
    };

    let type_name_lit = LitStr::new(&type_name, Span::call_site());
    let type_ddl_lit = LitStr::new(&type_ddl, Span::call_site());
    let table_ddl = match table_ddl {
        Some(ddl) => {
            let literal = LitStr::new(&ddl, Span::call_site());
            quote!(::core::option::Option::Some(::std::string::String::from(#literal)))
        }
        None => quote!(::core::option::Option::None),
    };
    let table_name = match &options.table {
        Some(table) => {
            let literal = LitStr::new(table, Span::call_site());
            quote!(::core::option::Option::Some(#literal))
        }
        None => quote!(::core::option::Option::None),
    };
    let ident = &input.ident;
    let dependency_lits = dependencies
        .iter()
        .map(|name| LitStr::new(name, Span::call_site()));
    Ok(quote! {
        impl ::unionid::UnionidSchema for #ident {
            const UNIONID_TYPE_NAME: &'static str = #type_name_lit;

            fn unionid_type_ddl() -> ::std::string::String {
                ::std::string::String::from(#type_ddl_lit)
            }

            fn unionid_dependencies() -> ::std::vec::Vec<&'static str> {
                ::std::vec![#(#dependency_lits),*]
            }

            fn unionid_table_name() -> ::core::option::Option<&'static str> {
                #table_name
            }

            fn unionid_table_ddl() -> ::core::option::Option<::std::string::String> {
                #table_ddl
            }
        }
    })
}

fn validate_table_options(input: &DeriveInput, options: &ContainerOptions) -> syn::Result<()> {
    if options.table.is_none() && options.key.is_some() {
        return Err(syn::Error::new(input.span(), "`key` requires `table`"));
    }
    let Some(_) = options.table else {
        return Ok(());
    };
    let data = match &input.data {
        Data::Struct(data) => data,
        _ => {
            return Err(syn::Error::new(
                input.span(),
                "only a named struct can declare a unionid table",
            ));
        }
    };
    let fields = named_fields(
        &data.fields,
        "a unionid table requires a struct with named fields",
    )?;
    if let Some(key) = &options.key
        && !fields
            .iter()
            .any(|field| field.ident.as_ref().is_some_and(|ident| ident == key))
    {
        return Err(syn::Error::new(
            input.span(),
            format!("key '{key}' is not a field of '{}'", input.ident),
        ));
    }
    Ok(())
}

fn container_options(attrs: &[Attribute]) -> syn::Result<ContainerOptions> {
    let mut options = ContainerOptions::default();
    for attr in attrs {
        if !attr.path().is_ident("unionid") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("table") {
                options.table = Some(meta.value()?.parse::<LitStr>()?.value());
                Ok(())
            } else if meta.path.is_ident("key") {
                options.key = Some(meta.value()?.parse::<LitStr>()?.value());
                Ok(())
            } else {
                Err(meta.error("unknown unionid attribute; expected `table` or `key`"))
            }
        })?;
    }
    Ok(options)
}

#[derive(Debug, Clone, Default)]
struct SerdeVariantOptions {
    rename: Option<String>,
    rename_all: Option<RenameRule>,
}

#[derive(Debug, Clone, Copy)]
enum NameKind {
    Field,
    Variant,
}

fn serde_container_options(
    attrs: &[Attribute],
    is_enum: bool,
) -> syn::Result<SerdeContainerOptions> {
    let mut options = SerdeContainerOptions::default();
    for meta in serde_metas(attrs)? {
        if meta.path().is_ident("rename_all") {
            options.rename_all = Some(rename_rule(&meta)?);
        } else if meta.path().is_ident("rename_all_fields") {
            if !is_enum {
                return Err(syn::Error::new(
                    meta.span(),
                    "serde rename_all_fields is only supported on enums",
                ));
            }
            options.rename_all_fields = Some(rename_rule(&meta)?);
        } else if [
            "tag",
            "content",
            "untagged",
            "transparent",
            "from",
            "try_from",
            "into",
            "field_identifier",
            "variant_identifier",
        ]
        .iter()
        .any(|name| meta.path().is_ident(name))
        {
            return Err(unsupported_serde_shape(&meta));
        }
    }
    Ok(options)
}

fn serde_variant_options(attrs: &[Attribute]) -> syn::Result<SerdeVariantOptions> {
    let mut rename = None;
    let mut rename_all = None;
    for meta in serde_metas(attrs)? {
        if meta.path().is_ident("rename") {
            rename = Some(serde_rename(&meta)?);
        } else if meta.path().is_ident("rename_all") {
            rename_all = Some(rename_rule(&meta)?);
        } else if [
            "skip",
            "skip_serializing",
            "skip_deserializing",
            "serialize_with",
            "deserialize_with",
            "with",
            "untagged",
            "other",
        ]
        .iter()
        .any(|name| meta.path().is_ident(name))
        {
            return Err(unsupported_serde_shape(&meta));
        }
    }
    Ok(SerdeVariantOptions { rename, rename_all })
}

fn serde_field_name(
    rust_name: &str,
    attrs: &[Attribute],
    rename_all: Option<RenameRule>,
) -> syn::Result<String> {
    let mut rename = None;
    for meta in serde_metas(attrs)? {
        if meta.path().is_ident("rename") {
            rename = Some(serde_rename(&meta)?);
        } else if [
            "flatten",
            "skip",
            "skip_serializing",
            "skip_deserializing",
            "skip_serializing_if",
            "serialize_with",
            "deserialize_with",
            "with",
            "getter",
        ]
        .iter()
        .any(|name| meta.path().is_ident(name))
        {
            return Err(unsupported_serde_shape(&meta));
        }
    }
    serde_name(
        rust_name,
        rename.as_deref(),
        rename_all,
        NameKind::Field,
        attrs
            .iter()
            .find(|attribute| attribute.path().is_ident("serde"))
            .map_or_else(Span::call_site, Attribute::span),
    )
}

fn reject_unnamed_field_serde_shape(attrs: &[Attribute]) -> syn::Result<()> {
    for meta in serde_metas(attrs)? {
        if [
            "rename",
            "flatten",
            "skip",
            "skip_serializing",
            "skip_deserializing",
            "skip_serializing_if",
            "serialize_with",
            "deserialize_with",
            "with",
            "getter",
        ]
        .iter()
        .any(|name| meta.path().is_ident(name))
        {
            return Err(unsupported_serde_shape(&meta));
        }
    }
    Ok(())
}

fn serde_metas(attrs: &[Attribute]) -> syn::Result<Vec<Meta>> {
    let mut metas = Vec::new();
    for attribute in attrs {
        if !attribute.path().is_ident("serde") {
            continue;
        }
        let Meta::List(list) = &attribute.meta else {
            return Err(syn::Error::new(
                attribute.span(),
                "serde attributes must use #[serde(...)]",
            ));
        };
        metas.extend(
            list.parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)?,
        );
    }
    Ok(metas)
}

fn serde_rename(meta: &Meta) -> syn::Result<String> {
    match meta {
        Meta::NameValue(value) => lit_string(&value.value, "serde rename") ,
        Meta::List(list) => {
            let pairs = list.parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)?;
            let mut serialize = None;
            let mut deserialize = None;
            for pair in pairs {
                let Meta::NameValue(value) = pair else {
                    return Err(syn::Error::new(pair.span(), "expected serialize = \"...\" or deserialize = \"...\""));
                };
                if value.path.is_ident("serialize") {
                    serialize = Some(lit_string(&value.value, "serde serialize rename")?);
                } else if value.path.is_ident("deserialize") {
                    deserialize = Some(lit_string(&value.value, "serde deserialize rename")?);
                } else {
                    return Err(syn::Error::new(value.path.span(), "expected serialize or deserialize"));
                }
            }
            match (serialize, deserialize) {
                (Some(serialize), Some(deserialize)) if serialize == deserialize => Ok(serialize),
                _ => Err(syn::Error::new(
                    meta.span(),
                    "unionid requires the same serde rename for serialization and deserialization",
                )),
            }
        }
        Meta::Path(_) => Err(syn::Error::new(meta.span(), "serde rename needs a value")),
    }
}

fn rename_rule(meta: &Meta) -> syn::Result<RenameRule> {
    let value = serde_rename(meta)?;
    match value.as_str() {
        "lowercase" => Ok(RenameRule::Lower),
        "UPPERCASE" => Ok(RenameRule::Upper),
        "PascalCase" => Ok(RenameRule::Pascal),
        "camelCase" => Ok(RenameRule::Camel),
        "snake_case" => Ok(RenameRule::Snake),
        "SCREAMING_SNAKE_CASE" => Ok(RenameRule::ScreamingSnake),
        "kebab-case" => Ok(RenameRule::Kebab),
        "SCREAMING-KEBAB-CASE" => Ok(RenameRule::ScreamingKebab),
        _ => Err(syn::Error::new(
            meta.span(),
            format!("unsupported serde rename rule '{value}'"),
        )),
    }
}

fn lit_string(expression: &Expr, context: &str) -> syn::Result<String> {
    match expression {
        Expr::Lit(value) => match &value.lit {
            Lit::Str(value) => Ok(value.value()),
            _ => Err(syn::Error::new(value.span(), format!("{context} must be a string"))),
        },
        _ => Err(syn::Error::new(expression.span(), format!("{context} must be a string"))),
    }
}

fn serde_name(
    rust_name: &str,
    explicit: Option<&str>,
    rename_all: Option<RenameRule>,
    kind: NameKind,
    span: Span,
) -> syn::Result<String> {
    let name = explicit.map_or_else(
        || match (rename_all, kind) {
            (Some(rule), NameKind::Field) => rule.apply_to_field(rust_name),
            (Some(rule), NameKind::Variant) => rule.apply_to_variant(rust_name),
            (None, _) => rust_name.to_owned(),
        },
        str::to_owned,
    );
    let valid_variant = !matches!(kind, NameKind::Variant)
        || name.starts_with(|character: char| character.is_ascii_uppercase());
    if !valid_schema_identifier(&name) || !valid_variant {
        return Err(syn::Error::new(
            span,
            format!("serde name '{name}' is not a valid unionid {kind} name"),
        ));
    }
    Ok(name)
}

impl std::fmt::Display for NameKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Field => "field",
            Self::Variant => "variant",
        })
    }
}

fn valid_schema_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(ch) if ch.is_ascii_alphabetic() || ch == '_')
        && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

fn insert_schema_name(
    names: &mut BTreeSet<String>,
    name: &str,
    span: Span,
    kind: &str,
) -> syn::Result<()> {
    if names.insert(name.to_owned()) {
        Ok(())
    } else {
        Err(syn::Error::new(
            span,
            format!("duplicate {kind} name '{name}' after applying serde rename rules"),
        ))
    }
}

fn unsupported_serde_shape(meta: &Meta) -> syn::Error {
    let name = meta
        .path()
        .segments
        .last()
        .map_or_else(|| "attribute".to_owned(), |segment| segment.ident.to_string());
    syn::Error::new(
        meta.span(),
        format!("serde {name} changes the value shape and is not supported by UnionidSchema"),
    )
}

impl RenameRule {
    fn apply_to_variant(self, variant: &str) -> String {
        match self {
            Self::Pascal => variant.to_owned(),
            Self::Lower => variant.to_ascii_lowercase(),
            Self::Upper => variant.to_ascii_uppercase(),
            Self::Camel => lowercase_first(variant),
            Self::Snake => {
                let mut output = String::new();
                for (index, ch) in variant.char_indices() {
                    if index > 0 && ch.is_uppercase() {
                        output.push('_');
                    }
                    output.push(ch.to_ascii_lowercase());
                }
                output
            }
            Self::ScreamingSnake => {
                Self::Snake.apply_to_variant(variant).to_ascii_uppercase()
            }
            Self::Kebab => Self::Snake.apply_to_variant(variant).replace('_', "-"),
            Self::ScreamingKebab => Self::ScreamingSnake
                .apply_to_variant(variant)
                .replace('_', "-"),
        }
    }

    fn apply_to_field(self, field: &str) -> String {
        match self {
            Self::Lower | Self::Snake => field.to_owned(),
            Self::Upper => field.to_ascii_uppercase(),
            Self::Pascal => {
                let mut output = String::new();
                let mut uppercase = true;
                for ch in field.chars() {
                    if ch == '_' {
                        uppercase = true;
                    } else if uppercase {
                        output.push(ch.to_ascii_uppercase());
                        uppercase = false;
                    } else {
                        output.push(ch);
                    }
                }
                output
            }
            Self::Camel => lowercase_first(&Self::Pascal.apply_to_field(field)),
            Self::ScreamingSnake => field.to_ascii_uppercase(),
            Self::Kebab => field.replace('_', "-"),
            Self::ScreamingKebab => field.to_ascii_uppercase().replace('_', "-"),
        }
    }
}

fn lowercase_first(value: &str) -> String {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return String::new();
    };
    first.to_ascii_lowercase().to_string() + chars.as_str()
}


fn named_fields<'a>(
    fields: &'a Fields,
    message: &str,
) -> syn::Result<&'a syn::punctuated::Punctuated<syn::Field, syn::token::Comma>> {
    match fields {
        Fields::Named(named) => Ok(&named.named),
        _ => Err(syn::Error::new(fields.span(), message)),
    }
}

fn field_type(
    ty: &Type,
    attrs: &[Attribute],
    dependencies: &mut BTreeSet<String>,
) -> syn::Result<String> {
    let decimal = decimal_attribute(attrs)?;
    type_ddl(ty, decimal.as_deref(), dependencies)
}

fn decimal_attribute(attrs: &[Attribute]) -> syn::Result<Option<String>> {
    let mut decimal = None;
    for attr in attrs {
        if !attr.path().is_ident("unionid") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("decimal") {
                decimal = Some(meta.value()?.parse::<LitStr>()?.value());
                Ok(())
            } else {
                Err(meta.error("unknown unionid field attribute; expected `decimal`"))
            }
        })?;
    }
    Ok(decimal)
}

fn type_ddl(
    ty: &Type,
    decimal: Option<&str>,
    dependencies: &mut BTreeSet<String>,
) -> syn::Result<String> {
    match ty {
        Type::Path(path) => path_ddl(path, decimal, dependencies),
        Type::Tuple(tuple) => {
            if tuple.elems.len() == 1 {
                return Err(syn::Error::new(
                    tuple.span(),
                    "one-element tuples must be named unionid types",
                ));
            }
            let parts = tuple
                .elems
                .iter()
                .map(|element| type_ddl(element, decimal, dependencies))
                .collect::<syn::Result<Vec<_>>>()?;
            Ok(format!("({})", parts.join(", ")))
        }
        Type::Reference(reference) => {
            if matches!(&*reference.elem, Type::Path(path) if path.path.is_ident("str")) {
                Ok("text".to_string())
            } else {
                Err(syn::Error::new(
                    reference.span(),
                    "unsupported reference type for a unionid schema",
                ))
            }
        }
        other => Err(syn::Error::new(
            other.span(),
            "unsupported type for a unionid schema",
        )),
    }
}

fn path_ddl(
    path: &syn::TypePath,
    decimal: Option<&str>,
    dependencies: &mut BTreeSet<String>,
) -> syn::Result<String> {
    let segment = path
        .path
        .segments
        .last()
        .ok_or_else(|| syn::Error::new(path.span(), "empty type path"))?;
    let name = segment.ident.to_string();
    match &segment.arguments {
        PathArguments::None => match name.as_str() {
            "i64" | "i32" => Ok("int".to_string()),
            "f64" | "f32" => Ok("float".to_string()),
            "bool" => Ok("bool".to_string()),
            "String" | "str" => Ok("text".to_string()),
            "Uuid" => Ok("uuid".to_string()),
            "Date" => Ok("date".to_string()),
            "Timestamp" => Ok("timestamp".to_string()),
            "Duration" => Ok("duration".to_string()),
            "Bytes" => Ok("bytes".to_string()),
            "Decimal" => {
                let value = decimal.ok_or_else(|| {
                    syn::Error::new(
                        path.span(),
                        "a Decimal field needs #[unionid(decimal = \"P S\")]",
                    )
                })?;
                Ok(format!(
                    "decimal {}",
                    validated_decimal(value, path.span())?
                ))
            }
            "u8" | "u16" | "u32" | "u64" | "u128" | "usize" | "i8" | "i16" | "i128" | "isize"
            | "char" => Err(syn::Error::new(
                path.span(),
                format!("unsupported primitive type `{name}` for a unionid schema"),
            )),
            _ => {
                dependencies.insert(name.clone());
                Ok(name)
            }
        },
        PathArguments::AngleBracketed(arguments) if arguments.args.len() == 1 => {
            let inner = match &arguments.args[0] {
                GenericArgument::Type(inner) => inner,
                other => {
                    return Err(syn::Error::new(
                        other.span(),
                        "unsupported generic argument for a unionid schema",
                    ));
                }
            };
            match name.as_str() {
                "Option" => Ok(format!(
                    "option ({})",
                    type_ddl(inner, decimal, dependencies)?
                )),
                "Vec" => Ok(format!(
                    "list ({})",
                    type_ddl(inner, decimal, dependencies)?
                )),
                "Box" => type_ddl(inner, decimal, dependencies),
                _ => Err(syn::Error::new(
                    path.span(),
                    "unsupported generic type for a unionid schema",
                )),
            }
        }
        _ => Err(syn::Error::new(
            path.span(),
            "unsupported generic type for a unionid schema",
        )),
    }
}

fn validated_decimal(value: &str, span: Span) -> syn::Result<String> {
    let parts = value.split_whitespace().collect::<Vec<_>>();
    if parts.len() != 2 {
        return Err(syn::Error::new(
            span,
            "decimal must be written as \"P S\" with two integers",
        ));
    }
    let precision = parts[0]
        .parse::<u8>()
        .map_err(|_| syn::Error::new(span, "decimal precision must be an integer"))?;
    let scale = parts[1]
        .parse::<u8>()
        .map_err(|_| syn::Error::new(span, "decimal scale must be an integer"))?;
    if !(1..=38).contains(&precision) {
        return Err(syn::Error::new(span, "decimal precision must be 1..=38"));
    }
    if scale > precision {
        return Err(syn::Error::new(span, "decimal scale must be <= precision"));
    }
    Ok(format!("{precision} {scale}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expand_str(source: &str) -> syn::Result<proc_macro2::TokenStream> {
        expand(&syn::parse_str::<DeriveInput>(source).unwrap())
    }

    #[test]
    fn rejects_key_without_table() {
        let error = expand_str("#[unionid(key = \"id\")] struct T { id: i64 }").unwrap_err();
        assert!(error.to_string().contains("requires `table`"), "{error}");
    }

    #[test]
    fn rejects_table_on_enum() {
        let error = expand_str("#[unionid(table = \"t\", key = \"id\")] enum T { A }").unwrap_err();
        assert!(error.to_string().contains("named struct"), "{error}");
    }

    #[test]
    fn rejects_key_that_is_not_a_field() {
        let error = expand_str("#[unionid(table = \"t\", key = \"missing\")] struct T { id: i64 }")
            .unwrap_err();
        assert!(error.to_string().contains("not a field"), "{error}");
    }

    #[test]
    fn rejects_invalid_decimal_metadata() {
        for value in ["invalid", "12", "39 2", "2 3", "12 x"] {
            let source =
                format!("struct T {{ #[unionid(decimal = \"{value}\")] amount: Decimal }}");
            assert!(
                expand_str(&source).is_err(),
                "decimal {value:?} should fail"
            );
        }
        let source = "struct T { #[unionid(decimal = \"12 2\")] amount: Decimal }";
        assert!(expand_str(source).is_ok());
    }

    #[test]
    fn rejects_unsupported_primitives() {
        for primitive in ["u64", "usize", "char", "i128", "u8"] {
            let source = format!("struct T {{ value: {primitive} }}");
            assert!(
                expand_str(&source).is_err(),
                "{primitive} should be rejected"
            );
        }
    }

    #[test]
    fn maps_supported_types() {
        let expanded = expand_str(
            "struct T { id: i64, name: String, tags: Vec<Option<String>>, owner: Contact }",
        )
        .unwrap()
        .to_string();
        assert!(expanded.contains("list (option (text))"), "{expanded}");
    }

    #[test]
    fn rejects_serde_attributes_that_change_the_value_shape() {
        for source in [
            "#[serde(tag = \"kind\", content = \"value\")] enum T { A, B(i64) }",
            "#[serde(untagged)] enum T { A(i64), B(String) }",
            "#[serde(transparent)] struct T { value: i64 }",
            "struct T { #[serde(flatten)] nested: Nested }",
            "struct T { #[serde(skip)] value: i64 }",
            "struct T { #[serde(skip_serializing_if = \"Option::is_none\")] value: Option<i64> }",
        ] {
            let error = expand_str(source).unwrap_err();
            assert!(
                error.to_string().contains("changes the value shape"),
                "unexpected error for {source}: {error}"
            );
        }
    }

    #[test]
    fn rejects_asymmetric_or_invalid_serde_names() {
        let asymmetric = expand_str(
            "struct T { #[serde(rename(serialize = \"out\", deserialize = \"in\"))] value: i64 }",
        )
        .unwrap_err();
        assert!(asymmetric.to_string().contains("same serde rename"));

        let invalid = expand_str("#[serde(rename_all = \"kebab-case\")] struct T { some_value: i64 }")
            .unwrap_err();
        assert!(invalid.to_string().contains("not a valid unionid field"));

        let lowercase_variant =
            expand_str("#[serde(rename_all = \"snake_case\")] enum T { InProgress }")
                .unwrap_err();
        assert!(
            lowercase_variant
                .to_string()
                .contains("not a valid unionid variant")
        );

        let duplicate = expand_str(
            "struct T { #[serde(rename = \"value\")] first: i64, value: i64 }",
        )
        .unwrap_err();
        assert!(duplicate.to_string().contains("duplicate record field"));
    }
}
