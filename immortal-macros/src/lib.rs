extern crate proc_macro;
use proc_macro::TokenStream;
// use proc_macro2::TokenStream;
use quote::quote;
use syn::{parse_macro_input, ItemFn};

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


#[proc_macro_attribute]
pub fn wf(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let item2 = item.clone();
    // Parse the input TokenStream as a function
    let input = parse_macro_input!(item as ItemFn);
    println!("input: {:#?}", input);
    //
    // // Extract the function name
    // let fn_name = &input.sig.ident;
    //
    // // Extract the function input types
    // let inputs: Vec<String> = input
    //     .sig
    //     .inputs
    //     .iter()
    //     .map(|arg| {
    //         match arg {
    //             syn::FnArg::Typed(pat_type) => {
    //                 let ty = &pat_type.ty;
    //                 format!("{}", quote! {#ty})
    //             }
    //             _ => panic!("Unexpected argument type"),
    //         }
    //     })
    //     .collect();
    //
    // // Extract the return type
    // let output = match &input.sig.output {
    //     syn::ReturnType::Default => "void".to_string(),
    //     syn::ReturnType::Type(_, ty) => format!("{}", quote! {#ty}),
    // };
    //
    // // Generate additional code to print the function schema
    // let expanded = quote! {
    //     #input
    //
    //     impl #fn_name {
    //         pub fn get_function_schema() -> String {
    //             let inputs = vec![#(#inputs.to_string()),*];
    //             let output = #output.to_string();
    //             format!("Function {} has inputs {:?} and returns {}", stringify!(#fn_name), inputs, output)
    //         }
    //     }
    // };

    // Return the original function plus the new code
    // TokenStream::from(expanded)
    item2
}

#[proc_macro_attribute]
pub fn function_schema(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let item2 = item.clone();
    // Parse the input TokenStream as a function
    let input = parse_macro_input!(item as ItemFn);
    // println!("input: {:#?}", input);
    //
    // // Extract the function name
    // let fn_name = &input.sig.ident;
    //
    // // Extract the function input types
    // let inputs: Vec<String> = input
    //     .sig
    //     .inputs
    //     .iter()
    //     .map(|arg| {
    //         match arg {
    //             syn::FnArg::Typed(pat_type) => {
    //                 let ty = &pat_type.ty;
    //                 format!("{}", quote! {#ty})
    //             }
    //             _ => panic!("Unexpected argument type"),
    //         }
    //     })
    //     .collect();
    //
    // // Extract the return type
    // let output = match &input.sig.output {
    //     syn::ReturnType::Default => "void".to_string(),
    //     syn::ReturnType::Type(_, ty) => format!("{}", quote! {#ty}),
    // };
    //
    // // Generate additional code to print the function schema
    // let expanded = quote! {
    //     #input
    //
    //     impl #fn_name {
    //         pub fn get_function_schema() -> String {
    //             let inputs = vec![#(#inputs.to_string()),*];
    //             let output = #output.to_string();
    //             format!("Function {} has inputs {:?} and returns {}", stringify!(#fn_name), inputs, output)
    //         }
    //     }
    // };

    // Return the original function plus the new code
    // TokenStream::from(expanded)
    item2
}
