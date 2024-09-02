use derive_more::From;
use std::any::TypeId;
use std::collections::{HashMap, HashSet};
use std::fmt::Debug;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, ThreadId};
use std::time::SystemTime;

use rand::distributions::Alphanumeric;
use rand::Rng;
use reqwest::header::ToStrError;
use rusty_weel_macro::get_str_from_value;

use crate::connection_wrapper::ConnectionWrapper;
use crate::data_types::{BlockingQueue, DynamicData, HTTPParams, State, StaticData, ThreadInfo};
use crate::dsl::DSL;
use crate::eval_helper::{self, EvalError};
use crate::redis_helper::{RedisHelper, Topic};

pub struct Weel {
    pub static_data: StaticData,
    pub dynamic_data: DynamicData,
    pub state: Mutex<State>,
    pub positions: Mutex<Vec<String>>,
    pub search_positions: Mutex<HashMap<String, Position>>,
    // Contains all open callbacks from async connections, ArcMutex as it is shared between the instance (to insert callbacks) and the callback thread (RedisHelper)
    pub callback_keys: Arc<Mutex<std::collections::HashMap<String, Arc<Mutex<ConnectionWrapper>>>>>,
    pub redis_notifications_client: Mutex<RedisHelper>,
    // Tracks all open votes via their ID. All voting needs to be finished before stopping.
    pub open_votes: Mutex<HashSet<String>>,
    // Stores a count and the last access for each call
    pub loop_guard: Mutex<HashMap<String, (u32, SystemTime)>>,
    // To allow threads to access parent data => thread_local storage is, as the name suggest, thread local
    // stores information such as the parents thread id, search mode ...
    // Invariant: When a thread is spawned within any weel method, thread information for this thread has to be created
    pub thread_information: Mutex<HashMap<ThreadId, ThreadInfo>>,
}

impl DSL for Weel {
    fn call(
        self: Arc<Self>,
        label: &str,
        endpoint_name: &str,
        parameters: HTTPParams,
        // Even though adding separate functions would be more idomatic for opt. parameters, the number and similar handling of these parameters would make it clunky to handle (2^4 variants)
        prepare_code: Option<&str>,
        update_code: Option<&str>,
        finalize_code: Option<&str>,
        rescue_code: Option<&str>,
    ) -> Result<()> {
        self.weel_activity(
            label,
            ActivityType::Call,
            Some(endpoint_name),
            Some(parameters),
            finalize_code,
            update_code,
            prepare_code,
            rescue_code,
        )
    }

    fn manipulate(self: Arc<Self>, label: &str, name: Option<&str>, code: &str) -> Result<()> {
        self.weel_activity(
            label,
            ActivityType::Manipulate,
            None,
            None,
            Some(code),
            None,
            None,
            None,
        )
    }

    fn parallel_do(
        &self,
        wait: Option<u32>,
        cancel: &str,
        start_branches: impl Fn() -> Result<()> + Sync,
    ) -> Result<()> {
        println!("Calling parallel_do");
        println!("Executing lambda");
        // start_branches();
        todo!()
    }

    fn parallel_branch(
        &self,
        /*data: &str,*/ lambda: impl Fn() -> Result<()> + Sync,
    ) -> Result<()> {
        println!("Executing parallel branch");
        thread::scope(|scope| {
            scope.spawn(|| {
                lambda();
            });
        });
        todo!()
    }

    fn choose(&self, variant: &str, lambda: impl Fn() -> Result<()> + Sync) -> Result<()> {
        println!("Executing choose");
        lambda();
        todo!()
    }

    fn alternative(&self, condition: &str, lambda: impl Fn() -> Result<()> + Sync) -> Result<()> {
        println!("Executing alternative, ignoring condition: {}", condition);
        lambda();
        todo!()
    }

