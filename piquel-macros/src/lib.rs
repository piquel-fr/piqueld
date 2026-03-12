use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use syn::{
    FnArg, Ident, ImplItem, ItemImpl, Pat, ReturnType, Token, Type,
    parse::{Parse, ParseStream},
    parse_macro_input,
};

/// Parses `#[service(error = SomeError)]`
struct ServiceAttr {
    error: Type,
}

impl Parse for ServiceAttr {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        // expect: error = <Type>
        let key: Ident = input.parse()?;
        if key != "error" {
            return Err(syn::Error::new(key.span(), "expected `error = <Type>`"));
        }
        let _eq: Token![=] = input.parse()?;
        let error: Type = input.parse()?;
        Ok(ServiceAttr { error })
    }
}

/// `get_repository` → `GetRepository`
fn snake_to_pascal(ident: &Ident) -> Ident {
    let pascal: String = ident
        .to_string()
        .split('_')
        .map(|word| {
            let mut chars = word.chars();
            // just the first char
            match chars.next() {
                None => String::new(),
                Some(c) => c.to_uppercase().to_string() + chars.as_str(),
            }
        })
        .collect();
    format_ident!("{}", pascal, span = ident.span())
}

/// Extracts `T` from `Result<T, _>` or `Result<T>` (type alias form).
/// Falls back to the whole type if not recognised as a Result.
fn unwrap_result_ok(ty: &Type) -> Type {
    // if the type is a path
    if let Type::Path(tp) = ty {
        // get the last segment of the path & check if it is Result
        if let Some(last) = tp.path.segments.last() {
            if last.ident == "Result" {
                // get the type in angle brackets
                if let syn::PathArguments::AngleBracketed(ref args) = last.arguments {
                    // get the type of the first argument
                    if let Some(syn::GenericArgument::Type(inner)) = args.args.first() {
                        return inner.clone();
                    }
                }
            }
        }
    }
    ty.clone()
}

struct ServiceMethod {
    name: Ident,
    params: Vec<(Ident, Type)>,
    /// The `T` in `Result<T, E>`
    ok_type: Type,
}

impl ServiceMethod {
    /// The PascalCase command-enum variant name.
    fn variant_name(&self) -> Ident {
        snake_to_pascal(&self.name)
    }

    /// Generates the enum variant definition.
    fn enum_variant(&self, error: &Type) -> TokenStream2 {
        let variant = self.variant_name();
        let fields = self.params.iter().map(|(n, t)| quote! { #n: #t, });
        let ok = &self.ok_type;
        quote! {
            #variant {
                #(#fields)*
                reply: ::tokio::sync::oneshot::Sender<
                    ::std::result::Result<#ok, #error>
                >,
            }
        }
    }

    /// Generates the `match` arm inside the actor loop.
    fn match_arm(&self, command_enum: &Ident) -> TokenStream2 {
        let variant = self.variant_name();
        let method = &self.name;
        let param_names: Vec<_> = self.params.iter().map(|(n, _)| n).collect();
        quote! {
            #command_enum::#variant { #(#param_names,)* reply } => {
                let _ = reply.send(service.#method(#(#param_names,)*));
            }
        }
    }

    /// Generates the `pub async fn` on the public service struct.
    fn async_method(&self, command_enum: &Ident, error: &Type) -> TokenStream2 {
        let variant = self.variant_name();
        let method = &self.name;
        let param_defs = self.params.iter().map(|(n, t)| quote! { #n: #t });
        let param_names: Vec<_> = self.params.iter().map(|(n, _)| n).collect();
        let ok = &self.ok_type;
        quote! {
            pub async fn #method(
                &self,
                #(#param_defs,)*
            ) -> ::std::result::Result<#ok, #error> {
                let (reply, rx) = ::tokio::sync::oneshot::channel();
                crate::services::ask(
                    &self.tx,
                    #command_enum::#variant { #(#param_names,)* reply },
                    rx,
                ).await
            }
        }
    }
}

/// Turns a synchronous `*Service` struct into a fully async actor-pattern service.
///
/// # Rules
///
/// - Applied to an `impl` block whose type name ends in `Service`
///   (e.g. `GitService` → produces `GitHandle`).
/// - The `fn init(config: &ServerConfig) -> Result<Self>` method is treated
///   specially and becomes the constructor of the public wrapper.
/// - Every other `fn` in this block becomes an async method on the wrapper,
///   regardless of visibility.
/// - Private helpers should live in a **separate, plain `impl` block** —
///   they will never be seen by this macro.
#[proc_macro_attribute]
pub fn service(attr: TokenStream, item: TokenStream) -> TokenStream {
    let ServiceAttr { error } = parse_macro_input!(attr as ServiceAttr);
    let impl_block = parse_macro_input!(item as ItemImpl);

    // -----------------------------------------------------------------------
    // Derive names
    // -----------------------------------------------------------------------
    let impl_ident: &Ident = match impl_block.self_ty.as_ref() {
        Type::Path(tp) => tp
            .path
            .get_ident()
            .expect("#[service] impl type must be a plain identifier"),
        _ => panic!("#[service] must be applied to a plain `impl TypeName` block"),
    };

    let raw = impl_ident.to_string();
    let stripped = raw.strip_suffix("Service").unwrap_or(&raw);

    let service_name = format_ident!("{}Handle", stripped, span = impl_ident.span());
    let command_enum = format_ident!("{}Command", stripped);

    // Collect service methods (public, not `init`)
    let unit_ty: Type = syn::parse2(quote! { () }).unwrap();

    let methods: Vec<ServiceMethod> = impl_block
        .items
        .iter()
        .filter_map(|item| match item {
            ImplItem::Fn(f) if f.sig.ident != "init" => Some(f),
            _ => None,
        })
        .map(|f| {
            let name = f.sig.ident.clone();

            let params = f
                .sig
                .inputs
                .iter()
                .filter_map(|arg| match arg {
                    FnArg::Typed(pt) => match pt.pat.as_ref() {
                        Pat::Ident(pi) => Some((pi.ident.clone(), *pt.ty.clone())),
                        _ => None,
                    },
                    FnArg::Receiver(_) => None,
                })
                .collect();

            let ok_type = match &f.sig.output {
                ReturnType::Type(_, ty) => unwrap_result_ok(ty),
                ReturnType::Default => unit_ty.clone(),
            };

            ServiceMethod {
                name,
                params,
                ok_type,
            }
        })
        .collect();

    // Code generation
    let variants: Vec<_> = methods.iter().map(|m| m.enum_variant(&error)).collect();
    let match_arms: Vec<_> = methods.iter().map(|m| m.match_arm(&command_enum)).collect();
    let async_fns: Vec<_> = methods
        .iter()
        .map(|m| m.async_method(&command_enum, &error))
        .collect();

    quote! {
        // Keep the original impl block unchanged.
        #impl_block

        enum #command_enum {
            #(#variants,)*
        }

        pub struct #service_name {
            tx: ::tokio::sync::mpsc::Sender<#command_enum>,
        }

        impl #service_name {
            pub fn init(
                config: &crate::config::ServerConfig,
            ) -> ::std::result::Result<Self, #error> {
                let (tx, mut rx) =
                    ::tokio::sync::mpsc::channel::<#command_enum>(32);
                let mut service = #impl_ident::init(config)?;

                ::tokio::spawn(async move {
                    while let Some(cmd) = rx.recv().await {
                        match cmd {
                            #(#match_arms)*
                        }
                    }
                });

                Ok(Self { tx })
            }

            #(#async_fns)*
        }
    }
    .into()
}
