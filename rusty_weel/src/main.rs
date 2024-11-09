#![allow(unused_imports)]
#![allow(uncommon_codepoints)]
// We allow unused imports here as they depend on the injected code
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

use std::sync::{mpsc, Arc, Mutex};

use indoc::indoc;
use lazy_static::lazy_static;
use rusty_weel::data_types::CancelCondition::{First, Last};
use rusty_weel::data_types::ChooseVariant::{Exclusive, Inclusive};

use std::{panic, thread};

use http_helper::Method;
use rusty_weel::connection_wrapper::ConnectionWrapper;
use rusty_weel::data_types::{
    CancelCondition, Context, HTTPParams, KeyValuePair, State, Opts, Status, ThreadInfo,
};
use rusty_weel::dsl::DSL;
use rusty_weel::dsl_realization::{Position, Result, Weel};
use rusty_weel::redis_helper::RedisHelper;
use rusty_weel_macro::inject;
use std::io::Write;

lazy_static! {
    static ref WEEL: Arc<Weel> = startup();
}

fn main() {
    let (stop_signal_sender, stop_signal_receiver) = mpsc::channel::<()>();
    *WEEL.stop_signal_receiver.lock().unwrap() = Some(stop_signal_receiver);
    let model = || -> Result<()> {
        inject!();
        Ok(())
    };

    // Executes the code and blocks until it is finished
    let res = weel!().start(model, stop_signal_sender);
    match res {
        Ok(_) => {}
        Err(err) => weel!().handle_error(err),
    }
    log::info!("At the end of main");
    log::info!("Data elements are now: {:?}", weel!().context.lock().unwrap());
    log::info!("Thread info at end of main: {:?}", weel!().thread_information.lock().unwrap())
}

fn startup() -> Arc<Weel> {
    //simple_logger::init_with_level(log::Level::Info).unwrap();
    init_logger();
    set_panic_hook();

    let opts = Opts::load("opts.json");
    let context = Mutex::new(Context::load("context.json"));
    let callback_keys: Arc<Mutex<HashMap<String, Arc<Mutex<ConnectionWrapper>>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let redis_helper = match RedisHelper::new(&opts, "notifications") {
        Ok(redis) => redis,
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
        stop_signal_receiver: Mutex::new(None),
        critical_section_mutexes: once_map::OnceMap::new(),
    };
    let current_thread = thread::current();

    // TODO: Think about the correct values. Some of them need to be determined dynamically I believe? E.g. search_mode and branch traces
    let mut thread_info = ThreadInfo::default();
    thread_info.in_search_mode = in_search_mode;
    weel.thread_information
        .lock()
        .unwrap()
        .insert(current_thread.id(), RefCell::new(thread_info));

    // create thread for callback subscriptions with redis
    RedisHelper::establish_callback_subscriptions(&weel.opts, Arc::clone(&weel.callback_keys));
    let weel = Arc::new(weel);

    setup_signal_handler(&weel);
    let local_weel = Arc::clone(&weel);
    local_weel
}

fn init_logger() -> () {
    env_logger::Builder::from_default_env()
        .filter_level(log::LevelFilter::Error)
        .format(|buf, record| {
            let style = buf.default_level_style(record.level());
            //buf.default_level_style(record.level());
            writeln!(
                buf,
                "{}:{} {} {style}[{}]{style:#} - {}",
                record.file().unwrap_or("unknown"),
                record.line().unwrap_or(0),
                chrono::Local::now().format("%Y-%m-%dT%H:%M:%S:%.6f"),
                record.level(),
                record.args()
            )
        })
        .write_style(env_logger::WriteStyle::Auto)
        .filter_module("multipart", log::LevelFilter::Error)
        .init();
}

/**
 * Will get the search positions out of the provided context file
 */
fn get_search_positions(
    context: &Mutex<Context>,
) -> HashMap<String, rusty_weel::dsl_realization::Position> {
    context
        .lock()
        .unwrap()
        .search_positions
        .iter()
        .map(|(identifier, position_dto)| (identifier.clone(), Position::new(
            position_dto.position.clone(),
            position_dto.uuid,
            position_dto.detail.clone(),
            position_dto.handler_passthrough.clone()
        )))
        .collect()
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
    let main_thread_id = thread::current().id();

    if let Err(err) = ctrlc::set_handler(move || {
        println!("Received SIGINT/SIGTERM/SIGHUP. Set state to stopping...");
        let res = weel.stop_weel(main_thread_id);
        match res {
            Ok(_) => {
            }
            Err(_err) => {
                panic!("Could not stop -> Crash instead of failing silently")
            }
        }
    }) {
        panic!("Could not setup SIGINT/SIGTERM/SIGHUP handler: {err}")
    }
}

/**
 * Creates a new Key value pair by evaluating the key and value expressions
 */
pub fn new_key_value_pair_ex(
    key_expression: &'static str,
    value: &'static str,
) -> KeyValuePair {
    create_kv_pair(key_expression, value, true)
}

/**
 * Creates a new Key value pair
 */
pub fn new_key_value_pair(
    key_expression: &'static str,
    value: &'static str,
) -> KeyValuePair {
    create_kv_pair(key_expression, value, false)
}

/**
 * Creates a new Key value pair
 */
pub fn create_kv_pair(
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

#[macro_export]
macro_rules! weel {
    () => {
        WEEL.clone()
    };
}

#[macro_export]
macro_rules! ƛ {
    ($block: block) => {
        &Box::new(|| -> rusty_weel::dsl_realization::Result<()> {
            $block
            Ok(())
    })
    };
}

#[macro_export]
macro_rules! pƛ {
    ($block: block) => {
        std::sync::Arc::new(|| -> rusty_weel::dsl_realization::Result<()> {
            $block
            Ok(())
    })
    };
}

#[macro_export]
macro_rules! code {
    ($str: tt) => {
        Some(indoc::indoc!{
            $str
        })
    };
}

#[cfg(test)]
mod test {
    #[test]
    fn test_lambda() {
        let lambda = ƛ!({
            println!("Inside the lambda");
            let inner = ƛ!({ println!("Inside the lambda2") });
            matches!(inner(), Ok(()));
        });
        matches!(lambda(), Ok(()));
    }

    #[test]
    fn test_plambda() {
        let plambda = pƛ!({
            println!("Inside the plambda");
            let inner = pƛ!({ println!("Inside the plambda2") });
            matches!(inner(), Ok(()));
        });
        matches!(plambda(), Ok(()));
    }

    #[test]
    fn test_code_expand() {
        let a = code!(
            r##"
                Hello this is a test
            "##
        );

        println!("a is {:?}", a);
    }
}
