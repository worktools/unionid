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
use syn::{Attribute, Data, DeriveInput, Fields, GenericArgument, LitStr, PathArguments, Type};
use syn::{parse_macro_input, spanned::Spanned};

#[proc_macro_derive(UnionidSchema, attributes(unionid))]
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

fn expand(input: &DeriveInput) -> syn::Result<proc_macro2::TokenStream> {
    if !input.generics.params.is_empty() {
        return Err(syn::Error::new(
            input.generics.span(),
            "unionid schema types cannot be generic",
        ));
    }
    let options = container_options(&input.attrs)?;
    validate_table_options(input, &options)?;
    let type_name = input.ident.to_string();
    let mut dependencies = BTreeSet::new();
    let type_ddl = match &input.data {
        Data::Struct(data) => {
            let fields = named_fields(&data.fields, "unionid records require named fields")?;
            let mut lines = Vec::new();
            for field in fields {
                let name = field.ident.as_ref().expect("named field").to_string();
                let ty = field_type(&field.ty, &field.attrs, &mut dependencies)?;
                lines.push(format!("  {name} {ty},"));
            }
            format!("type {type_name} = {{\n{}\n}}", lines.join("\n"))
        }
        Data::Enum(data) => {
            let mut lines = Vec::new();
            for variant in &data.variants {
                let name = variant.ident.to_string();
                let rendered = match &variant.fields {
                    Fields::Unit => name.clone(),
                    Fields::Named(fields) => {
                        let mut inner = Vec::new();
                        for field in &fields.named {
                            let field_name = field.ident.as_ref().expect("named field").to_string();
                            let ty = field_type(&field.ty, &field.attrs, &mut dependencies)?;
                            inner.push(format!("{field_name} {ty}"));
                        }
                        format!("{name} {{{}}}", inner.join(", "))
                    }
                    Fields::Unnamed(fields) => {
                        let mut rendered = Vec::new();
                        for field in &fields.unnamed {
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
            if let Some(key) = &options.key {
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
}
