use derive_more::From;
use std::collections::{HashMap, HashSet};
use std::fmt::Debug;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::SystemTime;

use rand::distributions::Alphanumeric;
use rand::Rng;
use reqwest::header::ToStrError;
use rusty_weel_macro::get_str_from_value;

use crate::connection_wrapper::ConnectionWrapper;
use crate::data_types::{DynamicData, HTTPParams, State, StaticData};
use crate::dsl::DSL;
use crate::eval_helper::EvalError;
use crate::redis_helper::{RedisHelper, Topic};

pub struct Weel {
    pub static_data: StaticData,
    pub dynamic_data: DynamicData,
    pub state: Mutex<State>,
    // Contains all open callbacks from async connections, ArcMutex as it is shared between the instance (to insert callbacks) and the callback thread (RedisHelper)
    pub callback_keys: Arc<Mutex<std::collections::HashMap<String, Arc<ConnectionWrapper>>>>,
    pub redis_notifications_client: Mutex<RedisHelper>,
    // Tracks all open votes via their ID. All voting needs to be finished before stopping.
    pub open_votes: Mutex<HashSet<String>>,
    // Stores a count and the last access for each call
    pub loop_guard: Mutex<HashMap<String, (u32, SystemTime)>>,
}

impl DSL<Error> for Weel {
    fn call(
        &self,
        label: &str,
        endpoint_name: &str,
        parameters: HTTPParams,
        // Even though adding separate functions would be more idomatic for opt. parameters, the number and similar handling of these parameters would make it clunky to handle (2^4 variants)
        prepare_code: Option<&str>,
        update_code: Option<&str>,
        finalize_code: Option<&str>,
        rescue_code: Option<&str>,
    ) -> Result<()> {
        println!(
            "Calling activity {} with parameters: {:?}",
            label, parameters
        );
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
        todo!()
    }

    fn parallel_do(
        &self,
        wait: Option<u32>,
        cancel: &str,
        start_branches: impl Fn() + Sync,
    ) -> Result<()> {
        println!("Calling parallel_do");
        println!("Executing lambda");
        start_branches();
        todo!()
    }

    fn parallel_branch(&self, data: &str, lambda: impl Fn() + Sync) -> Result<()> {
        println!("Executing parallel branch");
        thread::scope(|scope| {
            scope.spawn(|| {
                lambda();
            });
        });
        todo!()
    }

    fn choose(&self, variant: &str, lambda: impl Fn() + Sync) -> Result<()> {
        println!("Executing choose");
        lambda();
        todo!()
    }

    fn alternative(&self, condition: &str, lambda: impl Fn() + Sync) -> Result<()> {
        println!("Executing alternative, ignoring condition: {}", condition);
        lambda();
        todo!()
    }

    fn manipulate(&self, label: &str, name: Option<&str>, code: &str) -> Result<()> {
        println!("Calling manipulate");
        todo!()
    }

    fn loop_exec(&self, condition: bool, lambda: impl Fn() + Sync) -> Result<()> {
        println!("Executing loop!");
        lambda();
        todo!()
    }

    fn pre_test(&self, condition: &str) -> bool {
        false
    }

    fn post_test(&self, condition: &str) -> bool {
        true
    }

    fn stop(&self, label: &str) -> Result<()> {
        println!("Stopping... just kidding");
        todo!()
    }

    fn critical_do(&self, mutex_id: &str, lambda: impl Fn() + Sync) -> Result<()> {
        println!("in critical do");
        lambda();
        todo!()
    }
}

impl Weel {
    /**
     * Starts execution
     * To pass it to execution thread we need Send + Sync
     */
    pub fn start(&self, model: impl Fn() -> Result<()> + Send + 'static) -> Result<()> {
        let mut content = HashMap::new();
        content.insert("state".to_owned(), "running".to_owned());
        if self.vote("state/change", content)? {
            // TODO: instance.start
            // We take the closure out of instance code and pass it to the thread -> transfer ownership of closure
            let instance_thread = thread::spawn(model);
            // Take the join handle out of the member and join.
            let result = instance_thread.join();
            // TODO: Handle the result, especially a WeelError -> All methods return weel error in case of a
            match result {
                Ok(_) => todo!(),
                Err(_) => todo!(),
            }
        } else {
            // TODO: instance.stop
        };
        Ok(())
    }

    // TODO: Implement stop
    pub fn stop(&self) -> Result<()> {
        /*
           ### tell the instance to stop
           @instance.stop
           ### end all votes or it will not work
           Thread.new do # doing stuff in trap context is a nono. but in a thread its fine :-)
           @votes.each do |key|
               CPEE::Message::send(:'vote-response',key,base,@id,uuid,info,true,@redis)
           end
           end
        */
        for vote_id in self
            .open_votes
            .lock()
            .expect("Could not capture mutex")
            .iter()
        {
            self.redis_notifications_client
                .lock()
                .expect("Could not acquire mutex")
                // TODO: Ignore error for now
                .send(
                    "vote-response",
                    vote_id,
                    self.static_data.get_instance_meta_data(),
                    Some("true"),
                )?;
        }
        Ok(())
    }

