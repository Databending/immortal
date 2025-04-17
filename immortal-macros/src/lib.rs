extern crate proc_macro;
use proc_macro::TokenStream;
use quote::quote;
use syn::{parse_macro_input, FnArg, ItemFn, PatType, PathArguments, ReturnType, Type};



#[proc_macro_derive(FunctionSchema)]
pub fn function_schema_derive(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as ItemFn);

    // Extract function name
    let fn_name = &input.sig.ident;

    // Extract function inputs
    let inputs: Vec<String> = input
        .sig
        .inputs
        .iter()
        .map(|arg| match arg {
            syn::FnArg::Typed(pat_type) => {
                let ty = &pat_type.ty;
                format!("{}", quote! {#ty})
            }
            _ => panic!("Unexpected argument type"),
        })
        .collect();

    // Extract function return type
    let output = match &input.sig.output {
        syn::ReturnType::Default => "void".to_string(),
        syn::ReturnType::Type(_, ty) => format!("{}", quote! {#ty}),
    };

    // Generate code to print the function schema
    let expanded = quote! {
        impl #fn_name {
            pub fn get_function_schema() -> String {
                let inputs = vec![#(#inputs.to_string()),*];
                let output = #output.to_string();
                format!("Function {} has inputs {:?} and returns {}", stringify!(#fn_name), inputs, output)
            }
        }
    };

    TokenStream::from(expanded)
}
//
//fn get_add_operation_fn_name(route_fn_name: &Ident, suffix: &str) -> Ident {
//    Ident::new(&format!("{}_{suffix}", route_fn_name), route_fn_name.span())
//}

fn extract_generic_from_return_type(return_type: &ReturnType) -> Option<&Type> {
    // Check if the return type is of the form `-> Type`
    if let ReturnType::Type(_, ty) = return_type {
        // Check if the return type is a path (e.g., Result<T, E> or Option<T>)
        if let Type::Path(type_path) = &**ty {
            // Check if there are segments in the path
            if let Some(segment) = type_path.path.segments.last() {
                // Check if there are generic arguments (e.g., T in Result<T, E>)
                if let PathArguments::AngleBracketed(generics) = &segment.arguments {
                    // Return the first generic argument if available
                    if let Some(syn::GenericArgument::Type(ty)) = generics.args.first() {
                        return Some(ty);
                    }
                }
            }
        }
    }
    None
}

#[proc_macro_attribute]
pub fn wf(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let input = parse_macro_input!(item as ItemFn);


    // Extract the function signature and argument names
    let fn_name = &input.sig.ident;
    let fn_args = &input.sig.inputs;
    let fn_output = &input.sig.output;


    // input.sig.ident = Ident::new(&format!("{}_wf", input.sig.ident), input.sig.ident.span());

    let out = extract_generic_from_return_type(fn_output);
    // println!("fn_name: {:#?}", fn_output);
    // match fn_output {
    //     ReturnType::Type(_, ty) => {
    //     }
    //     _ => {}
    // }

    // let fn_name2 = get_add_operation_fn_name(&input.sig.ident, "wf");
    // let fn_name3 = get_add_operation_fn_name(&input.sig.ident, "wf_schema");
    // Generate code to extract payloads and map them to arguments
    let arg_mappings: Vec<(
        proc_macro2::TokenStream,
        proc_macro2::TokenStream,
        proc_macro2::TokenStream,
    )> = fn_args
        .iter()
        .enumerate()
        .map(|(i, arg)| {
            if i == 0 {
                None
            } else {
                if let FnArg::Typed(PatType { pat, ty, .. }) = arg {
                    let index = proc_macro2::Literal::usize_unsuffixed(i); // Index for each payload

                    Some((
                        quote! {
                            let #pat = ctx.args.payloads.get(#index - 1).unwrap().clone().to::<#ty>()?;
                        },
                        quote! {#pat},
                        quote! { schema_for!(#ty) },
                    ))
                } else {
                    None
                }
            }
            // Ensure that we are working with function arguments of the form `arg: Type`
        })
        .filter(|f| f.is_some())
        .map(|f| f.unwrap())
        .collect();
    let arg_mappings_0: Vec<_> = arg_mappings.iter().map(|f| f.0.clone()).collect();
    let arg_mappings_1: Vec<_> = arg_mappings.iter().map(|f| f.1.clone()).collect();
    let arg_mappings_2: Vec<_> = arg_mappings.iter().map(|f| f.2.clone()).collect();

    // Generate the expanded function that handles payload extraction
    let expanded = quote! {

    use schemars::schema_for;
    use schemars::schema::RootSchema;
            // Original function
            #input

            // Wrapper function to handle payload extraction
            pub async fn wf(ctx: WfContext) -> WorkflowResult<#out> {
                let payloads = ctx.args.deref().clone();

                // Extract all arguments from payloads
                #(#arg_mappings_0)*

                // Call the original workflow function
                let result = #fn_name(ctx, #(#arg_mappings_1),*).await;
                result
            }
            pub fn wf_schema() -> (Vec<schemars::schema::RootSchema>, schemars::schema::RootSchema) {
                (vec![#(#arg_mappings_2),*], schema_for!(#out))
            }
        };


    TokenStream::from(expanded)
}

