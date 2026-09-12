//! Derive unionid schema declarations from Rust types.
//!
//! `#[derive(UnionidSchema)]` maps a Rust struct or enum to a unionid `type`
//! declaration and, when `#[unionid(table = "...", key = "...")]` is present, a
//! `table` declaration. The generated code implements `unionid::UnionidSchema`
//! so declarations can be collected with `unionid::SchemaBuilder`.

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
    let type_name = input.ident.to_string();
    let type_ddl = match &input.data {
        Data::Struct(data) => {
            let fields = named_fields(&data.fields, "unionid records require named fields")?;
            let mut lines = Vec::new();
            for field in fields {
                let name = field.ident.as_ref().expect("named field").to_string();
                let ty = field_type(&field.ty, &field.attrs)?;
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
                            let ty = field_type(&field.ty, &field.attrs)?;
                            inner.push(format!("{field_name} {ty}"));
                        }
                        format!("{name} {{{}}}", inner.join(", "))
                    }
                    Fields::Unnamed(fields) => {
                        let mut rendered = Vec::new();
                        for field in &fields.unnamed {
                            rendered.push(field_type(&field.ty, &field.attrs)?);
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
    let ident = &input.ident;
    Ok(quote! {
        impl ::unionid::UnionidSchema for #ident {
            const UNIONID_TYPE_NAME: &'static str = #type_name_lit;

            fn unionid_type_ddl() -> ::std::string::String {
                ::std::string::String::from(#type_ddl_lit)
            }

            fn unionid_table_ddl() -> ::core::option::Option<::std::string::String> {
                #table_ddl
            }
        }
    })
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

fn field_type(ty: &Type, attrs: &[Attribute]) -> syn::Result<String> {
    let decimal = decimal_attribute(attrs)?;
    type_ddl(ty, decimal.as_deref())
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

fn type_ddl(ty: &Type, decimal: Option<&str>) -> syn::Result<String> {
    match ty {
        Type::Path(path) => path_ddl(path, decimal),
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
                .map(|element| type_ddl(element, decimal))
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

fn path_ddl(path: &syn::TypePath, decimal: Option<&str>) -> syn::Result<String> {
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
            "Decimal" => decimal
                .map(|value| format!("decimal {value}"))
                .ok_or_else(|| {
                    syn::Error::new(
                        path.span(),
                        "a Decimal field needs #[unionid(decimal = \"P S\")]",
                    )
                }),
            _ => Ok(name),
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
                "Option" => Ok(format!("option ({})", type_ddl(inner, decimal)?)),
                "Vec" => Ok(format!("list ({})", type_ddl(inner, decimal)?)),
                "Box" => type_ddl(inner, decimal),
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
