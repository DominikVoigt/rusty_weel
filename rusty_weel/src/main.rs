use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::thread;

use indoc::indoc;
use rand::distributions::Alphanumeric;
use rand::Rng;
use serde_json;

use rusty_weel::connection_wrapper::ConnectionWrapper;
use rusty_weel::dsl::DSL;
// Needed for inject!
use rusty_weel::data_types::{DynamicData, HTTPRequest, KeyValuePair, State, StaticData, HTTP};
use rusty_weel::dslrealization::Weel;
use rusty_weel::eval_helper::evaluate_expressions;
use rusty_weel::redis_helper::{RedisHelper, Topic};
use rusty_weel_macro::{get_str_from_value, inject};


fn main() {
    simple_logger::init_with_level(log::Level::Warn).unwrap();

    let data = ""; // TODO: Load data from file -> Maybe as a struct: holds data as a single string, if accessing field -> parses string for field
                   // TODO: Use execution handler and inform of this issue

    let static_data = StaticData::load("opts.yaml");
    let dynamic_data = DynamicData::load("context.yaml");
    let callback_keys: Arc<Mutex<HashMap<String, Arc<Mutex<ConnectionWrapper>>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let weel = Weel {
        redis_notifications_client: Mutex::new(RedisHelper::new(&static_data, "notifications")),
        static_data,
        dynamic_data,
        callback_keys,
        state: State::Starting,
        open_votes: Mutex::new(HashSet::new()),
    };
    // create thread for callback subscriptions with redis
    RedisHelper::establish_callback_subscriptions(
        &weel.static_data,
        Arc::clone(&weel.callback_keys),
    );
    let weel = Arc::new(weel);
    let local_weel = Some(Arc::clone(&weel));

    let model = move || {
        // Block included into main:
        weel.call(
            "a1",
            "bookAir",
            HTTPRequest {
                label: "Book Airline 1",
                method: HTTP::POST,
                arguments: Some(vec![
                    new_key_value_pair("from", "data.from"),
                    new_key_value_pair("from", "data.to"),
                    new_key_value_pair("persons", "data.persons"),
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
                weel.parallel_branch(data, || {
                    weel.call(
                        "a2",
                        "bookHotel",
                        HTTPRequest {
                            label: "Book Hotel",
                            method: HTTP::POST,
                            arguments: Some(vec![new_key_value_pair("to", "data.to")]),
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
                        arguments: Some(vec![new_key_value_pair("costs", "data.costs")]),
                    },
                    Option::None,
                    Option::None,
                    Option::None,
                    Option::None,
                );
            })
        });
    };
    // Executes the code and blocks until it is finished
    local_weel.as_ref().expect("Instance is not populated").start(model);
}

/**
 * Creates a new Key value pair by evaluating the key and value expressions (tries to resolve them in rust if they are simple data accessors)
 */
pub fn new_key_value_pair(key_expression: &'static str, value: &'static str) -> KeyValuePair {
    let key = key_expression;
    let value = Some(value.to_owned());
    KeyValuePair { key, value }
}

pub fn new_key_value_pair_ex(
    key_expression: &'static str,
    value_expression: &'static str,
    static_data: &StaticData,
    dynamic_data: &DynamicData,
) -> KeyValuePair {
    let key = key_expression;
    let mut statement = HashMap::new();
    statement.insert("k".to_owned(), value_expression.to_owned());
    // TODO: Should we lock *context* here as mutex or just pass copy?
    let eval_result = match evaluate_expressions(
        static_data.eval_backend_url.as_str(),
        dynamic_data,
        static_data,
        statement,
    ) {
        Ok(eval_result) => match eval_result.get("k") {
            Some(x) => x.clone(),
            None => {
                log::error!("failure creating new key value pair. Evaluation failed");
                panic!("Failure creating KV pair.")
            }
        },
        Err(err) => {
            log::error!(
                "failure creating new key value pair. EvaluationError: {:?}",
                err
            );
            panic!("Failure creating KV pair.")
        }
    };

    let value = Option::Some(eval_result);
    KeyValuePair { key, value }
}
