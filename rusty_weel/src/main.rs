use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

use std::sync::{mpsc, Arc, Mutex};

use indoc::indoc;
use rusty_weel::data_types::ChooseVariant::{Exclusive, Inclusive};

use std::{panic, thread};

use rusty_weel::connection_wrapper::ConnectionWrapper;
use rusty_weel::data_types::{
    BlockingQueue, DynamicData, HTTPParams, KeyValuePair, State, StaticData, Status, ThreadInfo,
};
use rusty_weel::dsl::DSL;
use rusty_weel::dsl_realization::{Result, Weel};
use rusty_weel::redis_helper::RedisHelper;
// Needed for inject!
use http_helper::Method;
use rusty_weel_macro::inject;
use std::io::Write;

fn main() {
    let (stop_signal_sender, stop_signal_receiver) = mpsc::channel::<()>();
    let local_weel = startup(stop_signal_receiver);
    let weel_ref = local_weel.clone();
    let weel = move || weel_ref.clone();

    let model = move || -> Result<()> {
        //inject!("/home/i17/git-repositories/ma-code/rusty-weel/resources/model_instance.eic");
        // Inject start
        weel().call(
            "a1",
            "bookAir",
            HTTPParams {
                label: "Book Airline 1",
                method: Method::GET,
                arguments: Some(vec![
                    //new_key_value_pair("from", "data.from", true),
                    //new_key_value_pair("to", "data.to", true),
                    //new_key_value_pair("persons", "data.persons", true),
                ]),
            },
            Option::None,
            Option::None,
            Some(indoc! {r###"
            data.airline = result.value('id')
            data.costs += result.value('costs').to_f
            status.update 1, 'Hotel'
            "###}),
            Option::None,
        )?;

        weel().choose(Exclusive, || {
            weel().alternative("data.flag", || {
                weel().call(
                    "a2",
                    "timeout",
                    HTTPParams {
                        label: "Timeout 1",
                        method: Method::GET,
                        arguments: Some(vec![
                            new_key_value_pair("timeout", "5", false),
                        ]),
                    },
                    Option::None,
                    Option::None,
                    None,
                    Option::None,
                )?;
                Ok(())
            })?;
            weel().alternative("data.flag", || {
                weel().call(
                    "a4",
                    "timeout",
                    HTTPParams {
                        label: "Timeout 1",
                        method: Method::GET,
                        arguments: Some(vec![
                            new_key_value_pair("timeout", "5", false),
                        ]),
                    },
                    Option::None,
                    Option::None,
                    None,
                    Option::None,
                )?;
                Ok(())
            })?;
            weel().otherwise(|| {
                weel().call(
                    "a3",
                    "timeout",
                    HTTPParams {
                        label: "Timeout 2",
                        method: Method::GET,
                        arguments: Some(vec![
                            new_key_value_pair("timeout", "3", false),
                        ]),
                    },
                    Option::None,
                    Option::None,
                    None,
                    Option::None,
                )?;
                Ok(())
            })?;
            Ok(())
        })?;
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
    let res = local_weel.clone().start(model, stop_signal_sender);
    match res {
        Ok(_) => {},
        Err(err) => local_weel.handle_error(err),
    }
    log::info!("At the end of main");
}

fn startup(stop_signal_receiver: mpsc::Receiver<()>) -> Arc<Weel> {
    //simple_logger::init_with_level(log::Level::Info).unwrap();
    init_logger();
    set_panic_hook();

    let opts = StaticData::load("opts.yaml");
    let context = Mutex::new(DynamicData::load("context.json"));
    let callback_keys: Arc<Mutex<HashMap<String, Arc<Mutex<ConnectionWrapper>>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let redis_helper = match RedisHelper::new(&opts, "notifications") {
        Ok(redis) => redis,
        Err(err) => {
            log::error!("Error during startup when connecting to redis: {:?}", err);
            panic!("Error during startup")
        }
    };
    let attributes = match RedisHelper::new(&opts, "attributes") {
        Ok(mut redis) => match redis.get_attributes(opts.instance_id) {
            Ok(attributes) => attributes,
            Err(err) => {
                log::error!("Error during startup when connecting to redis: {:?}", err);
                panic!("Error during startup")
            }
        },
        Err(err) => {
            log::error!("Error during startup when connecting to redis: {:?}", err);
            panic!("Error during startup")
        }
    };
    let search_positions = get_search_positions(&context);
    let in_search_mode = !search_positions.is_empty();

    let weel = Weel {
        redis_notifications_client: Mutex::new(redis_helper),
        opts,
        search_positions: Mutex::new(search_positions),
        context,
        callback_keys,
        state: Mutex::new(State::Starting),
        status: Mutex::new(Status::new(0, "undefined".to_owned())),
        open_votes: Mutex::new(HashSet::new()),
        loop_guard: Mutex::new(HashMap::new()),
        positions: Mutex::new(Vec::new()),
        thread_information: Mutex::new(HashMap::new()),
        stop_signal_receiver: Mutex::new(stop_signal_receiver),
        attributes,
    };
    let current_thread = thread::current();

    // TODO: Think about the correct values. Some of them need to be determined dynamically I believe? E.g. search_mode and branch traces
    weel.thread_information.lock().unwrap().insert(
        current_thread.id(),
        RefCell::new(ThreadInfo {
            parent: None,
            in_search_mode: in_search_mode,
            switched_to_execution: false,
            no_longer_necessary: false,
            blocking_queue: Arc::new(Mutex::new(BlockingQueue::new())),
            // TODO: Unsure here
            branch_id: 0,
            branch_traces: HashMap::new(),
            branch_position: None,
            // This should not matter since we are not in a parallel yet
            branch_wait_count_cancel_condition: rusty_weel::data_types::CancelCondition::First,
            branch_wait_count_cancel_active: false,
            branch_wait_count_cancel: 0,
            branch_wait_count: 0,
            branch_event: None,
            // to here
            local: String::new(),
            branches: Vec::new(),
            alternative_mode: Vec::new(),
            alternative_executed: Vec::new()
        }),
    );

    // create thread for callback subscriptions with redis
    RedisHelper::establish_callback_subscriptions(&weel.opts, Arc::clone(&weel.callback_keys));
    let weel = Arc::new(weel);

    setup_signal_handler(&weel);
    let local_weel = Arc::clone(&weel);
    local_weel
}

fn init_logger() -> () {
    env_logger::Builder::from_default_env()
        .filter_level(log::LevelFilter::Debug)
        .format(|buf, record| {
            let style = buf.default_level_style(record.level());
            //buf.default_level_style(record.level());
            writeln!(
                buf,
                "{}:{} {} {style}[{}]{style:#} - {}",
                record.file().unwrap_or("unknown"),
                record.line().unwrap_or(0),
                chrono::Local::now().format("%Y-%m-%dT%H:%M:%S"),
                record.level(),
                record.args()
            )
        })
        .write_style(env_logger::WriteStyle::Auto)
        .filter_module("multipart", log::LevelFilter::Info)
        .init();
}

/**
 * Will get the search positions out of the provided context file
 */
fn get_search_positions(
    context: &Mutex<DynamicData>,
) -> HashMap<String, rusty_weel::dsl_realization::Position> {
    // TODO: implement this, for now we do not support it
    return HashMap::new();
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
        log::info!("Handling signal on thread: {:?}", thread::current().id());
        let res = weel.stop_weel();
        match res {
            Ok(_) => {
                log::info!("Successfuly executed stop function on weel")
            },
            Err(err) => {
                log::error!("Error occured when trying to stop: {:?}", err);
                panic!("Could not stop -> Crash instead of failing silently")
            }
        }
        log::info!("Set state to stopping");
    }) {
        panic!("Could not setup SIGINT/SIGTERM/SIGHUP handler: {err}")
    }
}

/**
 * Creates a new Key value pair by evaluating the key and value expressions (tries to resolve them in rust if they are simple data accessors)
 */
pub fn new_key_value_pair(
    key_expression: &'static str,
    value: &'static str,
    expression_value: bool,
) -> KeyValuePair {
    let key = key_expression;
    let value = Some(value.to_owned());
    KeyValuePair {
        key,
        value,
        expression_value,
    }
}
