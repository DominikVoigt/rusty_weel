
fn main() {
    let main_skeleton = std::fs::read_to_string("src/main_skeleton.rs").unwrap();
    let main_content = std::fs::read_to_string("src/model_instance.eic").unwrap();

    let main_content = main_content.replace("\n", "\n\t");

    let main_skeleton: Vec<&str> = main_skeleton.split("{main}").collect();
    assert!(main_skeleton.len() == 2, "main_skeleton.rs is not formatted correctly");

    let main_file = format!("{}{}{}", main_skeleton.get(0).unwrap(), main_content, main_skeleton.get(1).unwrap());
    match std::fs::write("src/main.rs", main_file) {
        Ok(()) => {}
        Err(err) => println!("Error occured when writing file: {}", err)
    }
}