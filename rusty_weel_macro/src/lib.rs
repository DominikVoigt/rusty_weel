use proc_macro::TokenStream;

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

fn open_file(path: &str) -> String{
    println!("Trying to open path: {:?}", path);
    std::fs::read_to_string(path).unwrap()
}