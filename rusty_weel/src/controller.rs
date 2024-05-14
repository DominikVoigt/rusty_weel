use std::{
    borrow::BorrowMut,
    collections::HashMap,
    fmt::format,
    sync::{Arc, Mutex},
    thread::{self, sleep, JoinHandle},
    time::Duration,
};

use http_helper::HTTPParameters;
use redis::{Connection, RedisError};
use serde_json::json;

use crate::{connection_wrapper::ConnectionWrapper, dslrealization::Weel, model::Configuration};

const TOPICS: &[&str] = &["callback-response:*", "callback-end:*"];
const CALLBACK_RESPONSE_ERROR_MESSAGE: &str = "Callback-response had not the correct format, could not find whitespace separator";

pub struct Controller {
    pub instance: Weel,
    configuration: Configuration,
    redis_connection: redis::Connection,
    votes: Vec<String>, // Not sure yet
    callback_keys: HashMap<String, Arc<Mutex<ConnectionWrapper>>>,
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
    pub fn new(instance_id: &str, configuration: Configuration) -> Arc<Mutex<Controller>> {
        let controller = Controller {
            instance: Weel {},
            redis_connection: connect_to_redis(&configuration)
                .expect("Could not establish initial redis connection"),
            configuration: configuration,
            votes: Vec::new(),
            callback_keys: HashMap::new(),
            id: String::new(),
            attributes: HashMap::new(),
            instance_execution_thread: Option::None,
            redis_subscription_thread: Option::None,
            loop_guard: HashMap::new(),
        };

        let controller = Arc::new(Mutex::new(controller));
        // Here the mutex is created so we can lock and unwrap directly
        loop {
            match controller
                .lock()
                .expect("Mutex is locked directly after creation -> ERROR")
                .try_establish_subscriptions(Arc::clone(&controller))
            {
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
     * // TODO: Handle issue of redis not connecting
     */
    fn try_establish_subscriptions(
        &mut self,
        controller: Arc<Mutex<Controller>>,
    ) -> Result<(), RedisError> {
        // Create redis connection for subscriptions and their handling
        let mut redis_connection = match connect_to_redis(&self.configuration) {
            Ok(redis_connection) => redis_connection,
            Err(err) => return Err(err),
        };

        self.redis_subscription_thread = Some(thread::spawn(move || {
            // Move exclusive connection and controller reference into this thread
            let mut redis_subscription = redis_connection.as_pubsub();
            // will pushback message to self.waiting_messages of the PubSub instance
            match redis_subscription.psubscribe(TOPICS) {
                Ok(_) => {}
                Err(_) => return,
            }

            loop {
                let message: redis::Msg = redis_subscription.get_message().expect("");
                let mut payload: String = message
                    .get_payload()
                    .expect("Failed to get payload from message");
                let pattern: String = message.get_pattern().expect("Could not get pattern");
                match pattern.as_str() {
                    "callback-response:*" => {
                        let topic: Topic = split_topic(message.get_channel_name());
                        let (_instance_id, message) = payload.split_once(" ").expect(CALLBACK_RESPONSE_ERROR_MESSAGE);
                        construct_parameters(message);

                    }
                    "callback-end:*" => {
                        let topic: Topic = split_topic(message.get_channel_name());
                        controller.lock()
                            .expect("Could not acquire mutex as it is poisoned, when deleting callback key")
                            .callback_keys.remove(&topic.identifier);
                    }
                    x => {
                        println!("Received on channel {} the payload: {}", x, payload)
                    }
                }
            }
        }));
        Ok(())
    }

    fn notify(&self) {}
}

fn construct_parameters(message: &str) -> Vec<HTTPParameters> {
    // Parse message into json
    let message = json!(message);

    let mut http_parameters: Vec<http_helper::HTTPParameters> = Vec::new();
    // Values should be an array of values
    let values = match message["content"]["values"].as_array() {
        Some(x) => x,
        None => {
            log::error!("content.values of callback response is not an array");
            panic!("{}", CALLBACK_RESPONSE_ERROR_MESSAGE);
        }
    };
    values.iter().filter_map(|value| {
        if value[0].is_null() || value[1][0].is_null() || value[1][1].is_null() {
            log::error!("one of the values within the callback response content.values is null");
            panic!("{}", CALLBACK_RESPONSE_ERROR_MESSAGE)
        }
        if value[1][0] == "simple" {
            let header_name = match value[0].as_str() {
                Some(header_value) => header_value.to_owned(),
                None => handle_error("Could not create simple parameter as the header name field is not a string")
            };
            let header_value = match value[1][1].as_str() {
                Some(header_value) => header_value.to_owned(),
                None => handle_error("Could not create simple parameter as the header value field is not a string")
            };
            Some(HTTPParameters::SimpleParameter { name: header_name, value: header_value, param_type: http_helper::ParameterType::Body })
        } else if value[1][0] == "complex" {
            Some(HTTPParameters::ComplexParamter { a: (), mime_type: (), file_path: () })
        } else {
            log::warn!("Could not construct paramter out of callback response as the type was: {:?}", value[1][0])
            None
        }
    }
    )
    .collect()
}

/**
 * This function is a helper function that is called if an unrecoverable error is happening.
 * This function will end in the program panicing but also includes some prior logging
 */
fn handle_error(log_msg: &str) -> ! {
    log::error!("{}", log_msg);
    panic!("{}", log_msg);
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
