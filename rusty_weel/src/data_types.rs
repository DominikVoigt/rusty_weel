use std::collections::VecDeque;
use std::fs;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::ThreadId;
use std::{collections::HashMap, path::PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::dsl_realization::{Position, Signal};

#[derive(Debug, Clone, Serialize)]
pub struct HTTPParams {
    pub label: &'static str,
    
    pub method: http_helper::Method,
    pub arguments: Option<Vec<KeyValuePair>>,
}

impl TryInto<String> for HTTPParams {
    type Error = serde_json::Error;

    fn try_into(self) -> Result<String, Self::Error> {
        let mut hash_rep = HashMap::new();
        hash_rep.insert("label", self.label.to_owned());
        hash_rep.insert("method", format!("{:?}", self.method));
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
#[derive(Debug, Clone, Serialize)]
pub struct KeyValuePair {
    pub key: &'static str,
    pub value: Option<String>,
    pub expression_value: bool,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Clone, Copy)]
pub enum State {
    Ready,
    Starting,
    Running,
    Stopping,
    Stopped,
    Finishing,
    Finished,
}

/**
 * Contains all the meta data that is never changing during execution
 */
#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
pub struct StaticData {
    pub instance_id: u32,
    pub host: String,
    pub cpee_base_url: String,
    pub redis_url: Option<String>,
    pub redis_path: Option<String>,
    pub redis_db: i64,
    pub redis_workers: u32,
    pub executionhandlers: String,
    pub executionhandler: String,
    pub eval_language: String,
    pub eval_backend_exec_full: String,
    pub eval_backend_structurize: String,
}

/**
 * Contains meta data that might be changing during execution
 */
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct DynamicData {
    pub endpoints: HashMap<String, String>,
    pub data: Value,
}

#[derive(Debug, Default)]
pub struct Status {
    pub id: u32,
    pub message: String,
    pub nudge: BlockingQueue<Status>,
}

#[derive(Serialize, Deserialize, Debug, Default)]
pub struct StatusDTO {
    pub id: u32,
    pub message: String,
}

impl<'a> Status {
    pub fn to_dto(&self) -> StatusDTO {
        StatusDTO { id: self.id, message: self.message.clone() }
    }
}

impl Status {
    pub fn new(id: u32, message: String) -> Status {
        Self {
            id,
            message,
            nudge: BlockingQueue::new(),
        }
    }
}

/**
 * DTO that contains all the general information about the instance
 * Helper, can be directly derived from configuration
 */
#[derive(Serialize, Deserialize)]
pub struct InstanceMetaData {
    pub cpee_base_url: String,
    pub instance_id: u32,
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

    pub fn host(&self) -> &str {
        self.host.as_str()
    }

    pub fn base_url(&self) -> &str {
        self.cpee_base_url.as_str()
    }

    pub fn instance_url(&self) -> String {
        let mut path = PathBuf::from(self.cpee_base_url.as_str());
        path.push(self.instance_id.to_string());
        path.to_str()
            .expect("Path to instance is not valid UTF-8")
            .to_owned()
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

pub struct ThreadInfo {
    pub parent: Option<ThreadId>,
    pub in_search_mode: bool,
    pub switched_to_execution: bool,
    pub no_longer_necessary: bool,
    pub blocking_queue: Arc<BlockingQueue<Signal>>,

    // ID of this thread relative to its parent (not globaly unique), used mainly for debugging), this id is used within the branch_traces
    pub branch_id: i32,
    pub branch_traces: HashMap<i32, Vec<String>>,
    pub branch_position: Option<Position>,
    pub branch_wait_count_cancel_condition: CancelCondition,
    pub branch_wait_count_cancel_active: bool,
    // Counts the number of already canceled branches
    pub branch_wait_count_cancel: i32,
    pub branch_wait_count: i32,
    // Used for synchronization of child branches
    pub branch_event: Option<BlockingQueue<()>>,
    pub local: String,
    // Thread IDs of all spawned children threads (are branches)
    pub branches: Vec<ThreadId>
}

#[derive(PartialEq, Eq)]
pub enum CancelCondition {
    First,
    Last
}

/**
 * Simple multi-threading synchronization structure
 * Queue blocks on dequeue if it is empty.
 * Unblocks threads if elements are enqueued
 */
#[derive(Debug, Default)]
pub struct BlockingQueue<T> {
    queue: Mutex<VecDeque<T>>,
    signal: Condvar,
}

impl<T> BlockingQueue<T> {
    pub fn new() -> Self {
        BlockingQueue { queue: Mutex::new(VecDeque::new()), signal: Condvar::new() }
    }
    
    pub fn enqueue(&self, element: T) {
        self.queue.lock().unwrap().push_back(element);
        self.signal.notify_one();
    }

    pub fn dequeue(&self) -> T {
        let mut queue = self.queue.lock().unwrap();
        // Even though can wake up spuriously, not a problem if we check the condition repeatedly on whether the queue is non-empty
        while queue.is_empty() {
            log::info!("Queue empty, have to wait...");
            queue = self.signal.wait(queue).unwrap();
        }
        // Only leave queue if it contains an item -> can pop it off
        queue.pop_front().unwrap()
    }

    pub fn clear(&self) {
        self.queue.lock().unwrap().clear();
    }

    pub fn need_to_wait(&self) -> bool {
        self.queue.lock().unwrap().is_empty()
    }
}

type UndefinedTypeTODO = ();
// Define a float type to easily apply changes here if needed
#[allow(non_camel_case_types)]
type float = f32;

#[cfg(test)]
mod testing {
    use std::fs;

    use super::StaticData;

    fn create_dummy_static(path: &str) -> StaticData {
        //let config = fs::read_to_string(path).expect("Could not read configuration file!");
        let config: StaticData = StaticData {
            instance_id: 12,
            host: "test_id".to_owned(),
            cpee_base_url: "test_id".to_owned(),
            redis_url: None,
            redis_path: Some("test_id".to_owned()),
            redis_db: 0,
            redis_workers: 2,
            executionhandlers: "exh".to_owned(),
            executionhandler: "rust".to_owned(),
            eval_language: "ruby".to_owned(),
            eval_backend_exec_full: "ruby_backend_url".to_owned(),
            eval_backend_structurize: "ruby_backend_url".to_owned(),
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