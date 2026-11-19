//! `#[derive(Configurable)]` for Accelerando components.
//!
//! One struct declares both the [`ParamSpec`] and the constructor, so a parameter's name, default
//! and range are written exactly once:
//!
//! ```ignore
//! #[derive(Configurable)]
//! #[configurable(init = Self::init)]          // optional post-construction hook: fn init(&mut self)
//! pub struct MyStrategy {
//!     #[param(default = 8, min = 1, max = 100)]           stop_ticks: u32,
//!     #[param(default = 1.5, min = 0.3, max = 6.0)]       target_r: f64,
//!     #[param(default = true)]                            exit_on_cross: bool,
//!     #[param(default = "both", choice = ["both", "long", "short"])] side: String,
//!     #[param(fixed, default = 0.25)]                     tick: f64,
//!     #[param(flatten)]                                   session: SessionFilter,
//!     ema: Ema,                                           // no #[param]: Default::default()
//! }
//! ```
//!
//! Field kinds are picked from the field type:
//!
//! * integers (`i8`..`i64`, `u8`..`u64`, `usize`, `isize`) → `ParamSpec::int` (`step` defaults
//!   to 1)
//! * `f32` / `f64` → `ParamSpec::float` (optional `step`)
//! * `bool` → `ParamSpec::bool` (searched as 0/1; CLI accepts `true/false/1/0`)
//! * `int` on a float field → an integer parameter stored as `f64` (`#[param(int, default = 40,
//!   min = 4, max = 200, step = 2)] stop_ticks: f64`)
//! * integers with `time` → a minute-of-day: `#[param(time, default = "18:00", step = 5)]`;
//!   `min`/`max` default to `"00:00"`/`"24:00"` and are also written `"HH:MM"`. The field holds
//!   minutes (`18:00` → `1080`); the CLI accepts `--param name=18:00` as well as `1080`.
//! * `String` with `choice = [...]` → `ParamSpec::choice`; with `fixed` → `fixed_str`
//! * any other type with `choice = [...]` → parsed through `FromStr` (handy for enums)
//!
//! This derive is the only way to implement `Configurable`: the trait is sealed, so every
//! parameter is declared once, next to its field. Values that depend on several parameters (or
//! clamps between them) belong in the `init` hook.
//!
//! `#[configurable(indicator = "registry_name")]` additionally generates `FooRequest`:
//! typed setters for declared parameters, `build()` for direct construction and
//! `Into<IndicatorRequest>` for the existing serializable registry boundary.
//! Numeric validation and the init hook apply to direct construction as well.
//!
//! `fixed` makes any scalar non-searchable. `name = "..."` overrides the parameter name (keeps
//! old CLI names when renaming a field). `#[param(flatten)]` embeds another `#[derive(Configurable)]`
//! struct's parameters (e.g. shared session/discipline groups).

use proc_macro::TokenStream;
use proc_macro2::{Span, TokenStream as TokenStream2};
use quote::quote;
use syn::{
    parse_macro_input, spanned::Spanned, Data, DeriveInput, Expr, ExprArray, Fields, LitStr, Path,
    Type,
};