    fn loop_exec(
        &self,
        condition: Result<bool>,
        lambda: impl Fn() -> Result<()> + Sync,
    ) -> Result<()> {
        println!("Executing loop!");
        lambda();
        todo!()
    }

    fn pre_test(&self, condition: &str) -> Result<bool> {
        todo!()
    }

    fn post_test(&self, condition: &str) -> Result<bool> {
        todo!()
    }

    fn stop(&self, label: &str) -> Result<()> {
        println!("Stopping... just kidding");
        todo!()
    }

    fn critical_do(&self, mutex_id: &str, lambda: impl Fn() -> Result<()> + Sync) -> Result<()> {
        println!("in critical do");
        lambda();
        todo!()
    }
}

impl Weel {
    /**
     * Starts execution
     * To pass it to execution thread we need Send + Sync
     */
    pub fn start(
        &self,
        model: impl Fn() -> Result<()> + Send + 'static,
        stop_signal_sender: Sender<()>,
    ) {
        let mut content = HashMap::new();
        content.insert("state".to_owned(), "running".to_owned());
        match self.vote("state/change", content) {
            Ok(voted_start) => {
                if voted_start {
                    {
                        // Use custom scope to ensure dropping occurs asap
                        self.positions.lock().unwrap().clear();
                        *self.state.lock().unwrap() = State::Running;
                    }
                    // TODO: implement the __weel_control_flow error handling logic in the handle_error/handle_join error
                    let instance_thread = thread::spawn(model);
                    let join_result = instance_thread.join();
                    // Signal stop thread that execution of model ended:
                    let send_result = stop_signal_sender.send(());
                    if matches!(send_result, Err(_)) {
                        log::error!("Error sending termination signal for model thread. Receiver must have been dropped.")
                    }

                    match join_result {
                        Ok(result) => {
                            match result {
                                Ok(()) => {
                                    match *self.state.lock().unwrap() {
                                        State::Running | State::Finishing => todo!(),
                                        State::Stopping => {}
                                        _ => {
                                            //Do nothing
                                        }
                                    }
                                }
                                Err(err) => handle_error(err),
                            }
                        }
                        Err(err) => handle_join_error(err),
                    }
                } else {
                    self.abort_start();
                };
            }
            Err(err) => handle_error(err),
        }
    }

    fn abort_start(&self) {
        let mut state = self.state.lock().expect("Could not lock state mutex");
        // Should only be called when the start is aborted through voting (aka. weel is still in ready state):
        assert_eq!(*state, State::Ready);
        *state = State::Stopped;
    }

    pub fn stop(&self, stop_signal_receiver: &mut Receiver<()>) -> Result<()> {
        {
            let mut state = self.state.lock().expect("Could not lock state mutex");
            match *state {
                State::Ready => *state = State::Stopped,
                State::Running => {
                    // TODO: Where will this be set to stopped?
                    *state = State::Stopping;
                    let rec_result = stop_signal_receiver.recv();
                    if matches!(rec_result, Err(_)) {
                        log::error!("Error receiving termination signal for model thread. Sender must have been dropped.")
                    }
                }
                _ => log::info!(
                    "Instance stop was called but instance is in state: {:?}",
                    *state
                ),
            }
        }

        let mut redis = self
            .redis_notifications_client
            .lock()
            .expect("Could not acquire mutex");
        for vote_id in self
            .open_votes
            .lock()
            .expect("Could not capture mutex")
            .iter()
        {
            redis.send(
                "vote-response",
                vote_id,
                self.static_data.get_instance_meta_data(),
                Some("true"),
            )?;
        }
        Ok(())
    }

