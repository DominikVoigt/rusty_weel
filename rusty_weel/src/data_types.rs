use std::collections::HashMap;
use std::fs;

use serde::{Serialize, Deserialize};

//TODO: Think whether we can merge this with the http helper
#[derive(Debug)]
pub struct HTTPRequest {
    pub label: &'static str,
    pub method: HTTP,
    pub arguments: Option<Vec<KeyValuePair>>,
}

#[derive(Debug)]
pub enum HTTP {
    GET,
    PUT,
    POST,
    DELETE,
    PATCH,
}

/*
* Represents KVs with optional values
*/
#[derive(Debug)]
pub struct KeyValuePair {
    pub key: &'static str,
    pub value: Option<String>,
}

pub enum State {
    Running,
    Stopping,
    Stopped,
}

/**
 * Contains all the meta data that is never changing during execution
 */
#[derive(Serialize, Deserialize, Debug)]
pub struct Configuration {
    pub host: String,
    pub base_url: String,
    pub redis_url: String,
    pub redis_path: String,
    pub redis_db: u32,
    pub global_executionhandlers: String,
    pub executionhandlers: String,
    pub executionhandler: String,
    pub eval_language: String,
    pub eval_backend_url: String,
    pub attributes: HashMap<String, String>,
}

/**
 * Contains meta data that might be changing during execution
 */
#[derive(Serialize, Deserialize, Debug)]
pub struct Context {
    pub endpoints: HashMap<String, String>,
    pub data: String
}

/**
 * DTO that contains all the general information about the instance
 */
#[derive(Serialize, Deserialize)]
pub struct InstanceMetaData {
    pub cpee_base_url: String,
    pub instance_id: String,
    pub instance_url: String,
    pub instance_uuid: String,
    pub info: String,
}

/**
 * Contains all the meta data about the task
 */
pub struct TaskMetaData {
    task_label: String,
    task_id: String
}

impl Configuration {
    
    pub fn load_configuration(path: &str) -> Configuration {
        let config = fs::read_to_string(path).expect("Could not read configuration file!");
        let config: Configuration = serde_yaml::from_str(&config).expect("Could not parse Configuration");
        config
    } 
}

impl Context {
    
    pub fn load_context(path: &str) -> Context {
        let context = fs::read_to_string(path).expect("Could not read context file!");
        let context: Context = serde_yaml::from_str(&context).expect("Could not parse Configuration");
        context
    } 
}

type UndefinedTypeTODO = ();
// Define a float type to easily apply changes here if needed
#[allow(non_camel_case_types)]
type float = f32;
