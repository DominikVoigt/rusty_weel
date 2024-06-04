use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
    thread::{self, sleep, JoinHandle},
    time::Duration,
};

use crate::data_types::{InstanceMetaData, KeyValuePair};
use crate::eval_helper::evaluate;
use http_helper::HTTPParameters;
use redis::{Commands, RedisError};
use rusty_weel_macro::get_str_from_value;
use serde_json::json;

use crate::{
    connection_wrapper::ConnectionWrapper,
    data_types::{Configuration, Context},
    redis_helper::RedisHelper,
};

enum State {
    Running,
    Stopping,
    Stopped,
}

/**
 * Controller is central to the instance execution
 * It interfaces directly with Redis and thus manages ALL communication of the Weel Instance with the CPEE:
 *  - Status Updates
 *  - Callbacks
 *
 * The controller also takes any interrrupts and provides the correct signals to the weel instance (via state change) to halt execution when requested.
 */
pub struct Controller {
    configuration: Configuration,
    context: Context,
    redis_helper: RedisHelper,
    votes: Vec<String>, // Not sure yet
    callback_keys: Arc<Mutex<HashMap<String, Arc<Mutex<ConnectionWrapper>>>>>,
    id: String,
    redis_subscription_thread: Option<JoinHandle<()>>,
    loop_guard: HashMap<String, String>,
    state: Arc<Mutex<State>>,
}

impl Controller {
    /**
     * Creates a new controller instance
     * Instance is returned as an Arc<Mutex> as it is shared between the calling thread
     * and the thread that is started within new to handle messages the controller subscribes to
     */
    pub fn new(instance_id: &str, configuration: Configuration, context: Context) -> Controller {
        let callback_keys = Arc::new(Mutex::new(HashMap::new()));
        let mut controller = Self {
            configuration,
            context,
            votes: Vec::new(),
            callback_keys: Arc::clone(&callback_keys),
            redis_helper: RedisHelper::new(&configuration, callback_keys),
            id: instance_id.to_owned(),
            redis_subscription_thread: Option::None,
            loop_guard: HashMap::new(),
            state: Arc::new(Mutex::new(State::Running)),
        };
        controller
    }

    /**
     * //TODO: What is what
     */
    fn notify(&mut self, what: &str, content: Option<HashMap<String, String>>) {
        let mut content: HashMap<String, String> =
            content.unwrap_or_else(|| -> HashMap<String, String> { HashMap::new() });
        content.insert("attributes".to_owned(), self.translate_attributes());
        self.send("event", what, content);
    }

    // TODO: Check whether this works as intended, what should be returned
    /**
     * Checks attributes entries for expressions (which are prefixed by !) and sends them to the evaluation backend to be evaluated
     * The expressions are then replaced with their evaluated values
     */
    fn translate_attributes(&mut self) -> String {
        let mut statements = HashMap::new();
        self.configuration.attributes.iter().for_each(|(k, v)| {
            if v.starts_with("!") {
                statements.insert(k.to_owned(), v[1..].to_owned());
            }
        });

        // Evaluate expressions
        let eval_result = evaluate(
            self.configuration.eval_backend_url.as_str(),
            self.context.data.clone(),
            statements,
        );

        let evaluations = match eval_result {
            Ok(x) => x,
            Err(err) => {
                log::error!(
                    "failure creating new key value pair. EvaluationError: {:?}",
                    err
                );
                panic!("Failure creating KV pair.")
            }
        };

        // Replace expressions with values in attributes
        evaluations.iter().for_each(|(k, v)| {
            let k = k.to_owned();
            let v = v.to_owned();
            self.configuration.attributes.insert(k, v);
        });
    }

    fn send(&mut self, message_type: &str, event: &str, content: HashMap<String, String>) -> () {
        self.redis_helper.send(self.get_instance_meta_data(), message_type, event, content)
    }

    fn start(&self) {
        let mut content = HashMap::new();
        content.insert("state".to_owned(), "running".to_owned());
        if self.vote("state/change", content) {
            // TODO: Implement starting
        }
    }

    // TODO: Implement stop
    fn stop(&self) {
        todo!()
    }

    // TODO: Check this
    fn vote(&self, vote_topic: &str, content: HashMap<String, String>) -> bool {
        let (topic, name) = vote_topic
            .split_once("/")
            .expect("Vote topic did not contain / separator");
        let handler = format!("{}/{}/{}", topic, "vote", name);
        let mut votes: Vec<u128> = Vec::new();
        self.redis_helper
            .extract_handler(&handler, &self.id)
            .iter()
            .for_each(|client| {
                let vote_id: u128 = rand::random();
                content.insert("key".to_owned(), vote_id.to_string());
                content.insert("attributes".to_owned(), self.translate_attributes());
                content.insert("subscription".to_owned(), client.clone());
                let votes = &mut votes;
                votes.push(vote_id);
                self.send("vote", vote_topic, content)
            });


    }

    fn get_instance_meta_data(&self) -> InstanceMetaData {
        InstanceMetaData {
            cpee_base_url: self.base_url().to_owned(),
            instance_id: self.id,
            instance_url: self.instance_url(),
            instance_uuid: self.uuid().to_owned(),
            info: self.info().to_owned(),
        }
    }

    fn uuid(&self) -> &str {
        self.configuration
            .attributes
            .get("uuid")
            .expect("Attributes do not contain uuid")
    }

    fn info(&self) -> &str {
        self.configuration
            .attributes
            .get("info")
            .expect("Attributes do not contain info")
    }

    fn host(&self) -> &str {
        self.configuration.host.as_str()
    }

    fn base_url(&self) -> &str {
        self.configuration.base_url.as_str()
    }

    fn instance_url(&self) -> String {
        let mut path = PathBuf::from(self.base_url());
        path.push(self.id.clone());
        path.to_str()
            .expect("Path to instance is not valid UTF-8")
            .to_owned()
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
        &self,
        key_expression: &'static str,
        value_expression: &'static str,
    ) -> KeyValuePair {
        let key = key_expression;
        let mut statement = HashMap::new();
        statement.insert("k".to_owned(), value_expression.to_owned());
        // TODO: Should we lock context here as mutex or just pass copy?
        let eval_result = match evaluate(
            self.configuration.eval_backend_url.as_str(),
            self.context.data.clone(),
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
}

/**
 * This function is a helper function that is called if an unrecoverable error is happening.
 * This function will end in the program panicing but also includes some prior logging
 */
fn log_error_and_panic(log_msg: &str) -> ! {
    log::error!("{}", log_msg);
    panic!("{}", log_msg);
}
