use std::{
    collections::HashMap, path::PathBuf, sync::{Arc, Mutex}, thread::{self, sleep, JoinHandle}, time::Duration
};

use http_helper::HTTPParameters;
use redis::RedisError;
use rusty_weel_macro::get_str_from_value;
use serde_json::json;

use crate::{
    connection_wrapper::ConnectionWrapper, data_types::Configuration, dslrealization::Weel,
};

const TOPICS: &[&str] = &["callback-response:*", "callback-end:*"];
const CALLBACK_RESPONSE_ERROR_MESSAGE: &str =
    "Callback-response had not the correct format, could not find whitespace separator";

pub struct Controller {
    pub instance: Weel,
    configuration: Configuration,
    redis_connection: redis::Connection,
    votes: Vec<String>, // Not sure yet
    callback_keys: Arc<Mutex<HashMap<String, Arc<Mutex<ConnectionWrapper>>>>>,
    id: String,
    attributes: HashMap<String, String>,
    instance_execution_thread: Option<JoinHandle<()>>,
    redis_subscription_thread: Option<JoinHandle<()>>,
    loop_guard: HashMap<String, String>,
}

impl Controller {
    /**
     * Creates a new controller instance
     * Instance is returned as an Arc<Mutex> as it is shared between the calling thread
     * and the thread that is started within new to handle messages the controller subscribes to
     */
    pub fn new(instance_id: &str, configuration: Configuration) -> Controller {
        let controller = Controller {
            instance: Weel {},
            redis_connection: connect_to_redis(&configuration)
                .expect("Could not establish initial redis connection"),
            configuration: configuration,
            votes: Vec::new(),
            callback_keys: Arc::new(Mutex::new(HashMap::new())),
            id: instance_id.to_owned(),
            attributes: HashMap::new(),
            instance_execution_thread: Option::None,
            redis_subscription_thread: Option::None,
            loop_guard: HashMap::new(),
        };

        let mut controller = controller;
        // Here the mutex is created so we can lock and unwrap directly
        loop {
            match controller.try_establish_subscriptions() {
                Ok(_) => break,
                Err(_) => {
                    log::error!("Could not establish redis connection for subscription, will retry in 10 milliseconds");
                    sleep(Duration::from_millis(100))
                }
            }
        }
        controller
    }

    /**
     * Will try to subscribe to the necessary topics, if this is not possible, it will panic
     * If it subscribed to the necessary topics, it will start a new thread that handles incomming redis messages
     * The thread receives a shared reference to the controller.
     * If the thread fails to subscribe, it will currently panic!
     * // TODO: Seems to be semantically equal now -> **Review later**
     * // TODO: Handle issue of redis not connecting
     */
    fn try_establish_subscriptions(&mut self) -> Result<(), RedisError> {
        // Create redis connection for subscriptions and their handling
        let mut redis_connection = match connect_to_redis(&self.configuration) {
            Ok(redis_connection) => redis_connection,
            Err(err) => return Err(err),
        };

        let callback_keys = Arc::clone(&self.callback_keys);
        self.redis_subscription_thread = Some(thread::spawn(move || {
            // Move redis connection and callbacks reference into this thread
            let mut redis_subscription = redis_connection.as_pubsub();
            // will pushback message to self.waiting_messages of the PubSub instance
            match redis_subscription.psubscribe(TOPICS) {
                Ok(_) => {}
                Err(_) => return,
            }

            loop {
                let message: redis::Msg = redis_subscription.get_message().expect("");
                let payload: String = message
                    .get_payload()
                    .expect("Failed to get payload from message in callback thread");
                let pattern: String = message
                    .get_pattern()
                    .expect("Could not get pattern  in callback thread");
                match pattern.as_str() {
                    "callback-response:*" => {
                        let topic: Topic = split_topic(message.get_channel_name());
                        let callback_keys_guard = callback_keys
                            .lock()
                            .expect("Could not lock mutex in callback thread");
                        if callback_keys_guard.contains_key(&topic.identifier) {
                            let (_instance_id, message) = payload
                                .split_once(" ")
                                .expect(CALLBACK_RESPONSE_ERROR_MESSAGE);

                            // Parse message into json
                            let message_json = json!(message);
                            if message_json["content"]["headers"].is_null()
                                || !message_json["content"]["headers"].is_object()
                            {
                                log_error_and_panic("message[content][headers] is either null, or ..[headers] is not a hash")
                            }
                            let params = construct_parameters(&message_json);
                            callback_keys_guard.get(&topic.identifier)
                                               .expect("Cannot happen as we check containment previously and hold mutex throughout")
                                               .lock()
                                               .expect("Could not lock connection wrapper mutex")
                                               .callback(params, convert_headers_to_map(&message_json["content"]["headers"]));
                        }
                    }
                    "callback-end:*" => {
                        let topic: Topic = split_topic(message.get_channel_name());
                        callback_keys
                            .lock()
                            .expect("Mutex of callback_keys was poisoned")
                            .remove(&topic.identifier);
                    }
                    x => {
                        println!("Received on channel {} the payload: {}", x, payload)
                    }
                }
            }
        }));
        Ok(())
    }