    /*
     * Vote of controller
     */
    pub fn vote(&self, vote_topic: &str, mut content: HashMap<String, String>) -> Result<bool> {
        let static_data = &self.static_data;
        let (topic, name) = vote_topic
            .split_once("/")
            .expect("Vote topic did not contain / separator");
        let handler = format!("{}/vote/{}", topic, name);
        let mut votes: Vec<String> = Vec::new();
        let mut redis_helper: RedisHelper = RedisHelper::new(
            static_data,
            &format!(
                "Instance {} Vote | voting on: {}",
                static_data.instance_id, vote_topic
            ),
        )?;
        for client in redis_helper
            .extract_handler(&static_data.instance_id, &handler)
            .iter()
        {
            // Generate random ASCII string of length VOTE_KEY_LENGTH
            let vote_id: String = generate_random_key();
            content.insert("key".to_owned(), vote_id.to_string());
            content.insert(
                "attributes".to_owned(),
                // TODO: Check whether these are already "translated"
                serde_json::to_string(&static_data.attributes)
                    .expect("Could not serialize attributes"),
            );
            content.insert("subscription".to_owned(), client.clone());
            let content = serde_json::to_string(&content)
                .expect("Could not serialize content to json string");
            votes.push(vote_id);
            redis_helper.send(
                "vote",
                vote_topic,
                static_data.get_instance_meta_data(),
                Some(content.as_str()),
            )?;
        }

        if votes.len() > 0 {
            {
                self.open_votes
                    .lock()
                    .expect("could not lock votes")
                    .extend(votes.clone());
            }
            // TODO: Check whether topics have correct structure, check blocking_pub_sub function documentation.
            let topics = votes
                .iter()
                .map(|entry| format!("vote-response: {entry}"))
                .collect();

            let mut collected_votes = HashSet::new();
            redis_helper.blocking_pub_sub(
                topics,
                |payload: &str, _pattern: &str, _topic: Topic| {
                    let message = serde_json::json!(payload);
                    if message["content"].is_null() || message["name"].is_null() {
                        log::error!("Message content or name is null");
                        panic!("Message content or name is null")
                    }
                    // Check whether content directly contains boolean, otherwise look whether it is the text true, otherwise false
                    collected_votes.insert(
                        message["content"]
                            .as_bool()
                            .or(message["content"].as_str().map(|content| content == "true"))
                            .unwrap_or(false),
                    );
                    self.open_votes
                        .lock()
                        .expect("Could not lock votes ")
                        .remove(&get_str_from_value!(message["name"]));
                    self.cancel_callback(
                        message["name"]
                            .as_str()
                            .expect("Message does not contain name"),
                    )?;
                    let all_votes_collected = collected_votes.len() >= votes.len();
                    Ok(!all_votes_collected)
                },
            )?;
            Ok(!collected_votes.contains(&false))
        } else {
            Ok(true)
        }
    }

    /*
     * vote_sync_after of controller
     */
    fn vote_sync_after(&self, connection_wrapper: &ConnectionWrapper) -> Result<bool> {
        let content = connection_wrapper.construct_basic_content()?;
        self.vote("activity/syncing_after", content)
    }

    /*
     * vote_sync_before of controller
     */
    fn vote_sync_before(&self, connection_wrapper: &ConnectionWrapper, parameters: Option<HashMap<String, String>>) -> Result<bool> {
        let mut content = connection_wrapper.construct_basic_content()?;
        content.insert("parameters".to_owned(), serde_json::to_string(&parameters)?);
        self.vote("activity/syncing_before", content)
    }

    fn generate_content() {

    }

    pub fn callback(
        &self,
        connection_wrapper: Arc<Mutex<ConnectionWrapper>>,
        key: &str,
        mut content: HashMap<String, String>,
    ) -> Result<()> {
        content.insert("key".to_owned(), key.to_owned());
        let content =
            serde_json::to_string(&content).expect("could not serialize hashmap to string");
        self.redis_notifications_client
            .lock()
            .expect("Could not acquire Mutex")
            .send(
                "callback",
                "activity/content",
                self.static_data.get_instance_meta_data(),
                Some(&content),
            )?;
        self.callback_keys
            .lock()
            .expect("could not acquire Mutex")
            .insert(key.to_owned(), connection_wrapper.clone());
        Ok(())
    }

