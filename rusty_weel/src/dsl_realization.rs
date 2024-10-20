use derive_more::From;
use redis::Value;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::any::{Any, TypeId};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fmt::Debug;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, ThreadId};
use std::time::SystemTime;

use rand::distributions::Alphanumeric;
use rand::Rng;
use reqwest::header::ToStrError;
use rusty_weel_macro::get_str_from_value;

use crate::connection_wrapper::{self, ConnectionWrapper};
use crate::data_types::{
    BlockingQueue, CancelCondition, ChooseVariant, DynamicData, HTTPParams, InstanceMetaData,
    State, StaticData, Status, ThreadInfo,
};
use crate::dsl::DSL;
use crate::eval_helper::{self, EvalError};
use crate::redis_helper::{RedisHelper, Topic};

static EVALUATION_LOCK: Mutex<()> = Mutex::new(());

pub struct Weel {
    pub opts: StaticData,
    // Also static, but is persisted within redis and thus a separate struct
    pub attributes: HashMap<String, String>,
    pub context: Mutex<DynamicData>,
    pub state: Mutex<State>,
    pub status: Mutex<Status>,
    pub positions: Mutex<Vec<Position>>,
    // The positions we search for -> Positions from which we start the execution
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
    // Use ref cell here to allow immutable borrows -> Allows to independently borrow distinct elements
    pub thread_information: Mutex<HashMap<ThreadId, RefCell<ThreadInfo>>>,
    pub stop_signal_receiver: Mutex<Receiver<()>>,
}

impl DSL for Weel {
    fn call(
        self: Arc<Self>,
        id: &str,
        endpoint_name: &str,
        parameters: HTTPParams,
        // Even though adding separate functions would be more idomatic for opt. parameters, the number and similar handling of these parameters would make it clunky to handle (2^4 variants)
        prepare_code: Option<&str>,
        update_code: Option<&str>,
        finalize_code: Option<&str>,
        rescue_code: Option<&str>,
    ) -> Result<()> {
        self.weel_activity(
            id,
            ActivityType::Call,
            prepare_code,
            update_code,
            rescue_code,
            finalize_code,
            Some(parameters),
            Some(endpoint_name),
        )
    }

