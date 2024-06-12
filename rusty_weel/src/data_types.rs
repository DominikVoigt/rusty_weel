use std::fs;
use std::{collections::HashMap, path::PathBuf};

use serde::{Deserialize, Serialize};

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
    Starting,
    Running,
    Stopping,
    Stopped,
}

/**
 * Contains all the meta data that is never changing during execution
 */
#[derive(Serialize, Deserialize, Debug)]
pub struct StaticData {
    pub instance_id: String,
    pub host: String,
    pub base_url: String,
    pub redis_url: Option<String>,
    pub redis_path: Option<String>,
    pub redis_db: i64,
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
pub struct DynamicData {
    pub endpoints: HashMap<String, String>,
    pub data: String,
}

/**
 * DTO that contains all the general information about the instance
 * Helper, can be directly derived from configuration
 */
#[derive(Serialize, Deserialize)]
pub struct InstanceMetaData {
    pub cpee_base_url: String,
    pub instance_id: String,
    pub instance_url: String,
    pub instance_uuid: String,
    pub info: String,
    pub attributes: HashMap<String, String>,
}

/**
 * Contains all the meta data about the task
 */
pub struct TaskMetaData {
    task_label: String,
    task_id: String,
}

impl StaticData {
    pub fn load(path: &str) -> StaticData {
        let config = fs::read_to_string(path).expect("Could not read configuration file!");
        let config: StaticData =
            serde_yaml::from_str(&config).expect("Could not parse Configuration");
        config
    }

    fn uuid(&self) -> &str {
        self.attributes
            .get("uuid")
            .expect("Attributes do not contain uuid")
    }

    fn info(&self) -> &str {
        self.attributes
            .get("info")
            .expect("Attributes do not contain info")
    }

    fn host(&self) -> &str {
        self.host.as_str()
    }

    fn base_url(&self) -> &str {
        self.base_url.as_str()
    }

    fn instance_url(&self) -> String {
        let mut path = PathBuf::from(self.base_url.as_str());
        path.push(self.instance_id.clone());
        path.to_str()
            .expect("Path to instance is not valid UTF-8")
            .to_owned()
    }

    pub fn get_instance_meta_data(&self) -> InstanceMetaData {
        InstanceMetaData {
            cpee_base_url: self.base_url().to_owned(),
            instance_id: self.instance_id.clone(),
            instance_url: self.instance_url(),
            instance_uuid: self.uuid().to_owned(),
            info: self.info().to_owned(),
            attributes: self.attributes.clone(),
        }
    }
}

impl DynamicData {
    pub fn load(path: &str) -> DynamicData {
        let context = fs::read_to_string(path).expect("Could not read context file!");
        let context: DynamicData =
            serde_yaml::from_str(&context).expect("Could not parse Configuration");
        context
    }
}

type UndefinedTypeTODO = ();
// Define a float type to easily apply changes here if needed
#[allow(non_camel_case_types)]
type float = f32;
