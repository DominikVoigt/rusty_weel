use std::{
    collections::{HashMap, HashSet},
    io::{Seek, Write},
    panic::{set_hook, take_hook},
    sync::{Arc, Mutex},
    thread::{self, sleep, JoinHandle},
    time::Duration,
};

use crate::{
    connection_wrapper::ConnectionWrapper,
    data_types::{InstanceMetaData, Opts},
    dsl_realization::{Error, Result},
};
use http_helper::{Parameter, ParameterDTO};
use mime::Mime;
use once::assert_has_not_been_called;
use redis::{Commands, Connection, RedisResult};
use serde_json::{json, Value};

const CALLBACK_RESPONSE_ERROR_MESSAGE: &str =
    "Callback-response had not the correct format, could not find whitespace separator";

/**
 * Manages a single TCP connection with redis
 */
pub struct RedisHelper {
    pub connection: redis::Connection,
    number_workers: u32,
    last: u32,
}

impl RedisHelper {
    /** Tries to create redis connection.
     *  Panics if this fails
     * connection_name: Name of the connection displayed within the redis instance
     */
    pub fn new(static_data: &Opts, connection_name: &str) -> Result<Self> {
        let number_workers = if static_data.redis_workers > 99 {
            99
        } else {
            static_data.redis_workers
        };
        let connection = connect_to_redis(static_data, connection_name)?;

        Ok(Self {
            connection,
            number_workers,
            last: number_workers,
        })
    }

    fn target_worker(&mut self) -> u32 {
        let mut next = self.last + 1;
        // If we have 2 workers, our worker ids should be (0)0 and (0)1
        if next > (self.number_workers - 1) {
            next = 0;
        }
        self.last = next;
        next
    }

    /**
     * Sends a message to the redis DB
     */
    pub fn notify(
        &mut self,
        what: &str,
        content: Option<Value>,
        instace_meta_data: InstanceMetaData,
    ) -> Result<()> {
        let mut content = content.unwrap_or(json!({}));
        // TODO: Original code adds attributes_translated here, do we need to do this?
        content.as_object_mut().unwrap().insert(
            "attributes".to_owned(),
            json!(&instace_meta_data.attributes),
        );
        self.send("event", what, instace_meta_data, Some(content))?;
        Ok(())
    }

