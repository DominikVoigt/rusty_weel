use proc_macro::TokenStream;
use quote::quote;

/**
 * Injects the contents of the file provided into the file as if it was code.
 * Path can be either absolute or relative to the workspace root
 * Expects TokenStream to contain the path (single "string" expression)
 */
#[proc_macro]
pub fn inject(_input: TokenStream) -> TokenStream {

    let path = std::env::var("EIC_FILE").unwrap_or("./instance.rs".to_owned());
    // Strip "-symbol from string literal:
    //let path = input.to_string().replace("\"", "");
    let main_content = match open_file(&path) {
        Ok(x) => {x},
        Err(err) => {
            eprint!("Failed opening the file located at {:?} Error:{:?}", path, err);
            return "".parse().unwrap();
        },
    };
    // Correct indentiation
    // let main_content = format!("\t{}", main_content.replace("\n", "\n\t"));
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

fn open_file(path: &str) -> std::io::Result<String> {
    println!("Trying to open path: {:?}", path);
    std::fs::read_to_string(path)
}