    fn manipulate(self: Arc<Self>, label: &str, code: &str) -> Result<()> {
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

    fn choose(
        self: Arc<Self>,
        variant: ChooseVariant,
        lambda: impl Fn() -> Result<()> + Sync,
    ) -> Result<()> {
        if matches!(
            *self.state.lock().unwrap(),
            State::Stopping | State::Finishing | State::Stopped
        ) {
            return Ok(());
        }

        let current_thread = thread::current().id();
        let thread_info_map = self.thread_information.lock().unwrap();
        // Unwrap as we have precondition that thread info is available on spawning
        let mut thread_info = thread_info_map.get(&current_thread).unwrap().borrow_mut();
        thread_info.alternative_executed.push(false);
        thread_info.alternative_mode.push(variant);

        let connection_wrapper = ConnectionWrapper::new(self.clone(), None, None);
        connection_wrapper.split_branches(&thread_info.branch_traces)?;
        drop(thread_info);
        drop(thread_info_map);
        log::debug!("before lambda");
        self.clone().execute_lambda(lambda)?;
        log::debug!("after lambda");
        let current_thread = thread::current().id();
        let thread_info_map = self.thread_information.lock().unwrap();
        // Unwrap as we have precondition that thread info is available on spawning
        let mut thread_info = thread_info_map.get(&current_thread).unwrap().borrow_mut();

        thread_info.alternative_executed.pop();
        thread_info.alternative_mode.pop();
        Ok(())
    }

    fn alternative(
        self: Arc<Self>,
        condition: &str,
        lambda: impl Fn() -> Result<()> + Sync,
    ) -> Result<()> {
        if matches!(
            *self.state.lock().unwrap(),
            State::Stopping | State::Finishing | State::Stopped
        ) {
            return Ok(());
        }

        let error_message =
            "Should be present as alternative is called within a choose that pushes element in";
        let current_thread = thread::current().id();

        let thread_info_map = self.thread_information.lock().unwrap();
        // Unwrap as we have precondition that thread info is available on spawning
        let thread_info = thread_info_map.get(&current_thread).unwrap().borrow_mut();

        let choice_is_exclusive = matches!(
            thread_info.alternative_mode.last().expect(error_message,),
            ChooseVariant::Exclusive
        );
        
        let other_branch_executed = *thread_info
            .alternative_executed
            .last()
            .expect(error_message);
        if choice_is_exclusive && other_branch_executed {
            return Ok(());
        }

        drop(thread_info);
        drop(thread_info_map);
        let condition_res = self.clone().evaluate_condition(condition)?;
        log::info!("Condition {condition}, was evaluated to: {}", condition_res);
        // Make sure only one thread is executed for choice
        
        let thread_info_map = self.thread_information.lock().unwrap();
        // Unwrap as we have precondition that thread info is available on spawning
        let mut thread_info = thread_info_map.get(&current_thread).unwrap().borrow_mut();
        if condition_res {
            *thread_info
                .alternative_executed
                .last_mut()
                .expect(error_message) = true;
        }

        drop(thread_info);
        drop(thread_info_map);
        let in_search_mode = self.in_search_mode(None);

        if condition_res || in_search_mode {
            self.execute_lambda(lambda)?;
        }
        log::debug!("at end of alternative1");

        
        if in_search_mode != self.in_search_mode(None) {
            let current_thread = thread::current().id();
            let thread_info_map = self.thread_information.lock().unwrap();
            // Unwrap as we have precondition that thread info is available on spawning
            let mut thread_info = thread_info_map.get(&current_thread).unwrap().borrow_mut();
            *thread_info
                .alternative_executed
                .last_mut()
                .expect(error_message) = true;
        }
        log::debug!("at end of alternative2");
        Ok(())
    }

    fn otherwise(self: Arc<Self>, lambda: impl Fn() -> Result<()> + Sync) -> Result<()>{
        log::debug!("in otherwise");
        if matches!(
            *self.state.lock().unwrap(),
            State::Stopping | State::Finishing | State::Stopped
        ) {
            return Ok(());
        };
        let current_thread = thread::current().id();
        let thread_info_map = self.thread_information.lock().unwrap();
        // Unwrap as we have precondition that thread info is available on spawning
        let thread_info = thread_info_map.get(&current_thread).unwrap().borrow_mut();
        let alternative_executed = *thread_info.alternative_executed.last().expect(
            "Should be present as alternative is called within a choose that pushes element in",
        );
        drop(thread_info);
        drop(thread_info_map);
        if self.in_search_mode(None)
        || !alternative_executed
        {
            self.execute_lambda(lambda)?;
        }
        Ok(())
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

    fn critical_do(&self, mutex_id: &str, lambda: impl Fn() -> Result<()> + Sync) -> Result<()> {
        println!("in critical do");
        lambda();
        todo!()
    }

    fn stop(self: Arc<Self>, id: &str) -> Result<()> {
        let in_search_mode = self.in_search_mode(None);
        if in_search_mode {
            return Ok(());
        }
        let current_thread = thread::current().id();
        let thread_info_map = self.thread_information.lock().unwrap();
        let thread_info = thread_info_map.get(&current_thread).unwrap().borrow_mut();
        let no_longer_necessary = thread_info.no_longer_necessary;
        // Unwrap as we have precondition that thread info is available on spawning
        if matches!(
            *self.state.lock().unwrap(),
            State::Stopping | State::Stopped | State::Finishing
        ) || no_longer_necessary
        {
            return Ok(());
        }

        //TODO:gather traces in threads...
        if let Some(parent) = &thread_info.parent {
            let mut parent_thread_info = thread_info_map
                .get(parent)
                .expect("Since we have a reference, threadinfo of parent has to exist")
                .borrow_mut();
            if !parent_thread_info
                .branch_traces
                .contains_key(&thread_info.branch_id)
            {
                parent_thread_info
                    .branch_traces
                    .insert(thread_info.branch_id, Vec::new());
            }
            parent_thread_info
                .branch_traces
                .get_mut(&thread_info.branch_id)
                .unwrap()
                .push(id.to_owned());
        }
        self.weel_progress(id.to_owned(), "0".to_owned(), true)?;
        self.clone().set_state(State::Stopping)?;
        Ok(())
    }

    fn terminate(self: Arc<Self>) -> Result<()> {
        let in_search_mode = self.in_search_mode(None);
        if in_search_mode {
            return Ok(());
        }
        let current_thread = thread::current().id();
        let thread_info_map = self.thread_information.lock().unwrap();
        let thread_info = thread_info_map.get(&current_thread).unwrap().borrow_mut();
        let no_longer_necessary = thread_info.no_longer_necessary;
        // Unwrap as we have precondition that thread info is available on spawning
        if matches!(
            *self.state.lock().unwrap(),
            State::Stopping | State::Stopped | State::Finishing
        ) || no_longer_necessary
        {
            return Ok(());
        }

        *self.state.lock().unwrap() = State::Finishing;
        Ok(())
    }
}

impl Weel {
    /**
     * Starts execution
     * To pass it to execution thread we need Send + Sync
     */
    pub fn start(
        self: Arc<Self>,
        model: impl FnOnce() -> Result<()> + Send + 'static,
        stop_signal_sender: Sender<()>,
    ) -> Result<()> {
        let content = json!({
            "state": "running"
        });
        match self.vote("state/change", content) {
            Ok(voted_start) => {
                if voted_start {
                    {
                        // Use custom scope to ensure dropping occurs asap
                        self.positions.lock().unwrap().clear();
                        self.clone().set_state(State::Running)?;
                    }
                    // TODO: implement the __weel_control_flow error handling logic in the handle_error/handle_join error
                    let result = model();
                    // Signal stop thread that execution of model ended:
                    let send_result = stop_signal_sender.send(());
                    if matches!(send_result, Err(_)) {
                        log::error!("Error sending termination signal for model thread. Receiver must have been dropped.")
                    }

                    match result {
                        // TODO: Implement __weel_control_flow completely
                        Ok(()) => {
                            let mut state = self.state.lock().unwrap();
                            match *state {
                                State::Running | State::Finishing => {
                                    let positions = self.positions.lock().unwrap().clone();
                                    let ipc = json!({
                                        "unmark": positions
                                    });
                                    match ConnectionWrapper::new(self.clone(), None, None)
                                        .inform_position_change(Some(ipc))
                                    {
                                        Ok(()) => {
                                            drop(state);
                                            self.clone().set_state(State::Finished)?;
                                        }
                                        Err(err) => {
                                            self.handle_error(err);
                                        }
                                    };
                                }
                                State::Stopping => {
                                    self.recursive_join();
                                    *state = State::Stopped;
                                    match ConnectionWrapper::new(self.clone(), None, None)
                                        .inform_state_change(State::Stopped)
                                    {
                                        Ok(()) => {}
                                        Err(err) => {
                                            self.handle_error(err);
                                        }
                                    };
                                }
                                _ => {
                                    log::error!("Recached end of process in state: {:?}", state)
                                    //Do nothing
                                }
                            }
                        }
                        Err(err) => self.handle_error(err),
                    }
                } else {
                    self.abort_start();
                };
            }
            Err(err) => self.handle_error(err),
        }
        Ok(())
    }

    fn recursive_join(&self) {
        // TODO: Implement recursive join, this should not be required?
        log::error!("Recursive join not yet impemented/removed")
    }

    fn abort_start(&self) {
        let mut state = self.state.lock().expect("Could not lock state mutex");
        // Should only be called when the start is aborted through voting (aka. weel is still in ready state):
        assert_eq!(*state, State::Ready);
        *state = State::Stopped;
    }

    pub fn stop_weel(&self) -> Result<()> {
        {
            log::info!("Entered stop function of weel");
            let mut state = self.state.lock().expect("Could not lock state mutex");
            log::info!("Acquired lock for state");
            match *state {
                State::Ready => *state = State::Stopped,
                State::Running => {
                    // TODO: Where will this be set to stopped?
                    *state = State::Stopping;
                    // Wait for instance to stop
                    drop(state);
                    let rec_result = self.stop_signal_receiver.lock().unwrap().recv();
                    if matches!(rec_result, Err(_)) {
                        log::error!("Error receiving termination signal for model thread. Sender must have been dropped.")
                    }
                }
                _ => log::info!(
                    "Instance stop was called but instance is in state: {:?}",
                    *state
                ),
            }
            log::info!("Exit stop function of weel");
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
                self.get_instance_meta_data(),
                Some(serde_json::Value::Bool(true)),
            )?;
        }
        Ok(())
    }

    /**
     * Allows veto-voting on arbitrary topics and will return true if no veto was cast (otherwise false)
     *
     * Vote of controller
     *
     * Locks:
     *  - open_votes
     */
    pub fn vote(&self, vote_topic: &str, mut content_node: serde_json::Value) -> Result<bool> {
        let static_data = &self.opts;
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
        let content = content_node
            .as_object_mut()
            .expect("content has to be an object");
        for client in redis_helper
            .extract_handler(static_data.instance_id, &handler)
            .iter()
        {
            // Generate random ASCII string of length VOTE_KEY_LENGTH
            let vote_id: String = generate_random_key();
            content.insert("key".to_owned(), json!(vote_id));
            content.insert(
                "attributes".to_owned(),
                // TODO: Check whether these are already "translated"
                json!(self.attributes),
            );
            content.insert("subscription".to_owned(), json!(client));
            votes.push(vote_id);
            redis_helper.send(
                "vote",
                vote_topic,
                self.get_instance_meta_data(),
                Some(json!(content)),
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
        let content = connection_wrapper.construct_basic_content();
        self.vote("activity/syncing_after", content)
    }

    /*
     * vote_sync_before of controller
     */
    fn vote_sync_before(
        &self,
        connection_wrapper: &ConnectionWrapper,
        parameters: Option<HashMap<String, String>>,
    ) -> Result<bool> {
        let mut content_node = connection_wrapper.construct_basic_content();
        let content = content_node
            .as_object_mut()
            .expect("Construct basic content has to return a json object");
        content.insert("parameters".to_owned(), json!(parameters));
        self.vote("activity/syncing_before", content_node)
    }

    /**
     * Registers a callback
     */
    pub fn register_callback(
        &self,
        connection_wrapper: Arc<Mutex<ConnectionWrapper>>,
        key: &str,
        mut content_node: serde_json::Value,
    ) -> Result<()> {
        let content = content_node
            .as_object_mut()
            .expect("Construct basic content has to return json object");
        content.insert("key".to_owned(), serde_json::Value::String(key.to_owned()));
        self.redis_notifications_client
            .lock()
            .expect("Could not acquire Mutex")
            .send(
                "callback",
                "activity/content",
                self.get_instance_meta_data(),
                Some(json!(content)),
            )?;
        self.callback_keys
            .lock()
            .expect("could not acquire Mutex")
            .insert(key.to_owned(), connection_wrapper);
        Ok(())
    }

    /**
     * Removes a registered callback
     *
     * Locks:
     *  - redis_notification_client
     */
    pub fn cancel_callback(&self, key: &str) -> Result<()> {
        log::info!("Remove callback: {key}");
        self.redis_notifications_client
            .lock()
            .expect("Could not acquire Mutex for notifications RedisHelper")
            .send("callback-end", key, self.get_instance_meta_data(), None)?;
        Ok(())
    }

    /**
     * Executes the activity, handles errors
     *   - handles search mode: skips activities if in search mode
     *
     * Locks:
     *  - state (shortly)
     *  - `thread_information`
     *  - `ThreadInfo` of the current thread (within the thread_information)
     */
    fn weel_activity(
        self: Arc<Self>,
        activity_id: &str,
        activity_type: ActivityType,
        prepare_code: Option<&str>,
        update_code: Option<&str>,
        rescue_code: Option<&str>,
        finalize_code: Option<&str>,
        parameters: Option<HTTPParams>,
        endpoint_name: Option<&str>,
    ) -> Result<()> {
        let position = self.clone().position_test(activity_id)?;
        let in_search_mode = self.in_search_mode(Some(activity_id));
        if in_search_mode {
            return Ok(());
        }
        let connection_wrapper =
            ConnectionWrapper::new(self.clone(), Some(position.to_owned()), None);
        let connection_wrapper_mutex = Arc::new(Mutex::new(connection_wrapper));

        let mut weel_position;

        /*
         * We use a block computation here to mimick the exception handling -> If an exception in the original ruby code is raised, we return it here
         */
        let result: Result<()> = 'raise: {
            let current_thread = thread::current().id();
            let thread_info_map = self.thread_information.lock().unwrap();
            // Unwrap as we have precondition that thread info is available on spawning
            let mut thread_info = thread_info_map.get(&current_thread).unwrap().borrow_mut();

            // Check early return
            let in_invalid_state = match *self.state.lock().unwrap() {
                State::Running => false,
                _ => true,
            };
            if in_invalid_state || thread_info.no_longer_necessary {
                return Ok(());
            }

            thread_info.blocking_queue = Arc::new(Mutex::new(BlockingQueue::new()));
            let mut connection_wrapper = connection_wrapper_mutex.lock().unwrap();
            connection_wrapper.handler_continue = Some(thread_info.blocking_queue.clone());

            let parent = thread_info.parent.clone();

            // Register position/label of this thread in the branch traces of the parent thread
            if parent.is_some() {
                let mut parent_thread_info =
                    thread_info_map.get(&parent.unwrap()).unwrap().borrow_mut();
                let traces = parent_thread_info
                    .branch_traces
                    .get_mut(&thread_info.branch_id);
                match traces {
                    Some(traces) => traces.push(position.to_owned()),
                    None => {
                        parent_thread_info
                            .branch_traces
                            .insert(thread_info.branch_id, Vec::new());
                    }
                }
            };

            // Local information should not change outside of this thread TODO: add this to actual thread_local_storage
            let local = thread_info.local.clone();
            // Drop the thread_info here already as for a manipulate we do not need it at all and a call we need to acquire the lock every 'again loop anyway
            drop(thread_info);
            drop(thread_info_map);
            weel_position = self.weel_progress(
                position.to_owned(),
                connection_wrapper.handler_activity_uuid.clone(),
                false,
            )?;
            match activity_type {
                ActivityType::Manipulate => {
                    let state_stopping_or_finishing = matches!(
                        *self.state.lock().unwrap(),
                        State::Stopping | State::Finishing
                    );
                    if !self.vote_sync_before(&connection_wrapper, None)? {
                        break 'raise Err(Signal::Stop.into());
                    } else if state_stopping_or_finishing {
                        break 'raise Err(Signal::Skip.into());
                    }
                    match finalize_code {
                        Some(finalize_code) => {
                            connection_wrapper.activity_manipulate_handle(activity_id);
                            connection_wrapper.inform_activity_manipulate()?;
                            let result = match self.clone().execute_code(
                                false,
                                finalize_code,
                                &local,
                                &connection_wrapper,
                                &format!("Activity {}", position),
                                // In a manipulate, we do not have data available from a prior request
                                None,
                                None,
                            ) {
                                Ok(res) => res,
                                // For manipulate, we just pass all signals/errors downward
                                Err(err) => break 'raise Err(err),
                            };
                            connection_wrapper.inform_manipulate_change(result)?;
                        }
                        None => (),
                    };
                    connection_wrapper.inform_activity_done()?;
                    weel_position.detail = "after".to_owned();
                    let ipc = json!({
                        "after": weel_position
                    });
                    ConnectionWrapper::new(self.clone(), None, None)
                        .inform_position_change(Some(ipc))?;
                }
                ActivityType::Call => {
                    drop(connection_wrapper);
                    'again: loop {
                        // Reacquire thread information mutex every loop again as we might need to drop it during wait
                        let current_thread = thread::current().id();
                        let thread_info_map = self.thread_information.lock().unwrap();
                        // Unwrap as we have precondition that thread info is available on spawning
                        let thread_info =
                            thread_info_map.get(&current_thread).unwrap().borrow_mut();
                        // TODO: In manipulate we directly "abort" and do not run code, here we run code and then check for abort, is this correct?
                        let mut connection_wrapper = connection_wrapper_mutex.lock().unwrap();
                        let parameters = match connection_wrapper.prepare(
                            prepare_code,
                            thread_info.local.clone(),
                            &vec![endpoint_name.unwrap()],
                            parameters.clone().expect(
                                "The activity type call requires parameters to be provided",
                            ),
                        ) {
                            // When error/signal returned, pass it downwards for handling, except for Signal::Again that has some direct effects
                            Ok(res) => res,
                            Err(err) => match err {
                                Error::EvalError(eval_error) => match eval_error {
                                    EvalError::Signal(signal, _evaluation_result) => {
                                        match signal {
                                            // If signal again is raised by prepare code -> retry
                                            Signal::Again => continue 'again,
                                            other => break 'raise Err(other.into()),
                                        }
                                    }
                                    other => break 'raise Err(Error::EvalError(other)),
                                },
                                other_error => break 'raise Err(other_error),
                            },
                        };

                        let state_stopping_or_finishing = matches!(
                            *self.state.lock().unwrap(),
                            State::Stopping | State::Finishing
                        );

                        // Drop info before we enter blocking vote_sync_before
                        drop(thread_info);
                        drop(thread_info_map);
                        if !self.vote_sync_before(&connection_wrapper, None)? {
                            break 'raise Err(Signal::Stop.into());
                        } else if state_stopping_or_finishing {
                            break 'raise Err(Signal::Skip.into());
                        }

                        // Will be locked in the activity_handle again
                        drop(connection_wrapper);
                        // This executes the actual call
                        ConnectionWrapper::activity_handle(
                            &connection_wrapper_mutex,
                            weel_position
                                .handler_passthrough
                                .as_ref()
                                .map(|x| x.as_str()),
                            parameters,
                        )?;
                        let connection_wrapper = connection_wrapper_mutex.lock().unwrap();
                        weel_position.handler_passthrough =
                            connection_wrapper.handler_passthrough.clone();
                        if let Some(_) = &weel_position.handler_passthrough {
                            let connection_wrapper = ConnectionWrapper::new(
                                self.clone(),
                                // Do not need this data for the inform:
                                None,
                                None,
                            );
                            let content = json!({
                                "wait": weel_position
                            });
                            connection_wrapper.inform_position_change(Some(content))?;
                        };
                        drop(connection_wrapper);

                        'inner: loop {
                            let current_thread = thread::current().id();
                            let thread_info_map = self.thread_information.lock().unwrap();
                            // Unwrap as we have precondition that thread info is available on spawning
                            let thread_info =
                                thread_info_map.get(&current_thread).unwrap().borrow();
                            let state_stopping_or_finishing = matches!(
                                *self.state.lock().unwrap(),
                                State::Stopping | State::Stopped | State::Finishing
                            );
                            let connection_wrapper = connection_wrapper_mutex.lock().unwrap();

                            let should_block =
                                !state_stopping_or_finishing && !thread_info.no_longer_necessary;
                            let mut wait_result = None;

                            // Get reference on the queue to allow us to unlock the rest of the thread info
                            // TODO: Maybe put the blocking queue info into real thread local storage
                            let thread_queue = thread_info.blocking_queue.clone();
                            // We need to release the locks on the thread_info_map to allow other parallel branches to execute while we wait for the callback (can take long for async case)
                            drop(thread_info);
                            drop(thread_info_map);
                            // We need to release the connection_wrapper lock here to allow callbacks from redis to lock the wrapper
                            drop(connection_wrapper);

                            if should_block {
                                log::info!("Waiting...");
                                wait_result = Some(thread_queue.lock().unwrap().dequeue());
                                log::info!("Waited")
                            };

                            // Reacquire locks after waiting
                            let current_thread = thread::current().id();
                            let thread_info_map = self.thread_information.lock().unwrap();
                            // Unwrap as we have precondition that thread info is available on spawning
                            let thread_info =
                                thread_info_map.get(&current_thread).unwrap().borrow();
                            let connection_wrapper = connection_wrapper_mutex.lock().unwrap();

                            if thread_info.no_longer_necessary {
                                // TODO: Definition of this method is basically empty?
                                connection_wrapper.activity_no_longer_necessary();
                                break 'raise Err(Signal::NoLongerNecessary.into());
                            }
                            // Store local for code execution -> allows us to unlock the thread_local_map here
                            let local = thread_info.local.clone();
                            drop(thread_info);
                            drop(thread_info_map);

                            let state_stopping_or_finishing = matches!(
                                *self.state.lock().unwrap(),
                                State::Stopping | State::Stopped | State::Finishing
                            );
                            if state_stopping_or_finishing {
                                connection_wrapper.activity_stop()?;
                                weel_position.handler_passthrough =
                                    connection_wrapper.activity_passthrough_value();
                                break 'raise Err(Signal::Proceed.into());
                            };

                            let signaled_update_again = wait_result
                                .as_ref()
                                .map(|res| matches!(res, Signal::Again))
                                .unwrap_or(false);
                            let return_value_empty = connection_wrapper
                                .handler_return_value
                                .clone()
                                .map(|x| x.is_empty())
                                .unwrap_or(true);
                            if signaled_update_again && return_value_empty {
                                continue;
                            }

                            let code_type;
                            let signaled_update_again = wait_result
                                .as_ref()
                                .map(|res| matches!(res, Signal::UpdateAgain))
                                .unwrap_or(false);
                            let signaled_salvage = wait_result
                                .as_ref()
                                .map(|res| matches!(res, Signal::Salvage))
                                .unwrap_or(false);
                            let code = if signaled_update_again {
                                code_type = "update";
                                update_code
                            } else if signaled_salvage {
                                if rescue_code.is_some() {
                                    code_type = "salvage";
                                    rescue_code
                                } else {
                                    break 'raise Err(Error::GeneralError(format!(
                                        "Service returned status code {:?}, and no salvage/rescue code was provided",
                                        connection_wrapper.handler_return_status
                                    )));
                                }
                            } else {
                                code_type = "finalize";
                                finalize_code
                            };

                            connection_wrapper.inform_activity_manipulate()?;
                            if let Some(code) = code {
                                let mut signaled_again = false;
                                let result = match self.execute_code(
                                    false,
                                    code,
                                    &local,
                                    &connection_wrapper,
                                    &format!("Activity {} {}", position, code_type),
                                    connection_wrapper.handler_return_value.clone(),
                                    connection_wrapper.handler_return_options.clone()
                                ) // TODO: Even in signal case we need the eval result
                                {
                                    // When error/signal returned, pass it downwards for handling, except for Signal::Again that has some direct effects 
                                    Ok(res) => res,
                                    Err(err) => match err {
                                        Error::EvalError(eval_error) => {
                                            match eval_error {
                                                EvalError::Signal(signal, evaluation_result) => {
                                                    match signal {
                                                        Signal::Again => {
                                                            signaled_again = true;
                                                            evaluation_result
                                                        },
                                                        other => break 'raise Err(Error::Signal(other))
                                                    }
                                                },
                                                other_eval_error => break 'raise Err(Error::EvalError(other_eval_error)),
                                            }
                                        },
                                        other_error => break 'raise Err(other_error)
                                    }
                                };
                                connection_wrapper.inform_manipulate_change(result)?;

                                if signaled_again {
                                    continue 'again;
                                }
                            }
                            if !signaled_update_again {
                                // If wait result was not UpdateAgain -> Break out, otherwise continue inner loop
                                break 'inner;
                            }
                        }
                        let connection_wrapper = connection_wrapper_mutex.lock().unwrap();
                        if connection_wrapper.activity_passthrough_value().is_none() {
                            connection_wrapper.inform_activity_done()?;
                            weel_position.handler_passthrough = None;
                            weel_position.detail = "after".to_owned();
                            let content = json!({
                                "after": weel_position
                            });

                            ConnectionWrapper::new(self.clone(), None, None)
                                .inform_position_change(Some(content))?;
                        }
                        break 'again;
                    }
                }
            }
            // -> Feels very wrong, to do this but in this case the code treats this as an error, so will we
            Err(Signal::Proceed.into())
        };

        let connection_wrapper = connection_wrapper_mutex.lock().unwrap();
        if let Err(error) = result {
            match error {
                Error::Signal(signal) => match signal {
                    Signal::Proceed | Signal::SkipManipulate => {
                        let state_stopping_or_finishing = matches!(
                            *self.state.lock().unwrap(),
                            State::Stopping | State::Finishing
                        );

                        if !state_stopping_or_finishing
                            && !self.vote_sync_after(&connection_wrapper)?
                        {
                            self.clone().set_state(State::Stopping)?;
                            weel_position.detail = "unmark".to_owned();
                        }
                    }
                    Signal::NoLongerNecessary => {
                        connection_wrapper.inform_activity_done()?;
                        self.positions
                            .lock()
                            .unwrap()
                            .retain(|pos| *pos != weel_position);
                        let current_thread = thread::current().id();
                        let thread_info_map = self.thread_information.lock().unwrap();
                        // Unwrap as we have precondition that thread info is available on spawning
                        let mut thread_info =
                            thread_info_map.get(&current_thread).unwrap().borrow_mut();
                        thread_info.branch_position = None;
                        weel_position.handler_passthrough = None;
                        weel_position.detail = "unmark".to_owned();
                        let ipc = json!({
                            "unmark": [weel_position]
                        });
                        ConnectionWrapper::new(self.clone(), None, None)
                            .inform_position_change(Some(ipc))?;
                    }
                    Signal::Stop | Signal::StopSkipManipulate => {
                        self.clone().set_state(State::Stopping)?;
                    }
                    Signal::Skip => {
                        log::info!("Received skip signal. Do nothing")
                    }
                    x => {
                        log::error!("Received unexpected signal: {:?}", x);
                    }
                },
                Error::EvalError(eval_error) => match eval_error {
                    EvalError::SyntaxError(message) => {
                        connection_wrapper.inform_activity_failed(Error::EvalError(
                            EvalError::SyntaxError(message),
                        ))?;
                        self.clone().set_state(State::Stopping)?;
                    }
                    EvalError::Signal(_signal, _evaluation_result) => {
                        log::error!("Handling EvalError::Signal in weel_activity, this should never happen! Should be \"raised\" as Error::Signal");
                        panic!("Handling EvalError::Signal in weel_activity, this should never happen! Should be \"raised\" as Error::Signal");
                    }
                    // Runtime and general evaluation errors use the default error handling
                    other => {
                        self.handle_error(Error::EvalError(other));
                    }
                },
                err => {
                    self.handle_error(err);
                }
            };
        };
        {
            // Original ensure block
            let current_thread = thread::current().id();
            let thread_info_map = self.thread_information.lock().unwrap();
            // Unwrap as we have precondition that thread info is available on spawning
            let mut thread_info = thread_info_map.get(&current_thread).unwrap().borrow_mut();

            if let Some(parent_id) = thread_info.parent {
                let mut parent_info = thread_info_map.get(&parent_id).unwrap().borrow_mut();

                if parent_info.branch_wait_count_cancel_condition == CancelCondition::First {
                    if !thread_info.branch_wait_count_cancel_active
                        && parent_info.branch_wait_count_cancel < parent_info.branch_wait_count
                    {
                        thread_info.branch_wait_count_cancel_active = true;
                        parent_info.branch_wait_count_cancel =
                            parent_info.branch_wait_count_cancel + 1;
                    }
                }
                let state_not_stopping_or_finishing = match *self.state.lock().unwrap() {
                    State::Stopping | State::Finishing => false,
                    _other => true,
                };
                if parent_info.branch_wait_count_cancel == parent_info.branch_wait_count
                    && state_not_stopping_or_finishing
                {
                    // Will iteratively mark all children as no longer necessary
                    for child_id in &parent_info.branches {
                        match thread_info_map.get(child_id) {
                            Some(thread_info) => {
                                let mut thread_info = thread_info.borrow_mut();
                                if !thread_info.branch_wait_count_cancel_active {
                                    thread_info.no_longer_necessary = true;
                                    drop(thread_info);
                                    // Should be fine w.r.t. mutable borrows, since this will continue recusively down the hieararchy
                                    recursive_continue(&thread_info_map, &current_thread)
                                }
                            }
                            None => {
                                log::info!("Child Thread of Thread {:?} with id: {:?} does not have any thread info", parent_id, child_id)
                            }
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /**
     * Will execute the provided ruby code using the eval_helper and the evaluation backend
     *
     * The EvaluationResult will contain the complete data and endpoints after the code is executed (cody might change them even in readonly mode)
     * If the call failed or a signaling occurs in the ruby code (raised or thrown), will return EvalError (in case of Signalling: EvalError::Signal)
     *
     * The read_only flag governs whether changes to dataelements and endpoints are applied to the instance (read_only=false) or not (read_only=true)
     *
     * Locks:
     *  - dynamic_data (shortly)
     *  - status (shortly)
     *  - EVALUATION_LOCK (for read_only = false)
     */
    pub fn execute_code(
        self: &Self,
        read_only: bool,
        code: &str,
        local: &str,
        connection_wrapper: &ConnectionWrapper,
        location: &str,
        call_result: Option<String>,
        call_headers: Option<HashMap<String, String>>,
    ) -> Result<eval_helper::EvaluationResult> {
        log::info!("Execute code got called with code: {code}");
        log::info!("With call result: {:?}", call_result);
        // We clone the dynamic data and status dto here which is expensive but allows us to not block the whole weel until the eval call returns
        let dynamic_data = self.context.lock().unwrap().clone();
        let status = self.status.lock().unwrap().to_dto();
        if read_only {
            let result = eval_helper::evaluate_expression(
                &dynamic_data,
                &self.opts,
                code,
                Some(status),
                local,
                connection_wrapper.additional(),
                call_result,
                call_headers,
                location,
            )?;
            Ok(result)
        } else {
            // Lock down all evaluation calls to prevent race condition
            let eval_lock = EVALUATION_LOCK.lock().unwrap();
            let result = eval_helper::evaluate_expression(
                &dynamic_data,
                &self.opts,
                code,
                Some(status),
                local,
                connection_wrapper.additional(),
                call_result,
                call_headers,
                location,
            )?;
            // Apply changes to instance
            if result.data.is_some() || result.endpoints.is_some() {
                let mut dynamic_data = self.context.lock().unwrap();

                if let Some(data) = &result.data {
                    dynamic_data.data = data.clone();
                };
                if let Some(endpoints) = &result.endpoints {
                    dynamic_data.endpoints = endpoints.clone();
                };
                drop(dynamic_data);
            };
            if let Some(new_status) = &result.changed_status {
                // TODO: We probably reuse the existing blocking queue instead of adding a new one right?
                let mut current_status = self.status.lock().unwrap();
                *current_status = Status {
                    id: new_status.id,
                    message: new_status.message.clone(),
                    // Here we reuse the blocking queue from the previous status as this is noy changed by the script and not serialized
                    // We need to obeserve whether taking the memory plays nice with the condvar but should be fine since we have exclusive access to the status
                    nudge: std::mem::take(&mut current_status.nudge),
                };
            }
            drop(eval_lock);
            Ok(result)
        }
    }

    fn evaluate_condition(self: Arc<Self>, condition: &str) -> Result<bool> {
        let connection_wrapper = ConnectionWrapper::new(self.clone(), None, None);
        let current_thread = thread::current();
        let thread_info_map = self.thread_information.lock().unwrap();
        let thread_info = thread_info_map.get(&current_thread.id()).unwrap().borrow();
        let thread_local = thread_info.local.clone();
        drop(thread_info);
        drop(thread_info_map);
        let result = eval_helper::test_condition(
            &self.context.lock().unwrap(),
            &self.opts,
            condition,
            &thread_local,
            connection_wrapper.additional(),
        );
        match result {
            Ok(cond) => Ok(cond),
            Err(err) => {
                self.clone().set_state(State::Stopping)?;
                log::error!(
                    "Encountered error when evaluating condition {condition}: {:?}",
                    err
                );
                match ConnectionWrapper::new(self.clone(), None, None)
                    .inform_syntax_error(err, Some(condition))
                {
                    Ok(_) => {}
                    Err(c_err) => log::error!(
                        "Error occured when evaluating condition, but informing CPEE failed: {:?}",
                        c_err
                    ),
                }
                Ok(false)
            }
        }
    }

    fn execute_lambda(self: &Arc<Self>, lambda: impl Fn() -> Result<()> + Sync) -> Result<()> {
        let result = lambda();
        match result {
            Ok(()) => Ok(()),
            Err(err) => {
                self.clone().set_state(State::Stopping)?;
                match ConnectionWrapper::new(self.clone(), None, None)
                    .inform_syntax_error(err, None)
                {
                    Ok(_) => Ok(()),
                    Err(c_err) => {
                        log::error!(
                            "Error occured when executing lambda, but informing CPEE failed: {:?}",
                            c_err
                        );
                        Err(c_err)
                    }
                }
            }
        }
    }

    /**
     * Checks whether the provided label is valid
     */
    fn position_test<'a>(self: Arc<Self>, activity_id: &'a str) -> Result<&'a str> {
        if activity_id.chars().all(char::is_alphanumeric) {
            Ok(activity_id)
        } else {
            self.clone().set_state(State::Stopping)?;
            Err(Error::GeneralError(format!(
                "position: {activity_id} not valid"
            )))
        }
    }

    /**
     * Checks whether the instance is in search mode w.r.t. the current position
     *
     */
    fn in_search_mode(&self, activity_id: Option<&str>) -> bool {
        let thread = thread::current();
        let thread_info_map = self.thread_information.lock().unwrap();
        // We unwrap here but we need to ensure that when the weel creates a thread, it registers the thread info!
        let mut thread_info = thread_info_map.get(&thread.id()).unwrap().borrow_mut();

        if !thread_info.in_search_mode {
            return false;
        }

        if let Some(activity_id) = activity_id {
            // Whether the current position was searched for
            let found_position = self
                .search_positions
                .lock()
                .unwrap()
                .contains_key(&activity_id.to_owned());
            if found_position {
                // We found the first position on this branch -> We do not need to search futher along this branch of execution
                thread_info.in_search_mode = false;
                thread_info.switched_to_execution = true;
                while let Some(parent) = thread_info.parent {
                    // Each parent thread has to have some thread information. In general all threads should, when they spawn via weel register and add their thread information
                    // Communicate to ancestor branches that in one of its childs a label was found and the search is done.
                    thread_info = thread_info_map.get(&parent).unwrap().borrow_mut();
                    thread_info.in_search_mode = false;
                    thread_info.switched_to_execution = true;
                }
                // checked earlier for membership, thus we can simply unwrap:
                self.search_positions
                    .lock()
                    .unwrap()
                    .get(activity_id)
                    .unwrap()
                    .detail
                    == "after"
            } else {
                true
            }
        } else {
            true
        }
    }

    /*
     * Locks:
     *  - `thread_information`
     *  - `ThreadInfo` of the current thread (within the thread_information)
     *  - `ThreadInfo` of the parent thread (within the thread_information)
     *  - positions, search_positions of the instance
     */
    fn weel_progress(
        self: &Arc<Self>,
        position: String,
        uuid: String,
        skip: bool,
    ) -> Result<Position> {
        // TODO: We could also guard the thread_info with a mutex again
        let mut ipc_node = json!({});
        let ipc = ipc_node
            .as_object_mut()
            .expect("Has to be object as just created");
        let current_thread = thread::current();
        let thread_info_map = self.thread_information.lock().unwrap();
        let (parent_thread_id, weel_position) = {
            // We need to limit the borrow of current_thread_info s.t. we can access the parents info afterwards -> scope it
            let mut current_thread_info = match thread_info_map.get(&current_thread.id()) {
                Some(x) => x.borrow_mut(),
                None => {
                    log::error!(
                        "Thread information for branch {:?} is empty",
                        current_thread.id()
                    );
                    panic!("Thread information not present!")
                }
            };
            if let Some(branch_position) = &current_thread_info.branch_position {
                self.positions
                    .lock()
                    .unwrap()
                    .retain(|x| *x != *branch_position);
                ipc.insert("unmark".to_owned(), json!([branch_position]));
            };
            let mut search_positions = self.search_positions.lock().unwrap();
            let search_position = search_positions.remove(&position);
            let passthrough = search_position.map(|pos| pos.handler_passthrough).flatten();
            let weel_position = if current_thread_info.switched_to_execution {
                current_thread_info.switched_to_execution = false;
                Position::new(
                    position.clone(),
                    uuid,
                    if skip {
                        "after".to_owned()
                    } else {
                        "at".to_owned()
                    },
                    passthrough,
                )
            } else {
                Position::new(
                    position.clone(),
                    uuid,
                    if skip {
                        "after".to_owned()
                    } else {
                        "at".to_owned()
                    },
                    None,
                )
            };
            if skip {
                ipc.insert("after".to_owned(), json!([weel_position]));
            } else {
                ipc.insert("at".to_owned(), json!([weel_position]));
            }

            if !search_positions.is_empty() {
                if !ipc.contains_key("unmark") {
                    ipc.insert("unmark".to_owned(), json!([]));
                }
                search_positions.iter().for_each(|(_, value)| {
                    ipc.get_mut("unmark")
                        .expect("we added unmark above")
                        .as_array_mut()
                        .expect("has to be array")
                        .push(json!(value));
                });
            }
            self.positions.lock().unwrap().push(weel_position.clone());
            current_thread_info.branch_position = Some(weel_position.clone());

            (current_thread_info.parent, weel_position)
        };
        if let Some(parent_thread_id) = parent_thread_id {
            let mut parent_thread_info = match thread_info_map.get(&parent_thread_id) {
                Some(x) => x.borrow_mut(),
                None => {
                    log::error!(
                        "Thread information for branch {:?} is empty",
                        current_thread.id()
                    );
                    panic!("Thread information not present!")
                }
            };
            if let Some(branch_position) = parent_thread_info.branch_position.take() {
                self.positions
                    .lock()
                    .unwrap()
                    .retain(|x| *x != branch_position);
                // TODO: Probably clone here right?
                if !ipc.contains_key("unmark") {
                    ipc.insert("unmark".to_owned(), json!([]));
                }
                ipc.get_mut("unmark")
                    .expect("has to be present")
                    .as_array_mut()
                    .expect("Has to be array")
                    .push(json!(branch_position));
            };
        };
        ConnectionWrapper::new(self.clone(), None, None).inform_position_change(Some(ipc_node))?;
        Ok(weel_position)
    }

    pub fn handle_error(self: &Arc<Self>, err: Error) {
        // TODO implement error handling that adheres to the handling in __weel_control_flow
        match self.clone().set_state(State::Stopping) {
            Ok(_) => {}
            Err(err) => {
                log::error!("Encountered error: {:?}", err);
                match ConnectionWrapper::new(self.clone(), None, None)
                    .inform_connectionwrapper_error(err)
                {
                    Ok(_) => {}
                    Err(err) => {
                        log::error!(
                            "Encountered error but informing CPEE of error failed: {:?}",
                            err
                        )
                    }
                };
            }
        };
        log::error!("Encountered error: {:?}", err);
        match ConnectionWrapper::new(self.clone(), None, None).inform_connectionwrapper_error(err) {
            Ok(_) => {}
            Err(err) => {
                log::error!(
                    "Encountered error but informing CPEE of error failed: {:?}",
                    err
                )
            }
        };
    }

    pub fn get_instance_meta_data(&self) -> InstanceMetaData {
        InstanceMetaData {
            cpee_base_url: self.opts.base_url().to_owned(),
            instance_id: self.opts.instance_id.clone(),
            instance_url: self.opts.instance_url(),
            instance_uuid: self.uuid().to_owned(),
            info: self.info().to_owned(),
            attributes: self.attributes.clone(),
        }
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

    /**
     * Sets the state of the weel
     *
     * Locks: state and potentially positions and status
     */
    fn set_state(self: Arc<Self>, new_state: State) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        if *state == new_state && !matches!(*state, State::Ready) {
            return Ok(());
        }

        if matches!(new_state, State::Running) {
            *self.positions.lock().unwrap() = Vec::new();
        }
        *state = new_state;

        if matches!(new_state, State::Stopping | State::Finishing) {
            let status = self.status.lock().unwrap();
            status.nudge.wake_all();
            recursive_continue(
                &self.thread_information.lock().unwrap(),
                &thread::current().id(),
            );
        }

        ConnectionWrapper::new(self.clone(), None, None).inform_state_change(new_state)?;
        Ok(())
    }
}

fn recursive_continue(
    thread_info_map: &MutexGuard<HashMap<ThreadId, RefCell<ThreadInfo>>>,
    thread_id: &ThreadId,
) {
    let thread_info = thread_info_map.get(thread_id).unwrap().borrow();
    thread_info
        .blocking_queue
        .lock()
        .unwrap()
        .enqueue(Signal::None);
    if let Some(branch_event) = &thread_info.branch_event {
        // TODO: Unsure whether we can borrow here -> Where relative to this thread will this branch event exist?
        branch_event.enqueue(());
    }
    for child_id in &thread_info.branches {
        recursive_continue(thread_info_map, child_id);
    }
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
    Signal(Signal),
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub enum Signal {
    Again,
    Salvage,
    Stop,
    Proceed,
    Skip,
    None,
    NoLongerNecessary,
    SkipManipulate,
    StopSkipManipulate,
    SyntaxError,
    Error,
    UpdateAgain,
}

impl Default for Signal {
    fn default() -> Self {
        Signal::None
    }
}

pub enum ActivityType {
    Call,
    Manipulate,
}

#[derive(PartialEq, Eq, Debug, Clone, Hash, Serialize)]
pub struct Position {
    position: String,
    uuid: String,
    detail: String,
    handler_passthrough: Option<String>,
}
impl Position {
    fn new(
        position: String,
        uuid: String,
        detail: String,
        handler_passthrough: Option<String>,
    ) -> Self {
        Self {
            position,
            uuid,
            detail,
            handler_passthrough,
        }
    }
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
            Error::Signal(_) => todo!(),
        }
    }
}

impl<T> From<PoisonError<T>> for Error {
    fn from(_: PoisonError<T>) -> Self {
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

#[cfg(test)]
mod test {
    use std::{fs, io::Write};

    use super::*;

    #[test]
    fn create_opts() {
        let mut file = match fs::File::create("./opts.yaml") {
            Ok(file) => file,
            Err(err) => {
                log::error!("Error creating the opts.yaml file: {:?}", err);
                panic!("Could not create opts.yaml file")
            }
        };
        let stat = StaticData {
            instance_id: 142,
            host: "localhost".to_owned(),
            cpee_base_url: "https://echo.bpm.in.tum.de/flow/engine".to_owned(),
            redis_url: None,
            redis_path: Some(format!("unix:///home/mangler/run/flow/redis.sock")),
            redis_db: 0,
            redis_workers: 1,
            executionhandlers: "/home/mangler/run/flow/executionhandlers".to_owned(),
            executionhandler: "rust".to_owned(),
            eval_language: "rust".to_owned(),
            eval_backend_exec_full: "http://localhost:9302/exec-full".to_owned(),
            eval_backend_structurize: "http://localhost:9302/structurize".to_owned(),
        };
        file.write("---\n".as_bytes()).unwrap();
        serde_yaml::to_writer(file, &stat).unwrap();
    }

    #[test]
    fn create_context() {
        let mut file = match fs::File::create("./context.json") {
            Ok(file) => file,
            Err(err) => {
                log::error!("Error creating the context.json file: {:?}", err);
                panic!("Could not create context.yaml file")
            }
        };
        let mut test_endpoints = HashMap::new();
        test_endpoints.insert(
            "bookAir".to_owned(),
            "http://gruppe.wst.univie.ac.at/~mangler/services/airline.php".to_owned(),
        );
        test_endpoints.insert(
            "timeout".to_owned(),
            "https-post://cpee.org/services/timeout.php".to_owned(),
        );

        let test_data = json!({
            "from": "Vienna",
            "to": "Prague",
            "persons": 3,
            "costs": 0,
            "flag": true
        });
        let dynamic = DynamicData {
            endpoints: test_endpoints,
            data: test_data,
        };
        serde_json::to_writer_pretty(file, &dynamic).unwrap();
    }
}