#[proc_macro_derive(Configurable, attributes(param, configurable))]
pub fn derive_configurable(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    match expand(&input) {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

#[derive(Default)]
struct ParamAttr {
    present: bool,
    flatten: bool,
    fixed: bool,
    time: bool,
    int: bool,
    name: Option<String>,
    default: Option<Expr>,
    min: Option<Expr>,
    max: Option<Expr>,
    step: Option<Expr>,
    choice: Option<Vec<Expr>>,
}

enum Kind {
    Int,
    Float,
    Bool,
    Str,
    Parsed,
}

fn type_name(ty: &Type) -> Option<String> {
    match ty {
        Type::Path(p) => p.path.segments.last().map(|s| s.ident.to_string()),
        _ => None,
    }
}

fn kind_of(ty: &Type, attr: &ParamAttr) -> syn::Result<Kind> {
    let name = type_name(ty).unwrap_or_default();
    Ok(match name.as_str() {
        // A choice list on any non-String field: options are parsed with FromStr
        // (e.g. `entry_ratio: f64` with `choice = ["0.5", "0.618"]`).
        n if attr.choice.is_some() && n != "String" => Kind::Parsed,
        "i8" | "i16" | "i32" | "i64" | "isize" | "u8" | "u16" | "u32" | "u64" | "usize" => {
            Kind::Int
        }
        "f32" | "f64" => Kind::Float,
        "bool" => Kind::Bool,
        "String" => Kind::Str,
        _ if attr.choice.is_some() => Kind::Parsed,
        _ => {
            return Err(syn::Error::new(
                ty.span(),
                "#[param] supports integer, float, bool and String fields; other types need \
                 `choice = [...]` (parsed with FromStr) or `#[param(flatten)]`",
            ))
        }
    })
}

fn parse_param_attr(field: &syn::Field) -> syn::Result<ParamAttr> {
    let mut out = ParamAttr::default();
    for attr in &field.attrs {
        if !attr.path().is_ident("param") {
            continue;
        }
        out.present = true;
        attr.parse_nested_meta(|meta| {
            let key = meta
                .path
                .get_ident()
                .map(|i| i.to_string())
                .unwrap_or_default();
            match key.as_str() {
                "flatten" => out.flatten = true,
                "fixed" => out.fixed = true,
                "time" => out.time = true,
                "int" => out.int = true,
                "name" => out.name = Some(meta.value()?.parse::<LitStr>()?.value()),
                "default" => out.default = Some(meta.value()?.parse()?),
                "min" => out.min = Some(meta.value()?.parse()?),
                "max" => out.max = Some(meta.value()?.parse()?),
                "step" => out.step = Some(meta.value()?.parse()?),
                "choice" => {
                    let arr: ExprArray = meta.value()?.parse()?;
                    out.choice = Some(arr.elems.into_iter().collect());
                }
                other => {
                    return Err(meta.error(format!(
                        "unknown #[param] key `{other}`; expected one of: default, min, max, \
                         step, choice, fixed, time, int, name, flatten"
                    )))
                }
            }
            Ok(())
        })?;
    }
    Ok(out)
}

fn parse_container_init(input: &DeriveInput) -> syn::Result<Option<Path>> {
    let mut init = None;
    for attr in &input.attrs {
        if !attr.path().is_ident("configurable") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("init") {
                init = Some(meta.value()?.parse::<Path>()?);
                Ok(())
            } else if meta.path.is_ident("indicator") {
                let _: LitStr = meta.value()?.parse()?;
                Ok(())
            } else {
                Err(meta.error(
                    "unknown #[configurable] key; expected `init = path` or `indicator = \"name\"`",
                ))
            }
        })?;
    }
    Ok(init)
}

/// `"HH:MM"` (`"24:00"` allowed) → minutes, checked at compile time.
fn time_minutes(expr: &Expr, what: &str) -> syn::Result<i64> {
    let bad = || {
        syn::Error::new(
            expr.span(),
            format!("`time` parameter `{what}` must be a string literal \"HH:MM\""),
        )
    };
    let Expr::Lit(syn::ExprLit {
        lit: syn::Lit::Str(lit),
        ..
    }) = expr
    else {
        return Err(bad());
    };
    let raw = lit.value();
    let (h, m) = raw.split_once(':').ok_or_else(bad)?;
    let (h, m): (i64, i64) = (
        h.trim().parse().map_err(|_| bad())?,
        m.trim().parse().map_err(|_| bad())?,
    );
    if !(0..=24).contains(&h) || !(0..60).contains(&m) || h * 60 + m > 1440 {
        return Err(bad());
    }
    Ok(h * 60 + m)
}

fn require(expr: &Option<Expr>, what: &str, span: Span) -> syn::Result<Expr> {
    expr.clone()
        .ok_or_else(|| syn::Error::new(span, format!("#[param] needs `{what} = ...` here")))
}