    /**
     * Publishes messages on a channel (pubsub) (mainly to send to CPEE)
     * Channel is defined via <message_type>:<target>:<event>
     * The content of the message consists of instance metadata and the provided content
     * Meta data is provided via the InstanceMetaData
     * Providing content to the message is optional, the message otherwise contains {} for content
     *
     * Will return an error if the publishing to redis fails
     */
    pub fn send(
        &mut self,
        message_type: &str,
        event: &str,
        instace_meta_data: InstanceMetaData,
        content: Option<Value>,
    ) -> Result<()> {
        let cpee_url = instace_meta_data.cpee_base_url;
        let instance_id = instace_meta_data.instance_id;
        let instance_uuid = instace_meta_data.instance_uuid;
        let info = instace_meta_data.info;
        let content = content.unwrap_or(Value::Null);
        let target_worker = self.target_worker();
        let target_worker = if target_worker < 10 {
            format!("0{target_worker}")
        } else {
            target_worker.to_string()
        };
        let (topic, name) = event
            // If no separator is contained e.g. in case for callback-end, no topic is provided
            .split_once("/")
            .unwrap_or(("", event));

        let mut payload = HashMap::new();
        payload.insert("cpee", Value::String(cpee_url.clone()));
        payload.insert(
            "instance-url",
            Value::String(format!("{}/{}", cpee_url, instance_id.clone())),
        );
        payload.insert("instance", Value::Number(instance_id.into()));
        payload.insert("topic", Value::String(topic.to_owned()));
        payload.insert("type", Value::String(message_type.to_owned()));
        payload.insert("name", Value::String(name.to_owned()));
        payload.insert(
            "timestamp",
            Value::String(
                chrono::Utc::now()
                    .to_rfc3339_opts(chrono::SecondsFormat::Millis, false)
                    .to_string(),
            ),
        );
        payload.insert("content", content);
        payload.insert("instance-uuid", Value::String(instance_uuid));
        payload.insert("instance-name", Value::String(info));

        let channel: String = format!("{}:{}:{}", message_type, target_worker, event);
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

    pub fn extract_handler(&mut self, instance_id: u32, key: &str) -> HashSet<String> {
        self.connection
            .smembers(format!("instance:{}/handlers/{})", instance_id, key))
            .expect("Could not extract handlers")
    }

    /**
     * Will try to subscribe to the necessary topics, if this is not possible, it will panic
     * If it subscribed to the necessary topics, it will start a new thread that handles incomming redis messages
     * The thread receives a shared reference to the controller.
     * This method should be called exactly once, if it is called a second time, it will panic to prevent an accidental invokation.
     */
    pub fn establish_callback_subscriptions(
        static_data: &Opts,
        callback_keys: Arc<Mutex<HashMap<String, Arc<Mutex<ConnectionWrapper>>>>>,
    ) -> JoinHandle<Result<()>> {
        // Should only be called once in main!
        assert_has_not_been_called!();
        let connection: Connection;
        let workers = static_data.redis_workers;
        let last = workers;

        loop {
            // Create redis connection for subscriptions and their handling
            let connection_result = connect_to_redis(
                static_data,
                &format!("Callback_Subscription_Instance:{}", static_data.instance_id),
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
            // We will terminate the process if callback thread panics
            let original_hook = take_hook();
            set_hook(Box::new(move |panic_info| {
                log::error!("{}", panic_info);
                original_hook(panic_info);
                std::process::exit(1)
            }));

            let mut redis_helper = RedisHelper {
                connection,
                number_workers: workers,
                last,
            };
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
                        if callback_keys.contains_key(&topic.event) {
                            let message: Value = serde_json::from_str(payload).unwrap();
                            if message["content"]["headers"].is_null() || !message["content"]["headers"].is_object()
                            {
                                log_error_and_panic("message[content][headers] is either null, or ..[headers] is not a hash")
                            }
                            log::debug!("Processing values: {:?}", message["content"]["values"]);
                            let values = message["content"]["values"].as_array().expect("Values received is not an array!");
                            let mut content = Vec::with_capacity(values.len());
                            for value in values {
                                if value[1][0] == "simple" {
                                    content.push(Parameter::SimpleParameter { name: value[0].as_str().unwrap().to_owned(), value: value[1][1].as_str().unwrap().to_owned(), param_type: http_helper::ParameterType::Body });
                                } else if value[1][0] == "complex" {
                                    let mime_type = match value[1][1].as_str().unwrap().parse::<Mime>() {
                                        Ok(mime) => mime,
                                        Err(err) => {
                                            log::error!("Failed parsing mimetype: {:?} from string: {}", err, value[1][1].as_str().unwrap().to_owned());
                                            panic!("Failed parsing mimetype")
                                        },
                                    };
                                    // TODO: Handle complex with path or file handle
                                    let mut file = tempfile::tempfile()?;
                                    file.write_all(value[1][1].as_str().unwrap().as_bytes())?;
                                    file.rewind()?;
                                    content.push(Parameter::ComplexParameter { name: value[0].as_str().unwrap().to_owned(), mime_type, content_handle: file });
                                }
                            };
                            // TODO: Determine whether we need this still: construct_parameters(&message_json);
                            let headers = convert_headers_to_map(&message["content"]["headers"]);
                            callback_keys.get(&topic.event)
                                         .expect("Cannot happen as we check containment previously and hold mutex throughout")
                                         .lock()?
                                         // TODO: Maybe add status to message too?
                                         .handle_callback(None, crate::data_types::CallbackType::Structured(content), headers)?;
                        }
                    }
                    "callback-end:*" => {
                        callback_keys
                            .lock()
                            .expect("Mutex of callback_keys was poisoned")
                            .remove(&topic.type_);
                    }
                    x => {
                        log::error!("Received on channel {} the payload: {}", x, payload);
                    }
                };
                // This should loop indefinitely:
                Ok(true)
            })?;
            Ok(())
        })
    }

    /**
     * Uses the provided connection to establish a pub-sub channel
     * Waits for messages until the closure returns false (do not continue) or an error. Will return the error in the result enum
     *
     * The handler gets 3 parameters:
     * - payload:   The actual payload (without the id in front)
     * - pattern:   The pattern structure that matched (e.g. "callback-response:*")
     * - topic:     The topic (e.g. "callback-response:01:<identifier>")
     *              Topic has to have the structure <message_type>:<target>:<event>
     *
     * Will return an error if the un-subscribing fails
     */
    pub fn blocking_pub_sub(
        &mut self,
        topic_patterns: Vec<String>,
        mut handler: impl FnMut(&str, &str, Topic) -> Result<bool>,
    ) -> Result<()> {
        let mut subscription = self.connection.as_pubsub();
        // will pushback message to self.waiting_messages of the PubSub instance
        subscription.psubscribe(&topic_patterns)?;

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
            let pattern: String = message.get_pattern().expect("Could not get pattern");

            let topic: Topic = split_topic(message.get_channel_name())?;
            if !handler(payload, &pattern, topic)? {
                break;
            };
        }

        if let Err(err) = subscription.unsubscribe(topic_patterns) {
            log::error!("Could not unsubscribe from topics at the end: {}", err);
            return Err(Error::from(err));
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
 */
fn connect_to_redis(configuration: &Opts, connection_name: &str) -> Result<redis::Connection> {
    // Note: Socket takes precedence as it is way faster
    let url = configuration
        .redis_path
        .as_ref()
        .or(configuration.redis_url.as_ref())
        .expect("Configuration contains neither a redis_url nor a redis_path")
        .clone();
    let mut connection = redis::Client::open(url)?.get_connection()?;
    let connection_name = connection_name.replace(" ", "-");
    match redis::cmd("CLIENT")
        .arg("SETNAME")
        .arg(&connection_name)
        .query::<String>(&mut connection)
    {
        Ok(_resp) => {}
        Err(err) => log::error!("Error occured when setting client name: {}", err),
    };
    Ok(connection)
}

#[derive(Debug)]
pub struct Topic {
    // Since type is a keyword
    pub type_: String,
    pub worker: String,
    pub event: String,
}

/**
 * In the cpee each topic has a fixed structure:
 * <message_type>:<target>:<event>
 */
fn split_topic(topic: &str) -> Result<Topic> {
    // Topic string should have structure: <prefix>:<worker-id>:<identifier>
    let mut topic: Vec<String> = topic.split(":").map(String::from).collect();
    if topic.len() != 3 {
        Err(Error::GeneralError(format!("Topic did not have the expected structure of <prefix>:<worker-id>:<identifier> but was: {}", topic.join(":"))))
    } else {
        Ok(Topic {
            event: topic.pop().unwrap(),
            worker: topic.pop().unwrap(),
            type_: topic.pop().unwrap(),
        })
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
fn construct_parameters(message: &serde_json::Value) -> Result<Vec<Parameter>> {
    // Values should be an array of values
    let values = match message["content"]["values"].as_array() {
        Some(x) => x,
        None => {
            log_error_and_panic("content.values of callback response is not an array");
        }
    };
    let mut parameters = Vec::with_capacity(values.len());

    for parameter in values {
        if parameter[0].is_null() || parameter[1][0].is_null() || parameter[1][1].is_null() {
            log::error!("one of the values within the callback response content.values is null",);
            // TODO: Determine whether we want to return here or skip parameter
            continue;
        }
        let param_type = parameter[1][0].as_str().unwrap();
        if param_type == "simple" {
            let header_name = parameter[0].as_str().unwrap().to_owned();
            let header_value = parameter[1][1].as_str().unwrap().to_owned();
            parameters.push(Parameter::SimpleParameter {
                name: header_name,
                value: header_value,
                param_type: http_helper::ParameterType::Body,
            });
        } else if param_type == "complex" {
            let name = parameter[0].as_str().unwrap().to_owned();
            let mime_type = match parameter[1][1].as_str().unwrap().parse::<mime::Mime>() {
                Ok(x) => x,
                Err(err) => {
                    // TODO: Determine whether we want to return here or skip parameter
                    return Err(err.into());
                }
            };
            let content_path = parameter[1][2].as_str().unwrap();
            parameters.push(Parameter::ComplexParameter {
                name,
                mime_type,
                content_handle: std::fs::File::open(content_path)
                    .expect("Could not open file for complex param in callback thread"),
            });
        } else {
            log::error!(
                "Could not construct parameter out of callback response as the type was: {:?}",
                parameter[1][0]
            );
            // TODO: Determine whether we want to return here or skip parameter
            continue;
        }
    }
    Ok(parameters)
}

#[cfg(test)]
mod test {
    use super::*;

    /**
     * Setup: Expects a redis instance running with a UNIX socket to be located at /run/redis.sock
     * Ensures that the UNIX socket connection works
     */
    #[test]
    fn test_connection_socket() {
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

    /**
     * Tests whether the publish and subscribe cycle with redis works.
     * Creates a separate thread that publishes test messages.
     * Main thread subscribes to the messages and checks whether the correct messages are received
     */
    #[test]
    fn test_blocking_pub_sub() {
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
            let mut iter = 0;
            loop {
                // let topic_id = topic_ids.choose(&mut thread_rng()).unwrap()
                match connection.publish::<String, String, i32>(
                    "event:01:test_state/changed".to_owned(),
                    format!("{} {}", instance_id, "test_payload"),
                ) {
                    Ok(_) => {}
                    Err(err) => log::error!("Error publishing: {err}"),
                }
                sleep(Duration::from_secs(1));
                iter = iter + 1;
                if iter > 3 {
                    let _ = connection.publish::<String, String, i32>(
                        "event:01:test_state/changed".to_owned(),
                        format!("{} {}", instance_id, "stop"),
                    );
                    break;
                }
            }
        });

        let mut redis = RedisHelper::new(&get_unix_socket_configuration(), "pub_sub_test").unwrap();
        redis
            .blocking_pub_sub(
                vec!["event:*".to_owned()],
                |payload: &str, pattern: &str, topic: Topic| {
                    println!(
                        "Pattern: {}\nTopic: {:?}\nPayload:{}",
                        pattern, topic, payload
                    );
                    assert_eq!("event", topic.type_);
                    assert_eq!("01", topic.worker);
                    assert_eq!("test_state/changed", topic.event);
                    if payload == "stop" {
                        Ok(false)
                    } else {
                        Ok(true)
                    }
                },
            )
            .unwrap();
    }

    #[test]
    fn test_send() -> Result<()> {
        let rec_thread = thread::spawn(|| -> Result<()> {
            let mut redis_receiver =
                RedisHelper::new(&get_unix_socket_configuration(), "test_connection").unwrap();
            redis_receiver.blocking_pub_sub(
                vec!["event:*".to_owned()],
                |payload: &str, pattern: &str, topic: Topic| {
                    let expected_payload = json!({
                        "cpee": "localhost/cpee",
                        "instance-url": format!("{}/{}", "localhost/cpee", "test_id"),
                        "instance": "test_id",
                        "topic": "test_state",
                        "type": "event",
                        "name": "changed",
                        "content": "test_payload",
                        "instance-uuid": "test_uuid",
                        "instance-name": "test_info"
                    })
                    .to_string();
                    println!("Payload:\n{payload}");

                    // Remove timestamp as it will not be consistent
                    let mut modified_payload = Vec::new();
                    let mut item_index = 0;
                    for item in payload.split(",") {
                        item_index = item_index + 1;
                        if item_index == 8 {
                            continue;
                        }
                        modified_payload.push(item.to_owned());
                    }
                    let payload = modified_payload.join(",");
                    println!("Modified payload: {payload}");

                    assert_eq!(expected_payload, payload);
                    assert_eq!("event:*", pattern);
                    assert_eq!("event", topic.type_);
                    assert_eq!(0, topic.worker.parse::<u32>().unwrap());
                    assert_eq!("test_state/changed", topic.event);
                    return Ok(false);
                },
            )?;
            Ok(())
        });

        let conf = get_unix_socket_configuration();
        let mut redis_sender = RedisHelper::new(&conf, "test_connection").unwrap();

        redis_sender.send(
            "event",
            "test_state/changed",
            InstanceMetaData {
                instance_url: conf.instance_url(),
                instance_id: conf.instance_id,
                cpee_base_url: conf.cpee_base_url,
                instance_uuid: "test".to_owned(),
                info: "test".to_owned(),
                attributes: HashMap::new(),
            },
            Some(serde_json::Value::String("test_payload".to_owned())),
        )?;
        let result = rec_thread.join();
        match result {
            Ok(inner) => match inner {
                Ok(()) => println!("All fine"),
                Err(_) => panic!("Error within blocking_pub_sub"),
            },
            Err(_) => panic!("Thread paniced"),
        }
        Ok(())
    }

    fn get_unix_socket_configuration() -> Opts {
        let home = std::env::var("HOME").unwrap();
        let expanded_path = format!("{}/redis/redis.sock", home);
        let mut attributes = HashMap::new();
        attributes.insert("uuid".to_owned(), "test_uuid".to_owned());
        attributes.insert("info".to_owned(), "test_info".to_owned());

        Opts {
            instance_id: 12,
            host: "localhost".to_owned(),
            cpee_base_url: "localhost/cpee".to_owned(),
            redis_url: None,
            redis_path: Some(format!("unix://{}", expanded_path)),
            redis_db: 0,
            redis_workers: 2,
            executionhandlers: "".to_owned(),
            executionhandler: "".to_owned(),
            eval_language: "".to_owned(),
            eval_backend_exec_full: "".to_owned(),
            eval_backend_structurize: "".to_owned(),
            attributes,
            global_executionhandlers: "".to_owned(),
        }
    }

    fn get_tcp_configuration() -> Opts {
        Opts {
            instance_id: 12,
            host: "localhost".to_owned(),
            cpee_base_url: "localhost/cpee".to_owned(),
            // Default port
            redis_url: Some("redis://localhost:6379".to_owned()),
            redis_path: None,
            redis_db: 0,
            redis_workers: 2,
            executionhandlers: "".to_owned(),
            executionhandler: "".to_owned(),
            eval_language: "".to_owned(),
            eval_backend_exec_full: "".to_owned(),
            eval_backend_structurize: "".to_owned(),
            attributes: HashMap::new(),
            global_executionhandlers: "".to_owned(),
        }
    }
    
    #[test]
    fn parsing() {
        let mime = "application/json".parse::<Mime>().unwrap();
        println!("{}", mime)
    }
}
