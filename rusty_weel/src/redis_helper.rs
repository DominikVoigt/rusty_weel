use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
    thread::{self, sleep, JoinHandle},
    time::Duration,
};

use crate::{
    connection_wrapper::ConnectionWrapper,
    data_types::{InstanceMetaData, StaticData},
    dsl_realization::{Error, Result},
};
use http_helper::Parameter;
use once::assert_has_not_been_called;
use redis::{Commands, Connection, RedisResult};
use rusty_weel_macro::get_str_from_value;
use serde_json::json;

const CALLBACK_RESPONSE_ERROR_MESSAGE: &str =
    "Callback-response had not the correct format, could not find whitespace separator";

/**
 * Manages a single TCP connection with redis
 */
pub struct RedisHelper {
    pub connection: redis::Connection,
}

impl RedisHelper {
    /** Tries to create redis connection.
     *  Panics if this fails
     * connection_name: Name of the connection displayed within the redis instance
     */
    pub fn new(static_data: &StaticData, connection_name: &str) -> Self {
        // TODO: Think about returning result instead of panic here.
        let connection = connect_to_redis(static_data, connection_name)
            .expect("Could not establish initial redis connection");

        Self { connection }
    }

    pub fn notify(
        &mut self,
        what: &str,
        content: Option<HashMap<String, String>>,
        instace_meta_data: InstanceMetaData,
    ) -> Result<()> {
        let mut content: HashMap<String, String> = content.unwrap_or(HashMap::new());
        // Todo: What should we put here? Json?
        content.insert(
            "attributes".to_owned(),
            serde_json::to_string(&instace_meta_data.attributes)
                .expect("Could not serialize attributes"),
        );
        let content =
            serde_json::to_string(&content).expect("Could not serialize content to json string");
        self.send("event", what, instace_meta_data, Some(content.as_str()))?;
        Ok(())
    }

    /**
     * Publishes messages on a channel (mainly to send to CPEE)
     * Channel is defined via <message_type>:<target>:<event>
     * The content of the message consists of instance metadata and the provided content
     * Meta data is provided via the InstanceMetaData
     * Providing content to the message is optional, the message otherwise contains {} for content
     */
    pub fn send(
        &mut self,
        message_type: &str,
        event: &str,
        instace_meta_data: InstanceMetaData,
        content: Option<&str>,
    ) -> Result<()> {
        // TODO: Handle target / workers
        let cpee_url = instace_meta_data.cpee_base_url;
        let instance_id = instace_meta_data.instance_id;
        let instance_uuid = instace_meta_data.instance_uuid;
        let info = instace_meta_data.info;
        let content = content.unwrap_or("{}");
        let target = "";
        let (topic, name) = event
            .split_once("/")
            .expect("event does not have correct structure: Misses / separator");
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
        // Construct complete payload out of: <instance-id> <actual-payload>
        let payload: String = format!(
            "{} {}",
            instance_id,
            serde_json::to_string(&payload).expect("Could not deserialize payload")
        );
        let publish_result: RedisResult<()> = self.connection.publish(channel, payload);
        match publish_result {
            Ok(()) => Ok(()),
            Err(error) => Err(Error::from(error)),
        }
    }

    pub fn extract_handler(&mut self, instance_id: &str, key: &str) -> HashSet<String> {
        self.connection
            .smembers(format!("instance:#{}/handlers/#{})", instance_id, key))
            .expect("Could not extract handlers")
    }

    /**
     * Will try to subscribe to the necessary topics, if this is not possible, it will panic
     * If it subscribed to the necessary topics, it will start a new thread that handles incomming redis messages
     * The thread receives a shared reference to the controller.
     * This method should be called exactly once, if it is called a second time, it will panic to prevent an accidental invokation.
     * If the thread fails to subscribe, it will panic
     * // TODO: Seems to be semantically equal now -> **Review later**
     */
    pub fn establish_callback_subscriptions(
        static_data: &StaticData,
        callback_keys: Arc<Mutex<HashMap<String, Arc<ConnectionWrapper>>>>,
    ) -> JoinHandle<Result<()>> {
        // Should only be called once in main!
        assert_has_not_been_called!();
        let connection: Connection;
        loop {
            // Create redis connection for subscriptions and their handling
            let connection_result = connect_to_redis(
                static_data,
                &format!(
                    "Callback subscription Instance: {}",
                    static_data.instance_id
                ),
            );

            match connection_result {
                Ok(conn) => {
                    connection = conn;
                    break;
                }
                Err(_) => {
                    log::error!("Could not establish redis connection for subscription, will retry in 10 milliseconds");
                    sleep(Duration::from_millis(100));
                }
            }
        }

        thread::spawn(move || -> Result<()> {
            let mut redis_helper = RedisHelper { connection };
            let topics = vec![
                "callback-response:*".to_owned(),
                "callback-end:*".to_owned(),
            ];
            redis_helper.blocking_pub_sub(topics, move |payload: &str, pattern: &str, topic: Topic| {
                match pattern {
                    "callback-response:*" => {
                        let callback_keys = callback_keys
                            .lock()
                            .expect("Could not lock mutex in callback thread");
                        if callback_keys.contains_key(&topic.identifier) {
                            let message_json = json!(payload);
                            if message_json["content"]["headers"].is_null()
                                || !message_json["content"]["headers"].is_object()
                            {
                                log_error_and_panic("message[content][headers] is either null, or ..[headers] is not a hash")
                            }
                            let params = construct_parameters(&message_json);
                            let headers = convert_headers_to_map(&message_json["content"]["headers"]);
                            callback_keys.get(&topic.identifier)
                                                // TODO: This panic will not result in termination -> Detached thread panics    
                                               .expect("Cannot happen as we check containment previously and hold mutex throughout")
                                               .callback(params, headers);
                        }
                    }
                    "callback-end:*" => {
                        callback_keys
                            .lock()
                            .expect("Mutex of callback_keys was poisoned")
                            .remove(&topic.identifier);
                    }
                    x => {
                        println!("Received on channel {} the payload: {}", x, payload);
                    }
                };
                // This should loop indefinitely
                Ok(true)
            })?;
            Ok(())
        })
    }

