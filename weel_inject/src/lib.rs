use proc_macro::TokenStream;

/**
 * Injects the contents of the file provided into the file as if it was code.
 * Path can be either absolute or relative to the workspace root
 * Expects TokenStream to contain the path (single "string" expression)
 */
#[proc_macro]
pub fn inject(input: TokenStream) -> TokenStream {
    let mut path = input.to_string();
    if path.is_empty() {
        path = "./instance.rs".to_owned() 
    }
    // Strip "-symbol from string literal:
    //let path = input.to_string().replace("\"", "");
    let main_content = match open_file(&path) {
        Ok(x) => {x},
        Err(err) => {
            panic!("Failed opening the file located at {:?} Error:{:?}", path, err);
        },
    };
    // Correct indentiation
    // let main_content = format!("\t{}", main_content.replace("\n", "\n\t"));
    main_content.parse().unwrap()
}

fn open_file(path: &str) -> std::io::Result<String> {
    println!("Trying to open path: {:?}", path);
    std::fs::read_to_string(path)
}