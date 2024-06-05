use std::{
    collections::HashMap, path::PathBuf, sync::{Arc, Mutex}, thread::{self, JoinHandle}
};

use crate::{
    connection_wrapper::ConnectionWrapper,
    data_types::{Static, Dynamic, InstanceMetaData, KeyValuePair, State},
    redis_helper::RedisHelper,
    dslrealization::Weel,
};


/**
 * Controller is central to the instance execution
 * It interfaces directly with Redis and thus manages ALL communication of the Weel Instance with the CPEE:
 *  - Status Updates
 *  - Callbacks
 *
 * The controller also takes any interrrupts and provides the correct signals to the weel instance (via state change) to halt execution when requested.
 */
pub struct Controller {
    configuration: Static,
    context: Dynamic,
    // We need to guard redis helper if we keep one helper (aka one connection per helper/controller) (voting and notify can occur in parallel)
    redis_helper: Mutex<RedisHelper>,
    votes: Mutex<Vec<u128>>, // Not sure yet
    callback_keys: Arc<Mutex<HashMap<String, Arc<Mutex<ConnectionWrapper>>>>>,
    id: String,
    // TODO: Maybe we do not need to hold handle -> Detach thread
    redis_subscription_thread: Option<JoinHandle<()>>,
    instance_execution_thread: Mutex<Option<JoinHandle<()>>>,
    loop_guard: HashMap<String, String>,
    // Used to communicate with executing weel -> If set to stopping, weel will stop execution by skipping the activities
    state: Arc<Mutex<State>>,
    weel_instance: Option<Weel>
}