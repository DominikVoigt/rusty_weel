use indoc::indoc;
use rusty_weel::controller::Controller;
use rusty_weel::dsl::DSL;
// Needed for inject!
use rusty_weel::data_types::{Configuration, Context, HTTPRequest, HTTP};
use rusty_weel::dslrealization::Weel;
use rusty_weel_macro::inject;

fn main() {
    simple_logger::init_with_level(log::Level::Warn).unwrap();
    let cwd = std::env::current_dir()
        .expect("could not get cwd")
        .to_str()
        .expect("could not get cwd")
        .to_owned();
    // TODO: Add instance id
    let instance_id = cwd;
    println!("Current working directory is: {}", instance_id);
    let data = ""; // TODO: Load data from file -> Maybe as a struct: holds data as a single string, if accessing field -> parses string for field
                   // TODO: Use execution handler and inform of this issue

    let configuration = Configuration::load_configuration("opts.yaml");
    let context = Context::load_context("context.yaml");

    let controller = Arc::new(Controller::new(instance_id.as_str(), configuration, context));
    let weel = Weel { controller: controller };
    controller.
    let weel = Arc::new(weel);
    weel.controller.instance;
    // Block included into main:
    let model = || {
        inject!("rusty_weel/src/model_instance.eic");

        // Block included into main:
        weel.call(
            "a1",
            "bookAir",
            HTTPRequest {
                label: "Book Airline 1",
                method: HTTP::POST,
                arguments: Some(vec![
                    Controller::new_key_value_pair("from", "data.from"),
                    Controller::new_key_value_pair("from", "data.to"),
                    Controller::new_key_value_pair("persons", "data.persons"),
                ]),
            },
            Option::None,
            Option::None,
            Some(indoc! {r###"
                data.airlone = result.value(\'id')
                data.costs += result.value('costs').to_f
                status.update 1, 'Hotel'
            "###}),
        Option::None,
        );
        weel.parallel_do(Option::None, "last", || {
            weel.loop_exec(weel.pre_test("data.persons > 0"), || {
                weel.parallel_branch(data, |_local: &str| {
                    weel.call(
                        "a2",
                        "bookHotel",
                        HTTPRequest {
                            label: "Book Hotel",
                            method: HTTP::POST,
                            arguments: Some(vec![Controller::new_key_value_pair("to", "data.to")]),
                        },
                        Option::None,
                        Option::None,
                        Some(indoc! {r###"
                                data.hotels << result.value('id')
                                data.costs += result.value('costs').to_f
                            "###}),
                        Option::None,
                    );
                });
                weel.manipulate(
                    "a3",
                    Option::None,
                    indoc! {r###"
                    data.persons -= 1
                "###},
                )
            })
        });
        weel.choose("exclusive", || {
        weel.alternative("data.costs > 700", || {
            weel.call(
                "a4",
                "approve",
                HTTPRequest {
                    label: "Approve Hotel",
                    method: HTTP::POST,
                    arguments: Some(vec![Controller::new_key_value_pair("costs", "data.costs")]),
                },
                Option::None,
                Option::None,
                Option::None,
                Option::None,
            );
        })
        });
    };
}