fn expand(input: &DeriveInput) -> syn::Result<TokenStream2> {
    let ident = &input.ident;
    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();
    let Data::Struct(data) = &input.data else {
        return Err(syn::Error::new(
            input.span(),
            "#[derive(Configurable)] supports structs only",
        ));
    };
    let Fields::Named(fields) = &data.fields else {
        return Err(syn::Error::new(
            input.span(),
            "#[derive(Configurable)] needs named fields",
        ));
    };
    let init = parse_container_init(input)?;
    let core = quote!(::accelerando_core);

    let mut indicator_name = None;
    for attr in &input.attrs {
        if attr.path().is_ident("configurable") {
            attr.parse_nested_meta(|meta| {
                if meta.path.is_ident("indicator") {
                    indicator_name = Some(meta.value()?.parse::<LitStr>()?);
                } else if meta.path.is_ident("init") {
                    let _: Path = meta.value()?.parse()?;
                }
                Ok(())
            })?;
        }
    }
    let mut request_fields = Vec::new();
    let mut request_methods = Vec::new();
    let mut request_reads = Vec::new();
    let mut request_params = Vec::new();
    let mut request_validation = Vec::new();
    let mut declares = Vec::new();
    let mut reads = Vec::new();
    for field in &fields.named {
        let fident = field.ident.as_ref().expect("named field");
        let ty = &field.ty;
        let attr = parse_param_attr(field)?;
        if !attr.present {
            reads.push(quote!(#fident: ::core::default::Default::default()));
            request_reads.push(quote!(#fident: ::core::default::Default::default()));
            continue;
        }
        if attr.flatten {
            if indicator_name.is_some() {
                return Err(syn::Error::new(
                    field.span(),
                    "typed indicator requests do not yet support flatten",
                ));
            }
            declares.push(quote!(let spec = <#ty as #core::ParamGroup>::declare_params(spec);));
            reads.push(quote!(#fident: <#ty as #core::ParamGroup>::read_params(params)));
            continue;
        }
        let pname = attr.name.clone().unwrap_or_else(|| fident.to_string());
        let span = field.span();
        let mut kind = kind_of(ty, &attr)?;
        if attr.int {
            if !matches!(kind, Kind::Float) {
                return Err(syn::Error::new(
                    span,
                    "`int` is for float fields that hold an integer parameter",
                ));
            }
            kind = Kind::Int;
        }
        if attr.time && !matches!(kind, Kind::Int) {
            return Err(syn::Error::new(
                span,
                "`time` needs an integer field (it holds minutes)",
            ));
        }
        let (declare, read) = match kind {
            Kind::Int if attr.time => {
                let d = time_minutes(&require(&attr.default, "default", span)?, "default")?;
                let read = quote!(params.int(#pname, #d) as #ty);
                if attr.fixed {
                    (quote!(spec.fixed_int(#pname, #d)), read)
                } else {
                    let min = attr
                        .min
                        .as_ref()
                        .map(|e| time_minutes(e, "min"))
                        .transpose()?
                        .unwrap_or(0);
                    let max = attr
                        .max
                        .as_ref()
                        .map(|e| time_minutes(e, "max"))
                        .transpose()?
                        .unwrap_or(1440);
                    let step = attr.step.clone().map(|s| quote!(#s)).unwrap_or(quote!(1));
                    (
                        quote!(spec.time(#pname, #d, #min, #max, (#step) as i64)),
                        read,
                    )
                }
            }
            Kind::Int => {
                let d = require(&attr.default, "default", span)?;
                let read = quote!(params.int(#pname, (#d) as i64) as #ty);
                if attr.fixed {
                    (quote!(spec.fixed_int(#pname, (#d) as i64)), read)
                } else {
                    let min = require(&attr.min, "min", span)?;
                    let max = require(&attr.max, "max", span)?;
                    let step = attr.step.clone().map(|s| quote!(#s)).unwrap_or(quote!(1));
                    (
                        quote!(spec.int(#pname, (#d) as i64, (#min) as i64, (#max) as i64, (#step) as i64)),
                        read,
                    )
                }
            }
            Kind::Float => {
                let d = require(&attr.default, "default", span)?;
                let read = quote!(params.float(#pname, (#d) as f64) as #ty);
                if attr.fixed {
                    (quote!(spec.fixed_float(#pname, (#d) as f64)), read)
                } else {
                    let min = require(&attr.min, "min", span)?;
                    let max = require(&attr.max, "max", span)?;
                    let declare = match &attr.step {
                        Some(step) => {
                            quote!(spec.float_step(#pname, (#d) as f64, (#min) as f64, (#max) as f64, (#step) as f64))
                        }
                        None => {
                            quote!(spec.float(#pname, (#d) as f64, (#min) as f64, (#max) as f64))
                        }
                    };
                    (declare, read)
                }
            }
            Kind::Bool => {
                let d = attr
                    .default
                    .clone()
                    .map(|d| quote!(#d))
                    .unwrap_or(quote!(false));
                let declare = if attr.fixed {
                    quote!(spec.fixed_int(#pname, if #d { 1 } else { 0 }))
                } else {
                    quote!(spec.bool(#pname, #d))
                };
                (declare, quote!(params.bool(#pname, #d)))
            }
            Kind::Str | Kind::Parsed => {
                let d = match (&attr.default, &attr.choice) {
                    (Some(d), _) => quote!(#d),
                    (None, Some(opts)) if !opts.is_empty() => {
                        let first = &opts[0];
                        quote!(#first)
                    }
                    _ => return Err(syn::Error::new(span, "#[param] needs `default = \"...\"`")),
                };
                let declare = match (&attr.choice, attr.fixed) {
                    (Some(opts), false) => quote!(spec.choice(#pname, #d, &[#(#opts),*])),
                    (None, true) | (Some(_), true) => quote!(spec.fixed_str(#pname, #d)),
                    (None, false) => {
                        return Err(syn::Error::new(
                            span,
                            "String #[param] needs `choice = [...]` or `fixed`",
                        ))
                    }
                };
                let read = if matches!(kind, Kind::Str) {
                    quote!(params.str(#pname, #d))
                } else {
                    quote!({
                        let raw = params.str(#pname, #d);
                        raw.parse::<#ty>().unwrap_or_else(|_| {
                            panic!("parameter `{}`: cannot parse `{}`", #pname, raw)
                        })
                    })
                };
                (declare, read)
            }
        };
        if indicator_name.is_some() {
            let method = syn::parse_str::<syn::Ident>(&pname).map_err(|_| {
                syn::Error::new(
                    span,
                    "typed request parameter names must be Rust identifiers",
                )
            })?;
            let (setter_type, setter_value) = if attr.int {
                (quote!(i64), quote!(value as #ty))
            } else {
                (quote!(#ty), quote!(value))
            };
            request_fields.push(quote!(#fident: Option<#ty>));
            request_methods.push(quote! {
                pub fn #method(mut self, value: #setter_type) -> Self {
                    self.#fident = Some(#setter_value);
                    self
                }
            });
            request_reads.push(quote!(#fident: self.#fident.unwrap_or_else(|| #read)));
            let encoded = match kind {
                Kind::Int => quote!(value as i64),
                Kind::Float => quote!(value as f64),
                _ => quote!(value),
            };
            if !matches!(kind, Kind::Parsed) {
                request_validation.push(quote! {
                    if let Some(value) = self.#fident.clone() {
                        checks.set(#pname, (#encoded).into());
                    }
                });
            }
            request_params.push(quote! {
                if let Some(value) = self.#fident { request = request.param(#pname, #encoded); }
            });
        }
        declares.push(quote!(let spec = #declare;));
        reads.push(quote!(#fident: #read));
    }

    let post_init = init.as_ref().map(|path| quote!(#path(&mut value);));
    let request = indicator_name.map(|name| {
        let request_ident = syn::Ident::new(&format!("{}Request", ident), ident.span());
        let vis = &input.vis;
        let request_init = init.as_ref().map(|path| {
            let mut path = path.clone();
            if path.segments.first().is_some_and(|s| s.ident == "Self") {
                path.segments.first_mut().unwrap().ident = ident.clone();
            }
            quote!(#path(&mut value);)
        });
        quote! {
            /// Typed indicator options. Converts to a registry request only at the configuration boundary.
            #[derive(Default)]
            #vis struct #request_ident { #(#request_fields),* }
            impl #request_ident {
                pub fn new() -> Self { Self::default() }
                #(#request_methods)*
                /// Construct directly, preserving typed options without a Params round trip.
                pub fn build(self) -> #ident {
                    let mut checks = #core::Params::default();
                    #(#request_validation)*
                    <#ident as #core::Configurable>::param_spec().resolve(&checks)
                        .unwrap_or_else(|error| panic!("invalid params: {error}"));
                    let params = &#core::Params::default();
                    let mut value = #ident { #(#request_reads),* };
                    #request_init
                    value
                }
            }
            impl From<#request_ident> for #core::IndicatorRequest {
                fn from(options: #request_ident) -> Self { options.into_request() }
            }
            impl #request_ident {
                pub fn into_request(self) -> #core::IndicatorRequest {
                    let mut request = #core::IndicatorRequest::new(#name);
                    #(#request_params)*
                    request
                }
            }
        }
    });
    Ok(quote! {
        #request
        impl #impl_generics #core::__private::Sealed for #ident #ty_generics #where_clause {}
        impl #impl_generics #core::ParamGroup for #ident #ty_generics #where_clause {
            fn declare_params(spec: #core::ParamSpec) -> #core::ParamSpec {
                #(#declares)*
                spec
            }
            #[allow(unused_mut)]
            fn read_params(params: &#core::Params) -> Self {
                let mut value = Self { #(#reads),* };
                #post_init
                value
            }
        }
    })
}
