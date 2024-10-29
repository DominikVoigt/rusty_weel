use std::collections::VecDeque;
use std::fs;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::thread::{JoinHandle, ThreadId};
use std::{collections::HashMap, path::PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::dsl_realization::{Position, Result, Signal};

#[derive(Debug, Clone, Serialize)]
pub struct HTTPParams {
    pub label: &'static str,
    pub method: http_helper::Method,
    pub arguments: Option<Vec<KeyValuePair>>,
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
    None,
}

impl Default for State {
    fn default() -> Self {
        Self::None
    }
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
    pub nudge: BlockingQueue<()>,
}

#[derive(Serialize, Deserialize, Debug, Default)]
pub struct StatusDTO {
    pub id: u32,
    pub message: String,
}

impl<'a> Status {
    pub fn to_dto(&self) -> StatusDTO {
        StatusDTO {
            id: self.id,
            message: self.message.clone(),
        }
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
        let config: Self = serde_yaml::from_str(&config).expect("Could not parse Configuration");
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

// TODO: Certain parts here can be moved into true thread local storage to speed up the application
#[derive(Debug)]
pub struct ThreadInfo {
    pub parent_thread: Option<ThreadId>,
    pub branches: Vec<ThreadId>,
    pub branch_traces: HashMap<ThreadId, Vec<String>>,

    // For parallel and parallel branch to communicate
    pub parallel_wait_condition: CancelCondition,
    // Number of threads that need to fulfill the parallel wait condition
    pub branch_wait_threshold: usize,
    // Counts the number of executed branches w.r.t the parallel wait condition
    pub branch_wait_count: usize,
    // Counts the number of branches that reached the end of execution (either skipped or executed) 
    pub branch_finished_count: usize,
    // Sends a signal if the branch terminated
    pub terminated_signal_sender: Option<Sender<()>>,
    // Used to check whether the branch executed the lambda already
    pub terminated_signal_receiver: Option<Receiver<()>>,
    // Used for synon the start of child branches
    pub branch_barrier_start: Option<Arc<BlockingQueue<()>>>,
    // Used by child branches to signal (via sender) to the gateway (via receiver) that the wait condition is fulfilled -> unblock gateway thread
    pub branch_event_sender: Option<mpsc::Sender<()>>,
    // Join handle for the thread, this should only ever be called from parent, and is set by parent after the thread is initialized -> main thread has no join handle -> None
    pub join_handle: Option<JoinHandle<Result<()>>>,
    // Snapshot of the data at the time of creation of the branch
    pub local: Option<Value>,

    // For choose -> Might be truly thread local
    pub alternative_executed: Vec<bool>,
    pub alternative_mode: Vec<ChooseVariant>,

    pub no_longer_necessary: bool,

    pub in_search_mode: bool,
    pub branch_position: Option<Arc<Position>>,
    pub switched_to_execution: bool,
    // Continue structure in original code
    pub callback_signals: Arc<Mutex<BlockingQueue<Signal>>>,

    // Thread IDs of all spawned children threads (are branches)
    // ID of this thread relative to its parent (not globaly unique), used mainly for debugging), this id is used within the branch_traces
    pub first_activity_in_thread: bool,

}

impl Default for ThreadInfo {
    fn default() -> Self {
        Self {
            parent_thread: None,
            in_search_mode: false,
            switched_to_execution: false,
            no_longer_necessary: false,
            callback_signals: Arc::new(Mutex::new(BlockingQueue::new())),
            branch_traces: HashMap::new(),
            branch_position: None,
            // This should not matter since we are not in a parallel yet
            parallel_wait_condition: CancelCondition::First,
            first_activity_in_thread: true,
            branch_wait_threshold: 0,
            branch_wait_count: 0,
            branch_barrier_start: None,
            branch_event_sender: None,
            branches: Vec::new(),
            join_handle: None,
            // To here
            local: None,
            alternative_mode: Vec::new(),
            alternative_executed: Vec::new(),
            branch_finished_count: 0,
            terminated_signal_sender: None,
            terminated_signal_receiver: None,
        }
    }
}

#[derive(PartialEq, Eq, Debug)]
pub enum CancelCondition {
    First,
    Last,
}

#[derive(Clone, Copy, Debug)]
pub enum ChooseVariant {
    Inclusive,
    Exclusive,
}

/**
 * Simple multi-threading synchronization structure
 * Queue blocks on dequeue if it is empty.
 * Unblocks threads if elements are enqueued
 */
#[derive(Debug, Default)]
pub struct BlockingQueue<T> {
    queue: Mutex<VecDeque<T>>,
    pub num_waiting: Mutex<u32>,
    signal: Condvar,
}

impl<T: Default> BlockingQueue<T> {
    pub fn new() -> Self {
        BlockingQueue {
            queue: Mutex::new(VecDeque::new()),
            num_waiting: Mutex::new(0),
            signal: Condvar::new(),
        }
    }

    pub fn enqueue(&self, element: T) {
        self.queue.lock().unwrap().push_back(element);
        self.signal.notify_one();
    }

    pub fn dequeue(&self) -> T {
        let mut queue = self.queue.lock().unwrap();
        // Even though can wake up spuriously, not a problem if we check the condition repeatedly on whether the queue is non-empty
        while queue.is_empty() {
            *self.num_waiting.lock().unwrap() += 1;
            queue = self.signal.wait(queue).unwrap();
        }
        let mut waiting = self.num_waiting.lock().unwrap();
        if *waiting > 0 {
            *waiting -= 1;
        }
        // Only leave queue if it contains an item -> can pop it off
        queue.pop_front().unwrap()
    }

    pub fn wake_all(&self) {
        let mut queue = self.queue.lock().unwrap();
        let waiting = self.num_waiting.lock().unwrap();
        for _i in 0..*waiting {
            queue.push_back(T::default());
        }
        drop(queue);
        self.signal.notify_all();
    }

    pub fn clear(&self) {
        self.queue.lock().unwrap().clear();
    }

    pub fn need_to_wait(&self) -> bool {
        self.queue.lock().unwrap().is_empty()
    }
}

// Define a float type to easily apply changes here if needed
#[allow(non_camel_case_types)]
type float = f32;

#[cfg(test)]
mod test_queue {
    use std::{
        sync::Arc,
        thread::{self, sleep},
        time::Duration,
    };

    use super::BlockingQueue;

    #[test]
    fn test_count() {
        println!("Start test");
        let queue: Arc<BlockingQueue<u32>> = Arc::new(BlockingQueue::new());
        let handles: Vec<_> = (0..2000)
            .map(|_i| {
                let queue = queue.clone();
                thread::spawn(move || {
                    println!("Spawing thread {:?}", thread::current().id());
                    queue.dequeue();
                    println!("After dequeue")
                })
            })
            .collect();
        println!("before sleep");
        sleep(Duration::new(3, 0));
        println!("after sleep");
        queue.wake_all();
        // Try to join all handles, otherwise this test will timeout if some threads are not waked up
        handles.into_iter().for_each(|h| {
            h.join().unwrap();
        });
    }
}

#[cfg(test)]
mod test_data {
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
