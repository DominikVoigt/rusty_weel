use proc_macro::TokenStream;
use quote::quote;

/**
 * Injects the contents of the file provided into the file as if it was code.
 * Path can be either absolute or relative to the workspace root
 * Expects TokenStream to contain the path (single "string" expression)
 */
#[proc_macro]
pub fn inject(input: TokenStream) -> TokenStream {
    // Strip "-symbol from string literal:
    let main_content = open_file(input.to_string().replace("\"", "").as_str());
    // Correct indentiation
    let main_content = format!("\t{}", main_content.replace("\n", "\n\t"));
    main_content.parse().unwrap()
}

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

fn open_file(path: &str) -> String {
    println!("Trying to open path: {:?}", path);
    std::fs::read_to_string(path).unwrap()
}