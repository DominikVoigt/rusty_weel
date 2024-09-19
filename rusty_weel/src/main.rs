use std::collections::{HashMap, HashSet};
use std::sync::mpsc::Receiver;
use std::sync::{mpsc, Arc, Mutex};

use indoc::indoc;

use std::panic;

use rusty_weel::connection_wrapper::ConnectionWrapper;
use rusty_weel::dsl::DSL;
// Needed for inject!
use rusty_weel::data_types::{DynamicData, HTTPParams, KeyValuePair, State, StaticData, Status};
use rusty_weel::dsl_realization::{Weel, Error, Result};
use rusty_weel::eval_helper::{self, evaluate_expression};
use rusty_weel::redis_helper::RedisHelper;
use rusty_weel_macro::inject;
use reqwest::Method;

fn main() {
    simple_logger::init_with_level(log::Level::Info).unwrap();

    set_panic_hook();

    let static_data = StaticData::load("opts.yaml");
    let dynamic_data = Mutex::new(DynamicData::load("context.yaml"));
    let callback_keys: Arc<Mutex<HashMap<String, Arc<Mutex<ConnectionWrapper>>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let redis_helper = match RedisHelper::new(&static_data, "notifications") {
        Ok(redis) => redis,
        Err(err) => {log::error!("Error during startup when connecting to redis: {:?}", err); panic!("Error during startup")},
    };
    let (stopped_signal_sender, stopped_signal_receiver) = mpsc::channel::<()>();
    let weel = Weel {
        redis_notifications_client: Mutex::new(redis_helper),
        static_data,
        dynamic_data,
        callback_keys,
        state: Mutex::new(State::Starting),
        status: Mutex::new(Status::new(todo!(), todo!())),
        open_votes: Mutex::new(HashSet::new()),
        loop_guard: Mutex::new(HashMap::new()),
        positions: Mutex::new(Vec::new()),
        search_positions: todo!(),
        thread_information: todo!(),
        stop_signal_receiver
    };
    
    // create thread for callback subscriptions with redis
    RedisHelper::establish_callback_subscriptions(
        &weel.static_data,
        Arc::clone(&weel.callback_keys),
    );
    let weel = Arc::new(weel);
    let weel_clone = weel.clone();
    
    setup_signal_handler(&weel);
    let local_weel = Arc::clone(&weel);
    let weel = move || {
        weel_clone.clone()
    };
    let model = move || -> Result<()> {
        //inject!("/home/i17/git-repositories/ma-code/rusty-weel/resources/model_instance.eic");
        // Inject start
        weel().call(
            "a1",
            "bookAir",
            HTTPParams {
                label: "Book Airline 1",
                method: Method::POST,
                arguments: Some(vec![
                    new_key_value_pair("from", "data.from", true),
                    new_key_value_pair("to", "data.to", true),
                    new_key_value_pair("persons", "data.persons", true),
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
        )?;
        /*
        weel().parallel_do(Option::None, "last", move || -> Result<()> {
            weel().loop_exec(weel().pre_test("data.persons > 0"), || -> Result<()> {
                weel().parallel_branch(/*data,*/ || -> Result<()> {
                    weel().call(
                        "a2",
                        "bookHotel",
                        HTTPParams {
                            label: "Book Hotel",
                            method: Method::POST,
                            arguments: Some(vec![new_key_value_pair("to", "data.to")]),
                        },
                        Option::None,
                        Option::None,
                        Some(indoc! {r###"
                                data.hotels << result.value('id')
                                data.costs += result.value('costs').to_f
                            "###}),
                        Option::None,
                    )?;
                    Ok(())
                })?;
                weel().manipulate(
                    "a3",
                    Option::None,
                    indoc! {r###"
                    data.persons -= 1
                "###},
                )?;
                Ok(())
            })?;
            Ok(())
        })?;
         */
        // Inject end
        Ok(())
    };


    // Executes the code and blocks until it is finished
    local_weel.start(model, stopped_signal_sender);
}

fn set_panic_hook() -> () {
    let original_hook = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        // Log panic information in case we ever panic
        log::error!("Panic occured. Panic information: {info}");
        original_hook(info);
    }));
}

fn setup_signal_handler(weel: &Arc<Weel>) {
    let weel = Arc::clone(weel);
    
    if let Err(err) = ctrlc::set_handler(move || {
       log::info!("Received SIGINT/SIGTERM/SIGHUP. Set state to stopping...");
       let res = weel.stop();
       match res {
        Ok(_) => (),
        Err(err) => {
            log::error!("Error occured when trying to stop: {:?}", err);
            panic!("Could not stop -> Crash instgead of failing silently")
        },
        }
       log::info!("Set state to stopping");
    }) {
        panic!("Could not setup SIGINT/SIGTERM/SIGHUP handler: {err}")
    }
}

/**
 * Creates a new Key value pair by evaluating the key and value expressions (tries to resolve them in rust if they are simple data accessors)
 */
pub fn new_key_value_pair(key_expression: &'static str, value: &'static str, expression_value: bool) -> KeyValuePair {
    let key = key_expression;
    let value = Some(value.to_owned());
    KeyValuePair { key, value, expression_value }
}
