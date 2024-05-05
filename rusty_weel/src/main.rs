use std::clone;

use indoc::indoc;
use rusty_weel::dsl::DSL;
use rusty_weel::dslrealization::Weel;
use rusty_weel::model::{KeyValuePair, Parameters, HTTP};
use rusty_weel_macro::inject;

fn main() {
    let data = ""; // TODO: Load data from file -> Maybe as a struct: holds data as a single string, if accessing field -> parses string for field
    let config = ""; // TODO: Load config from file
    let weel: Weel = Weel {};

    // Block included into main:
	// TODO: adapt it s.t. we can decode the endpoint url directly from json
	inject!("rusty_weel/src/model_instance.eic");
}