    /**
     * Uses the provided connection to establish a pub-sub channel
     * Waits for messages until the closure returns false (do not continue) or an error. Will return the error in the result enum
     * The handler gets 3 parameters:
     * - payload:   The actual payload (without the id in front)
     * - pattern:   The pattern structure that matched (e.g. "callback-response:*")
     * - topic:     The topic (e.g. "callback-response:01:<identifier>")
     *              Topic has to have the structure <prefix>:<worker>:<identifier>
     */
    pub fn blocking_pub_sub(
        &mut self,
        topic_patterns: Vec<String>,
        mut handler: impl FnMut(&str, &str, Topic) -> Result<bool>,
    ) -> Result<()> {
        let mut subscription = self.connection.as_pubsub();
        // will pushback message to self.waiting_messages of the PubSub instance
        match subscription.psubscribe(&topic_patterns) {
            Ok(()) => {}
            Err(err) => {
                log::error!("Could not subscribe to the topics: {err}");
                // This is unrecoverable -> panic thread
                panic!("Could not subscribe to the topics: {err}")
            }
        }

        // Handle incomming CPEE messages with the structure <instance_id> <payload>
        loop {
            let message: redis::Msg = subscription.get_message().expect("");
            // Payload structure: <instance-id> <actual-content>
            let payload: String = message
                .get_payload()
                .expect("Failed to get payload from message in callback thread");
            // cut of the instance-id in front of the actual message off
            let (_instance_id, payload) = payload
                .split_once(" ")
                .expect(CALLBACK_RESPONSE_ERROR_MESSAGE);
            let pattern: String = message
                .get_pattern()
                .expect("Could not get pattern  in callback thread");
            println!("Pattern: {}, Topic: {}, Payload: {}", pattern, message.get_channel_name(), payload);
            let topic: Topic = split_topic(message.get_channel_name());
            if !handler(payload, &pattern, topic)? {
                break;
            };
        }

        if let Err(err) = subscription.unsubscribe(topic_patterns) {
            log::error!("Could not unsubscribe from topics at the end: {}", err);
        }
        Ok(())
    }
}

/**
 * Creates redis connection
 * Each call creates a new redis connection and an underlying TCP connection
 * The URL format is redis://[<username>][:<password>@]<hostname>[:port][/<db>]
 * redis+unix:///<path>[?db=<db>[&pass=<password>][&user=<username>]]
 * unix:///<path>[?db=<db>][&pass=<password>][&user=<username>]]
 * // TODO: Check whether connection with socket and TCP works
 */
fn connect_to_redis(
    configuration: &StaticData,
    connection_name: &str,
) -> Result<redis::Connection> {
    // Note: Socket takes precedence as it is way faster
    let url = configuration
        .redis_path
        .as_ref()
        .or(configuration.redis_url.as_ref())
        .expect("Configuration contains neither a redis_url nor a redis_path")
        .clone();
    let mut connection = redis::Client::open(url)?.get_connection()?;
    match redis::cmd("CLIENT")
        .arg("SETNAME")
        .arg(connection_name)
        .query::<String>(&mut connection)
    {
        Ok(resp) => log::info!("Setting Client Name Response: {}", resp),
        Err(err) => log::error!("Error occured when setting client name: {}", err),
    };
    Ok(connection)
}

#[derive(Debug)]
pub struct Topic {
    pub prefix: String,
    pub worker: String,
    pub identifier: String,
}

