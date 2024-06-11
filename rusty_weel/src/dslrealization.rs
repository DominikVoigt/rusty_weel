use std::sync::Arc;
use std::thread;

use crate::dsl::DSL;
use crate::data_types::{DynamicData, HTTPRequest, State, StaticData};

pub struct Weel {
    pub static_data: StaticData,
    pub dynamic_data: DynamicData,
    pub state: State,
    pub callback_keys: Arc<std::sync::Mutex<std::collections::HashMap<String, Arc<std::sync::Mutex<crate::connection_wrapper::ConnectionWrapper>>>>>,
    pub redis_notifications_client: crate::redis_helper::RedisHelper
}

impl DSL for Weel {
    fn call(
        &self, 
        label: &str,
        endpoint_url_name: &str,
        parameters: HTTPRequest,
        // Even though adding separate functions would be more idomatic for opt. parameters, the number and similar handling of these parameters would make it clunky to handle (2^4 variants)
        prepare_code: Option<&str>,
        update_code: Option<&str>,
        finalize_code: Option<&str>,
        rescue_code: Option<&str>,
    ) {
        println!("Calling activity {} with parameters: {:?}", label, parameters);
        if let Some(x) = prepare_code {
            println!("Prepare code: {:?}", prepare_code);
        }
        if let Some(x) = update_code {
            println!("Prepare code: {:?}", update_code)
        }
        if let Some(x) = finalize_code {
            println!("Finalize code: {:?}", finalize_code)
        }
        if let Some(x) = rescue_code {
            println!("Rescue code: {:?}", rescue_code)
        }
    }

    fn parallel_do(&self, wait: Option<u32>, cancel: &str, start_branches: impl Fn()) {
        println!("Calling parallel_do");
        println!("Executing lambda");
        start_branches();
    }

    fn parallel_branch(&self, data: &str, lambda: impl Fn() + Sync) {
        println!("Executing parallel branch");
        thread::scope(|scope| {
            scope.spawn(|| {
                lambda();
            });
        });
    }

    fn choose(&self, variant: &str, lambda: impl Fn()) {
        println!("Executing choose");
        lambda();
    }

    fn alternative(&self, condition: &str, lambda: impl Fn()) {
        println!("Executing alternative, ignoring condition: {}", condition);
        lambda();
    }

    fn manipulate(&self, label: &str, name: Option<&str>, code: &str) {
        println!("Calling manipulate")
    }

    fn loop_exec(&self, condition: bool, lambda: impl Fn()) {
        println!("Executing loop!");
        lambda();
    }

    fn pre_test(&self, condition: &str) -> bool {
        false
    }

    fn post_test(&self, condition: &str) -> bool {
        true
    }

    fn stop(&self, label: &str) {
        println!("Stopping... just kidding")
    }

    fn critical_do(&self, mutex_id: &str, lambda: impl Fn()) {
        println!("in critical do");
        lambda();
    }
}