    fn vote(&self, vote_topic: &str, mut content: HashMap<String, String>) -> Result<bool> {
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
        for client in redis_helper
            .extract_handler(&handler, &static_data.instance_id)
            .iter()
        {
            // Generate random ASCII string of length VOTE_KEY_LENGTH
            let vote_id: String = generate_random_key();
            content.insert("key".to_owned(), vote_id.to_string());
            content.insert(
                "attributes".to_owned(),
                serde_json::to_string(&static_data.attributes)
                    .expect("Could not serialize attributes"),
            );
            content.insert("subscription".to_owned(), client.clone());
            let content = serde_json::to_string(&content)
                .expect("Could not serialize content to json string");
            votes.push(vote_id);
            redis_helper.send(
                "vote",
                vote_topic,
                static_data.get_instance_meta_data(),
                Some(content.as_str()),
            )?;
        }

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
            redis_helper.blocking_pub_sub(
                topics,
                |payload: &str, _pattern: &str, _topic: Topic| {
                    let message = serde_json::json!(payload);
                    if message["content"].is_null() || message["name"].is_null() {
                        log::error!("Message content or name is null");
                        panic!("Message content or name is null")
                    }
                    // Check whether content directly contains boolean, otherwise look whether it is the text true, otherwise false
                    collected_votes.insert(
                        message["content"]
                            .as_bool()
                            .or(message["content"].as_str().map(|content| content == "true"))
                            .unwrap_or(false),
                    );
                    self.open_votes
                        .lock()
                        .expect("Could not lock votes ")
                        .remove(&get_str_from_value!(message["name"]));
                    // TODO: should we really `cancel_callback m['name']` ?
                    self.cancel_callback(
                        message["name"]
                            .as_str()
                            .expect("Message does not contain name"),
                    )?;
                    let all_votes_collected = collected_votes.len() >= votes.len();
                    Ok(!all_votes_collected)
                },
            )?;
            Ok(!collected_votes.contains(&false))
        } else {
            Ok(true)
        }
    }

    pub fn callback(
        &self,
        hw: Arc<ConnectionWrapper>,
        key: &str,
        mut content: HashMap<String, String>,
    ) -> Result<()> {
        content.insert("key".to_owned(), key.to_owned());
        let content =
            serde_json::to_string(&content).expect("could not serialize hashmap to string");
        self.redis_notifications_client
            .lock()
            .expect("Could not acquire Mutex")
            .send(
                "callback",
                "activity/content",
                self.static_data.get_instance_meta_data(),
                Some(&content),
            )?;
        self.callback_keys
            .lock()
            .expect("could not acquire Mutex")
            .insert(key.to_owned(), hw.clone());
        Ok(())
    }

    fn cancel_callback(&self, key: &str) -> Result<()> {
        self.redis_notifications_client
            .lock()
            .expect("Could not acquire Mutex for notifications RedisHelper")
            .send(
                "callback-end",
                key,
                self.static_data.get_instance_meta_data(),
                None,
            )?;
        Ok(())
    }
}

#[derive(Debug, From)]
pub enum Error {
    SyntaxError(String),
    InvalidHeaderValue(reqwest::header::InvalidHeaderValue),
    InvalidHeaderName(reqwest::header::InvalidHeaderName),
    JsonError(serde_json::Error),
    ReqwestError(reqwest::Error),
    ToStrError(ToStrError),
    IOError(std::io::Error),
    RedisError(redis::RedisError),
    EvalError(EvalError),
    StrUTF8Error(std::str::Utf8Error),
    StringUTF8Error(std::string::FromUtf8Error),
    HttpHelperError(http_helper::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn as_str(&self) -> &str {
        match self {
            Error::SyntaxError(message) => message.as_str(),
            Error::JsonError(_) => todo!(),
            Error::IOError(_) => todo!(),
            Error::RedisError(_) => todo!(),
            Error::EvalError(_) => todo!(),
            Error::StrUTF8Error(_) => todo!(),
            Error::StringUTF8Error(_) => todo!(),
            Error::InvalidHeaderValue(_) => todo!(),
            Error::InvalidHeaderName(_) => todo!(),
            Error::ReqwestError(_) => todo!(),
            Error::ToStrError(_) => todo!(),
            Error::HttpHelperError(_) => todo!(),
        }
    }
}

const KEY_LENGTH: usize = 32;

/**
 * Generates random ASCII character string of length KEY_LENGTH
 */
pub fn generate_random_key() -> String {
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(KEY_LENGTH)
        .map(char::from)
        .collect()
}
