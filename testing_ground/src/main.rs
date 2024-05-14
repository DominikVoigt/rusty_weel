use rusty_weel_macro::get_str_from_value_simple;


fn main() {
    let mut data = json::object!{
        foo: false,
        bar: null,
        answer: 42,
        answer2: "42",
        list: [null, "world", true]
    };

    let var: String = get_str_from_value_simple!(data["list"][1]);
    println!("{}", var);
}