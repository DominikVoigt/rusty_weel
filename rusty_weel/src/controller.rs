use std::{
    collections::HashMap, path::PathBuf, sync::{Arc, Mutex}, thread::{self, sleep, JoinHandle}, time::Duration
};

use http_helper::{eval::evaluate, HTTPParameters};
use http_helper::eval;
use redis::{Commands, RedisError};
use rusty_weel_macro::get_str_from_value;
use crate::data_types::KeyValuePair;
use serde_json::json;

use crate::{
    connection_wrapper::ConnectionWrapper, data_types::{Configuration, Context}
};

const TOPICS: &[&str] = &["callback-response:*", "callback-end:*"];
const CALLBACK_RESPONSE_ERROR_MESSAGE: &str =
    "Callback-response had not the correct format, could not find whitespace separator";

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
    redis_connection: redis::Connection,
    votes: Vec<String>, // Not sure yet
    callback_keys: Arc<Mutex<HashMap<String, Arc<Mutex<ConnectionWrapper>>>>>,
    id: String,
    redis_subscription_thread: Option<JoinHandle<()>>,
    loop_guard: HashMap<String, String>,
}

impl Controller {
    /**
     * Creates a new controller instance
     * Instance is returned as an Arc<Mutex> as it is shared between the calling thread
     * and the thread that is started within new to handle messages the controller subscribes to
     */
    pub fn new(instance_id: &str, configuration: Configuration, context: Context) -> Controller {
        let mut controller = Controller {
            redis_connection: connect_to_redis(&configuration)
                .expect("Could not establish initial redis connection"),
            configuration,
            context,
            votes: Vec::new(),
            callback_keys: Arc::new(Mutex::new(HashMap::new())),
            id: instance_id.to_owned(),
            redis_subscription_thread: Option::None,
            loop_guard: HashMap::new(),
        };

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

    /**
     * //TODO: What is what
     */
    fn notify(&mut self, what: &str, content: Option<HashMap<String, String>>) {
        let mut content: HashMap<String, String> = content.unwrap_or_else(|| -> HashMap<String, String> {
            HashMap::new()
        });
        content.insert("attributes".to_owned(), self.translate_attributes());
        self.send("event", what, content);
    }

    fn uuid(&self) -> &str {
        self.context.attributes.get("uuid").expect("Attributes do not contain uuid")
    }

    fn info(& self) -> &str {
        self.context.attributes.get("info").expect("Attributes do not contain info")
    }

    fn translate_attributes(&mut self) -> String {
        let mut statements = HashMap::new(); 
        self.context.attributes.iter().for_each(|(k, v)| {
            if v.starts_with("!") {
                statements.insert(k.to_owned(), v[1..].to_owned());
            }
        }); 
        // Evaluate expressions
        let evaluations = evaluate(self.configuration.eval_backend_url.as_str(), self.context.data.clone(), statements);
        
        // Replace expressions with values in attributes
        evaluations.iter().for_each(|(k,v)| {
            let k = k.to_owned();
            let v = v.to_owned();
            self.context.attributes.insert(k, v);
        });
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


    fn send(&mut self, message_type: &str, event: &str, content: HashMap<String, String>) -> () {
        let cpee_url = self.base_url();
        let instance_id = self.id.as_str();
        let instance_uuid = self.uuid();
        let info = self.info(); 

        let target = "";
        let (topic, name) = event.split_at(event.rfind("/").expect("event does not have correct structure: Misses / delimiter"));
        let content = serde_json::to_string(&content).expect("Could not serialize content to json string");
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
        let payload: String = format!("{} {}", instance_id, serde_json::to_string(&payload).expect("Could not deserialize payload"));
        let publish_result: Result<(), redis::RedisError> = self.redis_connection.publish(channel, payload);
        if publish_result.is_err() {
            log_error_and_panic(format!("Error occured when publishing message to redis during send: {:?}", publish_result.expect_err("Not possible to reach")).as_str())
        }
    }

        /**
     * Creates a new Key value pair by evaluating the key and value expressions (tries to resolve them in rust if they are simple data accessors)
     */
    pub fn new_key_value_pair(key_expression: &'static str, value: &'static str) -> KeyValuePair {
        let key = key_expression;
        let value = Some(value.to_owned());
        KeyValuePair {
            key,
            value,
        }
    }

    pub fn new_key_value_pair_ex(&self, key_expression: &'static str, value_expression: &'static str) -> KeyValuePair {
        let key = key_expression;
        let mut statement = HashMap::new();
        statement.insert("k".to_owned(), value_expression.to_owned());
        // TODO: Should we lock context here as mutex or just pass copy?
        let value = eval::evaluate(self.configuration.eval_backend_url.as_str(), self.context.data.clone(), statement).remove("k").expect("Response is empty");
        let value = Option::Some(value);
        KeyValuePair {
            key,
            value,
        }
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

