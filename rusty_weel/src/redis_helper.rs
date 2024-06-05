use std::{collections::{HashMap, HashSet}, sync::{Arc, Mutex}, thread::{self, sleep, JoinHandle}, time::Duration};

use http_helper::HTTPParameters;
use redis::{Commands, RedisError};
use rusty_weel_macro::get_str_from_value;
use serde_json::json;
use crate::{connection_wrapper::ConnectionWrapper, data_types::{Static, InstanceMetaData}};



const TOPICS: &[&str] = &["callback-response:*", "callback-end:*"];
const CALLBACK_RESPONSE_ERROR_MESSAGE: &str =
    "Callback-response had not the correct format, could not find whitespace separator";

pub struct RedisHelper {
    redis_connection: redis::Connection,
    redis_subscription_thread: Option<JoinHandle<()>>,
}

impl RedisHelper {
    /** Tries to create redis connection. Panics if this fails */
    pub fn new(static_data: &Static, callback_keys: Arc<Mutex<HashMap<String, Arc<Mutex<ConnectionWrapper>>>>>) -> Self {
        // TODO: Think about returning result instead of panic here.
        let redis_connection = connect_to_redis(static_data)
                                        .expect("Could not establish initial redis connection");
        
        let mut redis_helper = Self { redis_connection, redis_subscription_thread: Option::None};

        // Here the mutex is created so we can lock and unwrap directly
        loop {
            match redis_helper.establish_subscriptions(static_data, Arc::clone(&callback_keys)) {
                Ok(_) => break,
                Err(_) => {
                    log::error!("Could not establish redis connection for subscription, will retry in 10 milliseconds");
                    sleep(Duration::from_millis(100))
                }
            }
        }
        redis_helper
    }

    /**
     * //TODO: What is what
     */
    fn notify(&mut self, what: &str, content: Option<HashMap<String, String>>, instace_meta_data: InstanceMetaData) {
        let mut content: HashMap<String, String> =
            content.unwrap_or_else(|| -> HashMap<String, String> { HashMap::new() });
        // Todo: What should we put here? Json?
        content.insert("attributes".to_owned(), serde_json::to_string(&instace_meta_data.attributes).expect("Could not serialize attributes"));
        self.send(instace_meta_data, "event", what, &content);
    }

    pub fn send(&mut self, instace_meta_data: InstanceMetaData, message_type: &str, event: &str, content: &HashMap<String, String>) -> () {
        let cpee_url = instace_meta_data.cpee_base_url;
        let instance_id = instace_meta_data.instance_id;
        let instance_uuid = instace_meta_data.instance_uuid;
        let info = instace_meta_data.info;

        let target = "";
        let (topic, name) = event.split_once("/")
                .expect("event does not have correct structure: Misses / separator");
        let content =
            serde_json::to_string(content).expect("Could not serialize content to json string");
        let payload = json!({
            "cpee": cpee_url,
            "instance-url": format!("{}/{}", cpee_url, instance_id),
            "instance": instance_id,
            "topic": topic,
            "type": message_type,
            "name": name,
            // Use ISO 8601 format
            "timestamp": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, false),
            "content": content,
            "instance-uuid": instance_uuid,
            "instance-name": info
        });
        let channel: String = format!("{}:{}:{}", message_type, target, event);
        let payload: String = format!(
            "{} {}",
            instance_id,
            serde_json::to_string(&payload).expect("Could not deserialize payload")
        );
        let publish_result: Result<(), redis::RedisError> =
            self.redis_connection.publish(channel, payload);
        if publish_result.is_err() {
            log_error_and_panic(
                format!(
                    "Error occured when publishing message to redis during send: {:?}",
                    publish_result.expect_err("Not possible to reach")
                )
                .as_str(),
            )
        }
    }

    pub fn extract_handler(&mut self, instance_id: &str, key: &str) -> HashSet<String> {
        self.redis_connection.smembers(format!("instance:#{}/handlers/#{})", instance_id, key)).expect("Could not extract handlers")
    }

    /**
     * Will try to subscribe to the necessary topics, if this is not possible, it will panic
     * If it subscribed to the necessary topics, it will start a new thread that handles incomming redis messages
     * The thread receives a shared reference to the controller.
     * If the thread fails to subscribe, it will currently panic!
     * // TODO: Seems to be semantically equal now -> **Review later**
     * // TODO: Handle issue of redis not connecting
     */
    fn establish_subscriptions(&mut self, configuration: &Static, callback_keys: Arc<Mutex<HashMap<String, Arc<Mutex<ConnectionWrapper>>>>>) -> Result<(), RedisError> {
        // Create redis connection for subscriptions and their handling
        let mut redis_connection = match connect_to_redis(configuration) {
            Ok(redis_connection) => redis_connection,
            Err(err) => return Err(err),
        };

        self.redis_subscription_thread = Some(thread::spawn(move || {
            // Move redis connection and callbacks reference into this thread
            let mut redis_subscription = redis_connection.as_pubsub();
            // will pushback message to self.waiting_messages of the PubSub instance
            match redis_subscription.psubscribe(TOPICS) {
                Ok(_) => {}
                Err(_) => return,
            }

            // Handle incomming messages
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
}

/**
 * Creates redis connection
 * Each call creates a new redis connection and an underlying TCP connection
 * The URL format is redis://[<username>][:<password>@]<hostname>[:port][/<db>]
 * redis+unix:///<path>[?db=<db>[&pass=<password>][&user=<username>]]
 * unix:///<path>[?db=<db>][&pass=<password>][&user=<username>]]
 */
fn connect_to_redis(configuration: &Static) -> Result<redis::Connection, RedisError> {
    // TODO: make real connection here
    match redis::Client::open("") {
        Ok(client) => match client.get_connection() {
            Ok(connection) => Ok(connection),
            Err(err) => Err(err),
        },
        Err(err) => Err(err),
    }
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

/**
 * Converts the headers in the callback response message into a hashmap (Key, Value) pairs
 */
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
