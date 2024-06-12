use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::thread;

use rand::distributions::Alphanumeric;
use rand::Rng;
use rusty_weel_macro::get_str_from_value;

use crate::connection_wrapper::ConnectionWrapper;
use crate::dsl::DSL;
use crate::data_types::{DynamicData, HTTPRequest, State, StaticData};
use crate::redis_helper::{RedisHelper, Topic};

pub struct Weel {
    pub static_data: StaticData,
    pub dynamic_data: DynamicData,
    pub state: State,
    pub callback_keys: Arc<Mutex<std::collections::HashMap<String, Arc<Mutex<ConnectionWrapper>>>>>,
    pub redis_notifications_client: Mutex<RedisHelper>,
    // Tracks all open votes via their ID. All voting needs to be finished before stopping.
    pub open_votes: Mutex<HashSet<String>>
}

impl Weel {
    /**
 * Starts execution
 * To pass it to execution thread we need Send + Sync
 */
pub fn start(&self, model: impl Fn() + Send + 'static) {
    let mut content = HashMap::new();
    content.insert("state".to_owned(), "running".to_owned());
    if self.vote("state/change", content) {
        // We take the closure out of instance code and pass it to the thread -> transfer ownership of closure
        let instance_thread = thread::spawn(model);
        // Take the join handle out of the member and join.
        instance_thread.join();
    } else {
        // TODO: What does
    }
}

// TODO: Implement stop
pub fn stop(&self) {

}

fn vote(&self, vote_topic: &str, mut content: HashMap<String, String>) -> bool {
    let static_data = &self.static_data;
    let (topic, name) = vote_topic
        .split_once("/")
        .expect("Vote topic did not contain / separator");
    let handler = format!("{}/{}/{}", topic, "vote", name);
    let mut votes: Vec<String> = Vec::new();
    let mut redis_helper: RedisHelper = RedisHelper::new(
        static_data,
        &format!(
            "Instance {} Vote | voting on: {}",
            static_data.instance_id, vote_topic
        ),
    );
    redis_helper
        .extract_handler(&handler, &static_data.instance_id)
        .iter()
        .for_each(|client| {
            // Generate random ASCII string of length VOTE_KEY_LENGTH
            let vote_id: String = generate_vote_key();
            content.insert("key".to_owned(), vote_id.to_string());
            content.insert(
                "attributes".to_owned(),
                serde_json::to_string(&static_data.attributes)
                    .expect("Could not serialize attributes"),
            );
            content.insert("subscription".to_owned(), client.clone());
            votes.push(vote_id);
            redis_helper.send(
                static_data.get_instance_meta_data(),
                "vote",
                vote_topic,
                &content,
            )
        });

    if votes.len() > 0 {
        self.open_votes
            .lock()
            .expect("could not lock votes")
            .extend(votes.clone());

        let topics = votes
        .iter()
        .map(|entry| format!("vote-response: {entry}"))
        .collect();

        let mut collected_votes = HashSet::new();
        redis_helper.blocking_pub_sub(topics, |payload: &str, _pattern: &str, _topic: Topic| {
            let message = serde_json::json!(payload);
            if message["content"].is_null() || message["name"].is_null() {
                log::error!("Message content or name is null");
                panic!("Message content or name is null")
            }
            // Check whether content directly contains boolean, otherwise look whether it is the text true, otherwise false
            collected_votes.insert(message["content"].as_bool().or(message["content"].as_str().map(|content| content == "true")).unwrap_or_else(|| false));
            self.open_votes.lock().expect("Could not lock votes ").remove(&get_str_from_value!(message["name"]));
            // TODO: should we really `cancel_callback m['name']` ?
            let all_votes_collected = collected_votes.len() >= votes.len();
            !all_votes_collected
        });
        !collected_votes.contains(&false)
    } else {
        true
    }
}
}

impl DSL for Weel {
    fn call(
        &self, 
        label: &str,
        endpoint_name: &str,
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

    fn parallel_do(&self, wait: Option<u32>, cancel: &str, start_branches: impl Fn() + Sync) {
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

    fn choose(&self, variant: &str, lambda: impl Fn() + Sync) {
        println!("Executing choose");
        lambda();
    }

    fn alternative(&self, condition: &str, lambda: impl Fn()+ Sync) {
        println!("Executing alternative, ignoring condition: {}", condition);
        lambda();
    }

    fn manipulate(&self, label: &str, name: Option<&str>, code: &str) {
        println!("Calling manipulate")
    }

    fn loop_exec(&self, condition: bool, lambda: impl Fn() + Sync) {
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

    fn critical_do(&self, mutex_id: &str, lambda: impl Fn() + Sync) {
        println!("in critical do");
        lambda();
    }
}

const VOTE_KEY_LENGTH: usize = 32;

/**
 * Generates random ASCII character string of length VOTE_KEY_LENGTH
 */
fn generate_vote_key() -> String {
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(VOTE_KEY_LENGTH)
        .map(char::from)
        .collect()
}