fn split_topic(topic: &str) -> Topic {
    // Topic string should have structure: <prefix>:<worker-id>:<identifier>
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
fn convert_headers_to_map(headers_json: &serde_json::Value) -> HashMap<String, String> {
    let mut headers = HashMap::new();
    for (key, value) in headers_json
        .as_object()
        .expect("We checked for being object prior, so this should never happen")
        .iter()
    {
        let value = value.as_str().expect(
            format!(
                "Could not transform header value in object to string. Actual value: {:?}",
                value
            )
            .as_str(),
        );
        headers.insert(key.clone(), value.to_owned());
    }
    headers
}

/**
 * Constructs parameters from query *panics* if parameters cannot be constructed due to incorrect internal structure
 */
fn construct_parameters(message: &serde_json::Value) -> Vec<Parameter> {
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
                Some(Parameter::SimpleParameter {
                    name: header_name,
                    value: header_value,
                    param_type: http_helper::ParameterType::Body,
                })
            } else if param_type == "complex" {
                let name = get_str_from_value!(parameter[0]);
                let mime_type = get_str_from_value!(parameter[1][1]);
                let content_path = get_str_from_value!(parameter[1][2]);
                Some(Parameter::ComplexParameter {
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

mod test {
    use super::*;

    fn init_logger() {
        simple_logger::init_with_level(log::Level::Info).unwrap();
    }

    /**
     * Setup: Expects a redis instance running with a UNIX socket to be located at /run/redis.sock
     * Ensures that the UNIX socket connection works
     */
    #[test]
    fn test_connection_socket() {
        init_logger();
        let config = get_unix_socket_configuration();
        let mut connection = connect_to_redis(&config, "test_connection_unix").unwrap();
        assert_eq!(
            "test_connection_unix",
            redis::cmd("CLIENT")
                .arg("GETNAME")
                .query::<String>(&mut connection)
                .unwrap()
        );
    }

    /**
     * Setup: Redis instance running and listening on port 6379 (default port)
     * Ensures that the TCP connection works
     */
    #[test]
    fn test_connection_tcp() {
        init_logger();
        let config = get_tcp_configuration();
        let mut connection = connect_to_redis(&config, "test_connection_TCP").unwrap();
        assert_eq!(
            "test_connection_TCP",
            redis::cmd("CLIENT")
                .arg("GETNAME")
                .query::<String>(&mut connection)
                .unwrap()
        );
    }

    use log::error;
    use rand::{seq::SliceRandom, thread_rng};

    #[test]
    fn test_blocking_pub_sub() {
        init_logger();
        thread::spawn(|| {
            let mut connection =
                match connect_to_redis(&get_unix_socket_configuration(), "publisher") {
                    Ok(connection) => connection,
                    Err(err) => {
                        log::error!("Error creating publisher thread: {:?}", err);
                        panic!("error");
                    }
                };
            let instance_id = 6;
            let topic_ids = vec![1, 3, 5];
            loop {
                let topic_id = topic_ids.get(0).unwrap();
                // let topic_id = topic_ids.choose(&mut thread_rng()).unwrap()
                match connection.publish::<String, String, i32>(
                    format!("test_topic:01:{}", topic_id),
                    format!("{} {}", instance_id, "test_payload")
                ) {
                    Ok(_) => {},
                    Err(err) => error!("Error publishing: {err}"),
                }
                sleep(Duration::from_secs(3));
            }
        });

        let mut redis = RedisHelper::new(&get_unix_socket_configuration(), "pub_sub_test");
        redis
            .blocking_pub_sub(
                vec!["test_topic:*".to_owned()],
                |payload: &str, pattern: &str, topic: Topic| {
                    println!(
                        "Pattern: {}\nTopic: {}\nPayload:{:?}",
                        pattern, payload, topic
                    );
                    Ok(true)
                },
            )
            .unwrap();
    }

    fn get_unix_socket_configuration() -> StaticData {
        let home = std::env::var("HOME").unwrap();
        let expanded_path = format!("{}/redis/redis.sock", home);
        StaticData {
            instance_id: "test_id".to_owned(),
            host: "localhost".to_owned(),
            base_url: "localhost/cpee".to_owned(),
            redis_url: None,
            redis_path: Some(format!("unix://{}", expanded_path)),
            redis_db: 0,
            global_executionhandlers: "".to_owned(),
            executionhandlers: "".to_owned(),
            executionhandler: "".to_owned(),
            eval_language: "".to_owned(),
            eval_backend_url: "".to_owned(),
            attributes: HashMap::new(),
        }
    }

    fn get_tcp_configuration() -> StaticData {
        StaticData {
            instance_id: "test_id".to_owned(),
            host: "localhost".to_owned(),
            base_url: "localhost/cpee".to_owned(),
            // Default port
            redis_url: Some("redis://localhost:6379".to_owned()),
            redis_path: None,
            redis_db: 0,
            global_executionhandlers: "".to_owned(),
            executionhandlers: "".to_owned(),
            executionhandler: "".to_owned(),
            eval_language: "".to_owned(),
            eval_backend_url: "".to_owned(),
            attributes: HashMap::new(),
        }
    }
}
