use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::thread;

use indoc::indoc;
use rusty_weel::dsl::DSL;
// Needed for inject!
use rusty_weel::data_types::{DynamicData, HTTPRequest, InstanceMetaData, KeyValuePair, State, StaticData, HTTP};
use rusty_weel::eval_helper::evaluate_expressions;
use rusty_weel::redis_helper::RedisHelper;
use rusty_weel::dslrealization::Weel;
use rusty_weel_macro::inject;

fn main() {
    simple_logger::init_with_level(log::Level::Warn).unwrap();

    let data = ""; // TODO: Load data from file -> Maybe as a struct: holds data as a single string, if accessing field -> parses string for field
                   // TODO: Use execution handler and inform of this issue

    let static_data = StaticData::load("opts.yaml");
    let dynamic_data = DynamicData::load("context.yaml");
    let weel = Weel {
        static_data,
        dynamic_data,
        state: State::Starting
    }; 
    
    let weel = Arc::new(weel);

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
                weel.parallel_branch(data, |_local: &str| {
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
                        arguments: Some(vec![new_key_value_pair(
                            "costs",
                            "data.costs",
                        )]),
                    },
                    Option::None,
                    Option::None,
                    Option::None,
                    Option::None,
                );
            })
        });
    };
    let redis_helper = RedisHelper::new(&weel.static_data, Arc::new(Mutex::new(HashMap::new())));
    start(Mutex::new(redis_helper), model, weel.static_data.get_instance_meta_data());

}

/**
 * Starts execution
 * To pass it to execution thread we need Send + Sync
 */
fn start(redis_helper: Mutex<RedisHelper>, instance_code: impl Fn() + Send + Sync, instance_meta_data: InstanceMetaData) {
    let mut content = HashMap::new();
    content.insert("state".to_owned(), "running".to_owned());
    if vote(redis_helper, "state/change", content, instance_meta_data) {
        // We take the closure out of instance code and pass it to the thread -> transfer ownership of closure
        let instance_thread = thread::spawn(
            instance_code
        );
        // Take the join handle out of the member and join.
        instance_thread.join();
    } else {
        // TODO: What does
    }
}

// TODO: Implement stop
fn stop() {
}

// TODO: What is this supposed to do?
fn vote(redis_helper: Mutex<RedisHelper>, vote_topic: &str, mut content: HashMap<String, String>, configuration: &StaticData) -> bool {
    let (topic, name) = vote_topic
        .split_once("/")
        .expect("Vote topic did not contain / separator");
    let handler = format!("{}/{}/{}", topic, "vote", name);
    let mut votes: Vec<u128> = Vec::new();
    // TODO: Put redis_helper behind mutex here?
    redis_helper
        .lock()
        .expect("Could not acquire redis helper for voting")
        .extract_handler(&handler, &instance_meta_data.instance_id)
        .iter()
        .for_each(|client| {
            let vote_id: u128 = rand::random();
            content.insert("key".to_owned(), vote_id.to_string());
            content.insert("attributes".to_owned(), serde_json::to_string(&instance_meta_data.attributes).expect("Could not serialize attributes"));
            content.insert("subscription".to_owned(), client.clone());
            let votes = &mut votes;
            votes.push(vote_id);
            redis_helper.lock().expect("Could not lock helper").send(instance_meta_data, "vote", vote_topic, &content)
        });

    // TODO: Where to hold votes now? -> Weel? redis helper?
    if votes.len() > 0 {
        //self.votes.lock()
        //    .expect("Could not lock votes")
        //    .append(&mut votes);
    }
    todo!()
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
    configuration: &StaticData,
    context: &DynamicData
) -> KeyValuePair {
    let key = key_expression;
    let mut statement = HashMap::new();
    statement.insert("k".to_owned(), value_expression.to_owned());
    // TODO: Should we lock *context* here as mutex or just pass copy?
    let eval_result = match evaluate_expressions(
        configuration.eval_backend_url.as_str(),
        context.data.clone(),
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

/**
 * This function is a helper function that is called if an unrecoverable error is happening.
 * This function will end in the program panicing but also includes some prior logging
 */
fn log_error_and_panic(log_msg: &str) -> ! {
    log::error!("{}", log_msg);
    panic!("{}", log_msg);
}
