use std::fs;
use std::{collections::HashMap, path::PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
pub struct HTTPParams {
    pub label: &'static str,
    pub method: reqwest::Method,
    pub arguments: Option<Vec<KeyValuePair>>,
}

impl TryInto<String> for HTTPParams {
    type Error = serde_json::Error;

    fn try_into(self) -> Result<String, Self::Error> {
        let mut hash_rep = HashMap::new();
        hash_rep.insert("label", self.label.to_owned());
        hash_rep.insert("method", self.method.to_string());
        let args = match self.arguments {
            Some(args) => {
                let args_map: HashMap<_, _>;
                args_map = args.iter().map(|e| {
                    let value = match e.value.clone() {
                        Some(value) => value,
                        None => {"->{ nil }"}.to_owned(),
                    };
                    (e.key, value)
                }).collect();
                serde_json::to_string(&args_map)?
            },
            None => "nil".to_owned(),
        };
        hash_rep.insert("arguments", args);
        
        serde_json::to_string(&hash_rep)
    }
}

/*
* Represents KVs with optional values
*/
#[derive(Debug, Clone)]
pub struct KeyValuePair {
    pub key: &'static str,
    pub value: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum State {
    Ready,
    Starting,
    Running,
    Stopping,
    Stopped,
    Finishing,
}

/**
 * Contains all the meta data that is never changing during execution
 */
#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
pub struct StaticData {
    pub instance_id: String,
    pub host: String,
    pub base_url: String,
    pub redis_url: Option<String>,
    pub redis_path: Option<String>,
    pub redis_db: i64,
    pub redis_workers: u32,
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
    pub fn load(path: &str) -> Self {
        let config = fs::read_to_string(path).expect("Could not read configuration file!");
        let config: Self =
            serde_yaml::from_str(&config).expect("Could not parse Configuration");
        config
    }

    pub fn uuid(&self) -> &str {
        self.attributes
            .get("uuid")
            .expect("Attributes do not contain uuid")
    }

    pub fn info(&self) -> &str {
        self.attributes
            .get("info")
            .expect("Attributes do not contain info")
    }

    pub fn host(&self) -> &str {
        self.host.as_str()
    }

    pub fn base_url(&self) -> &str {
        self.base_url.as_str()
    }

    pub fn instance_url(&self) -> String {
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

mod testing {
    use std::{collections::HashMap, fs};

    use super::StaticData;

    fn create_dummy_static(path: &str) -> StaticData {
        //let config = fs::read_to_string(path).expect("Could not read configuration file!");
        let mut attr = HashMap::new();
        attr.insert("uuid".to_owned(), "test-uuid".to_owned());
        attr.insert("modeltype".to_owned(), "CPEE".to_owned());
        let config: StaticData = StaticData {
            instance_id: "test_id".to_owned(),
            host: "test_id".to_owned(),
            base_url: "test_id".to_owned(),
            redis_url: None,
            redis_path: Some("test_id".to_owned()),
            redis_db: 0,
            redis_workers: 2,
            global_executionhandlers: "exhs".to_owned(),
            executionhandlers: "exh".to_owned(),
            executionhandler: "rust".to_owned(),
            eval_language: "ruby".to_owned(),
            eval_backend_url: "ruby_backend_url".to_owned(),
            attributes: attr,
        };
        let file = fs::File::create_new(path).unwrap();
        serde_yaml::to_writer(file, &config).unwrap();
        config
    }

    #[test]
    fn test_loading_static() {
        let path = "./test_files/static.data";
        let _ = fs::remove_file(path);
        let dummy = create_dummy_static(path);
        
        assert_eq!(dummy, StaticData::load(path));

    }
}