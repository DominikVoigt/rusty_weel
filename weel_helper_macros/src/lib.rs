use proc_macro::TokenStream;
use quote::quote;

/**
 * Helper to extract string value from a JsonValue (https://docs.rs/json/latest/json/enum.JsonValue.html)
 */
#[proc_macro]
pub fn get_str_from_value(input: TokenStream) -> TokenStream {
    let input = proc_macro2::TokenStream::from(input);
    quote! {
        match #input.as_str() {
            Some(x) => x.to_owned(),
            None => {
                panic!("Could not retrieve string value");
            }
        }
    }.into()
}

/**
 * Helper to extract string value from a JsonValue (https://docs.rs/json/latest/json/enum.JsonValue.html)
 */
#[proc_macro]
pub fn get_str_from_value_simple(input: TokenStream) -> TokenStream {
    let input = proc_macro2::TokenStream::from(input);
    quote! {
        match #input.as_str() {
            Some(x) => x.to_owned(),
            None => {
                panic!("panicing!");
            }
        }
    }.into()
}