    fn notify(&self) {

    }

    fn uuid(&self) -> String {
        self.attributes.get("uuid").expect("Attributes do not contain uuid").to_owned()
    }

    fn attributes_translated(&self) {
        todo!()
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
        path.to_str().expect("Path to instance is not valid UTF-8").to_owned()
    }
}

/**
 * Creates redis connection
 * Each call creates a new redis connection and an underlying TCP connection
 * The URL format is redis://[<username>][:<password>@]<hostname>[:port][/<db>]
 * redis+unix:///<path>[?db=<db>[&pass=<password>][&user=<username>]]
 * unix:///<path>[?db=<db>][&pass=<password>][&user=<username>]]
 */
fn connect_to_redis(configuration: &Configuration) -> Result<redis::Connection, RedisError> {
    // TODO: make real connection here
    match redis::Client::open("") {
        Ok(client) => match client.get_connection() {
            Ok(connection) => Ok(connection),
            Err(err) => Err(err),
        },
        Err(err) => Err(err),
    }
}

fn convert_headers_to_map(message_json: &serde_json::Value) -> HashMap<String, String> {
    message_json.as_object()
                .expect("We checked for being object prior, so this should never happen")
                .iter()
                .map(|(key, value)| {
                    (key.to_owned(), value.as_str().expect(format!("Could not transform header value in object to string. Actual value: {:?}", value).as_str()).to_owned())
                })
                .collect()
}

/**
 * Constructs parameters from query *panics* if parameters cannot be constructed due to incorrect internal structure
 */
fn construct_parameters(message: &serde_json::Value) -> Vec<HTTPParameters> {
    // Values should be an array of values
    let values = match message["content"]["values"].as_array() {
        Some(x) => x,
        None => {
            log_error_and_panic("content.values of callback response is not an array");
        }
    };
    values
        .iter()
        .filter_map(|parameter| {
            if parameter[0].is_null() || parameter[1][0].is_null() || parameter[1][1].is_null() {
                log_error_and_panic(
                    "one of the values within the callback response content.values is null",
                );
            }
            let param_type = get_str_from_value!(parameter[1][0]);
            if param_type == "simple" {
                let header_name = get_str_from_value!(parameter[0]);
                let header_value = get_str_from_value!(parameter[1][1]);
                Some(HTTPParameters::SimpleParameter {
                    name: header_name,
                    value: header_value,
                    param_type: http_helper::ParameterType::Body,
                })
            } else if param_type == "complex" {
                let name = get_str_from_value!(parameter[0]);
                let mime_type = get_str_from_value!(parameter[1][1]);
                let content_path = get_str_from_value!(parameter[1][2]);
                Some(HTTPParameters::ComplexParamter {
                    name,
                    mime_type,
                    content_handle: std::fs::File::open(content_path)
                        .expect("Could not open file for complex param in callback thread"),
                })
            } else {
                log::warn!(
                    "Could not construct paramter out of callback response as the type was: {:?}",
                    parameter[1][0]
                );
                None
            }
        })
        .collect()
}

struct Topic {
    prefix: String,
    worker: String,
    identifier: String,
}

fn split_topic(topic: &str) -> Topic {
    let mut topic: Vec<String> = topic.split(":").map(String::from).collect();
    Topic {
        prefix: topic.remove(0),
        worker: topic.remove(1),
        identifier: topic.remove(2),
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