    pub fn cancel_callback(&self, key: &str) -> Result<()> {
        self.redis_notifications_client
            .lock()
            .expect("Could not acquire Mutex for notifications RedisHelper")
            .send(
                "callback-end",
                key,
                self.static_data.get_instance_meta_data(),
                None,
            )?;
        Ok(())
    }

    fn weel_activity(
        self: Arc<Self>,
        label: &str,
        activity_type: ActivityType,
        endpoint_name: Option<&str>,
        parameters: Option<HTTPParams>,
        finalize_code: Option<&str>,
        update_code: Option<&str>,
        prepare_code: Option<&str>,
        rescue_code: Option<&str>,
    ) -> Result<()> {
        let position = self.position_test(label)?;
        let search_mode = self.in_search_mode(Some(label));
        if search_mode {
            return Ok(());
        }

        let result: Result<()> = {
            let state = self.state.lock().unwrap();
            let invalid_state = match *state {
                State::Running => false,
                _ => true,
            };

            {
                let no_longer_necessary = self
                    .thread_information
                    .lock()
                    .unwrap()
                    .get(&thread::current().id())
                    .map(|info| info.no_longer_necessary)
                    .unwrap_or(true);

                if invalid_state || no_longer_necessary {
                    return Ok(());
                }
            }

            let current_thread = thread::current().id();
            let mut thread_info_map = self.thread_information.lock().unwrap();
            let thread_info = thread_info_map.get_mut(&current_thread).unwrap();

            thread_info.blocking_queue = Arc::new(BlockingQueue::new());
            let mut connection_wrapper = ConnectionWrapper::new(
                self.clone(),
                Some(position.to_owned()),
                Some(thread_info.blocking_queue.clone()),
            );
            // This allows thread_info to be dropped and thus we can get the mutable access to the parent thread
            let parent = thread_info.parent.clone();
            let branch_traces_id = thread_info.branch_traces_id.clone();
            

            if parent.is_some() && branch_traces_id.is_some() {
                let branch_trace_id = branch_traces_id.as_ref().unwrap();
                let parent_thread_info = thread_info_map
                    .get_mut(&parent.unwrap())
                    .unwrap();
                let traces = parent_thread_info.branch_traces.get_mut(branch_trace_id);
                match traces {
                    Some(traces) => traces.push(position.to_owned()),
                    None => {
                        parent_thread_info
                            .branch_traces
                            .insert(branch_trace_id.to_owned(), Vec::new());
                    }
                }
            }

            // TODO: wp = __weel_progress position, connectionwrapper.activity_uuid
            if search_mode /* == "after"*/ {
                Err(Error::Signal(Signal::Proceed))
            } else {
                match activity_type {
                    ActivityType::Manipulate => {
                        let state_stopping_or_finished = matches!(*self.state.lock().unwrap(), State::Stopping | State::Finishing);
                        if !self.vote_sync_before(&connection_wrapper, None)? {
                            Err(Error::Signal(Signal::Stop))
                        } else if state_stopping_or_finished {
                            Err(Error::Signal(Signal::Skip))
                        } else {
                            match finalize_code {
                                Some(finalize_code) => {
                                    connection_wrapper.activity_manipulate_handle(label);
                                    connection_wrapper.inform_activity_manipulate();
                                    eval_helper::evaluate_expression(dynamic_context, static_context, expression)
                                    Ok(())
                                },
                                None => Ok(()),
                            }
                        }
                    },
                    ActivityType::Call => todo!(),
                }
            }
        };

        todo!()
    }

