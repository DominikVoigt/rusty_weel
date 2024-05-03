use std::clone;

use indoc::indoc;
use quote::quote;
use rusty_weel::dsl::DSL;
use rusty_weel::dslrealization::Weel;
use rusty_weel::model::{KeyValuePair, Parameters, HTTP};

fn main() {
    let data = ""; // TODO: Load data from file -> Maybe as a struct: holds data as a single string, if accessing field -> parses string for field
    let config = ""; // TODO: Load config from file
    let weel: Weel = Weel {};

    {main}
    // End bock included into main
}