    fn position_test<'a>(&self, label: &'a str) -> Result<&'a str> {
        let is_alpha_numeric = label.chars().all(char::is_alphanumeric);
        if is_alpha_numeric {
            Ok(label)
        } else {
            {
                *self.state.lock().unwrap() = State::Stopping
            };
            Err(Error::GeneralError(format!("position: {label} not valid")))
        }
    }

    fn in_search_mode(&self, label: Option<&str>) -> bool {
        let thread = thread::current();
        let mut thread_info_map = self.thread_information.lock().unwrap();
        // We unwrap here but we need to ensure that when the weel creates a thread, it registers the thread info!
        let mut thread_info = thread_info_map.get_mut(&thread.id()).unwrap();

        if let Some(label) = label {
            let found_position = thread_info.branch_search
                && self
                    .search_positions
                    .lock()
                    .unwrap()
                    .contains_key(&label.to_owned());
            if found_position {
                thread_info.branch_search = false;
                thread_info.branch_search_now = true;
                while let Some(parent) = thread_info.parent {
                    // Each parent thread has to have some thread information. In general all threads should, when they spawn via weel register and add their thread information
                    thread_info = thread_info_map.get_mut(&parent).unwrap();
                    thread_info.branch_search = false;
                    thread_info.branch_search_now = true;
                }
                // checked earlier for membership, thus we can simply unwrap:
                self.search_positions
                    .lock()
                    .unwrap()
                    .get(label)
                    .unwrap()
                    .detail
                    == Mark::After
            } else {
                true
            }
        } else {
            true
        }
    }
}

fn handle_join_error(err: Box<dyn std::any::Any + Send>) {
    if TypeId::of::<String>() == err.type_id() {
        let x = err.downcast::<String>();
        match x {
            Ok(x) => log::error!("Model thread paniced: {}", x),
            Err(_err) => log::error!(
                "Model thread paniced but provided panic result cannot be cast into a String."
            ),
        }
    };
}

fn handle_error(err: Error) {
    todo!()
}

#[derive(Debug, From)]
pub enum Error {
    GeneralError(String),
    InvalidHeaderValue(reqwest::header::InvalidHeaderValue),
    InvalidHeaderName(reqwest::header::InvalidHeaderName),
    JsonError(serde_json::Error),
    ReqwestError(reqwest::Error),
    ToStrError(ToStrError),
    IOError(std::io::Error),
    RedisError(redis::RedisError),
    EvalError(EvalError),
    StrUTF8Error(std::str::Utf8Error),
    StringUTF8Error(std::string::FromUtf8Error),
    HttpHelperError(http_helper::Error),
    PoisonError(),
    FromStrError(mime::FromStrError),
    Signal(Signal)
}

#[derive(Debug)]
pub enum Signal {
    Again,
    Salvage,
    Stop,
    Proceed,
    Skip
}

pub enum ActivityType {
    Call,
    Manipulate,
}

pub struct Position {
    detail: Mark,
}

#[derive(PartialEq, Eq)]
pub enum Mark {
    At,
    After,
    Unmark,
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn as_str(&self) -> &str {
        match self {
            Error::GeneralError(message) => message.as_str(),
            Error::JsonError(_) => todo!(),
            Error::IOError(_) => todo!(),
            Error::RedisError(_) => todo!(),
            Error::EvalError(_) => todo!(),
            Error::StrUTF8Error(_) => todo!(),
            Error::StringUTF8Error(_) => todo!(),
            Error::InvalidHeaderValue(_) => todo!(),
            Error::InvalidHeaderName(_) => todo!(),
            Error::ReqwestError(_) => todo!(),
            Error::ToStrError(_) => todo!(),
            Error::HttpHelperError(_) => todo!(),
            Error::PoisonError() => todo!(),
            Error::FromStrError(_) => todo!(),
        }
    }
}

impl<T> From<PoisonError<T>> for Error {
    fn from(value: PoisonError<T>) -> Self {
        Error::PoisonError()
    }
}

const KEY_LENGTH: usize = 32;

/**
 * Generates random ASCII character string of length KEY_LENGTH
 */
pub fn generate_random_key() -> String {
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(KEY_LENGTH)
        .map(char::from)
        .collect()
}
