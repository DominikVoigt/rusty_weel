use derive_more::From;
use serde::{Deserialize, Serialize};
use std::any::{Any, TypeId};
use std::cell::RefCell;
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

use crate::connection_wrapper::{self, ConnectionWrapper};
use crate::data_types::{
    BlockingQueue, DynamicData, HTTPParams, State, StaticData, Status, ThreadInfo,
};
use crate::dsl::DSL;
use crate::eval_helper::{self, EvalError, EvaluationResult};
use crate::redis_helper::{RedisHelper, Topic};

static EVALUATION_LOCK: Mutex<()> = Mutex::new(());

pub struct Weel {
    pub static_data: StaticData,
    pub dynamic_data: Mutex<DynamicData>,
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
            prepare_code,
            update_code,
            rescue_code,
            finalize_code,
            Some(parameters),
            Some(endpoint_name),
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
        model: impl FnOnce() -> Result<()> + Send + 'static,
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

    pub fn stop(&self) -> Result<()> {
        {
            let mut state = self.state.lock().expect("Could not lock state mutex");
            match *state {
                State::Ready => *state = State::Stopped,
                State::Running => {
                    // TODO: Where will this be set to stopped?
                    *state = State::Stopping;
                    // Wait for instance to stop
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

    /**
     * Allows veto-voting on arbitrary topics and will return true if no veto was cast (otherwise false)
     *
     * Vote of controller
     *
     * Locks:
     *  - open_votes
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
    fn vote_sync_before(
        &self,
        connection_wrapper: &ConnectionWrapper,
        parameters: Option<HashMap<String, String>>,
    ) -> Result<bool> {
        let mut content = connection_wrapper.construct_basic_content()?;
        content.insert("parameters".to_owned(), serde_json::to_string(&parameters)?);
        self.vote("activity/syncing_before", content)
    }

    /**
     * Registers a callback
     */
    pub fn register_callback(
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

    /**
     * Removes a registered callback
     */
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
        label: &str,
        activity_type: ActivityType,
        prepare_code: Option<&str>,
        update_code: Option<&str>,
        rescue_code: Option<&str>,
        finalize_code: Option<&str>,
        parameters: Option<HTTPParams>,
        endpoint_name: Option<&str>,
    ) -> Result<()> {
        let position = self.position_test(label)?;
        let in_search_mode = self.in_search_mode(Some(label));
        if in_search_mode {
            return Ok(());
        }
        let connection_wrapper =
            ConnectionWrapper::new(self.clone(), Some(position.to_owned()), None);
        let connection_wrapper_mutex = Arc::new(Mutex::new(connection_wrapper));

        let mut weel_position = None;
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

            thread_info.blocking_queue = Arc::new(BlockingQueue::new());
            let mut connection_wrapper = connection_wrapper_mutex.lock().unwrap();
            connection_wrapper.handler_continue = Some(thread_info.blocking_queue.clone());

            let parent = thread_info.parent.clone();

            // Register position/label of this thread in the branch traces of the parent thread
            if parent.is_some() && thread_info.branch_traces_id.is_some() {
                let branch_trace_id = thread_info.branch_traces_id.as_ref().unwrap();
                let mut parent_thread_info =
                    thread_info_map.get(&parent.unwrap()).unwrap().borrow_mut();
                let traces = parent_thread_info.branch_traces.get_mut(branch_trace_id);
                match traces {
                    Some(traces) => traces.push(position.to_owned()),
                    None => {
                        parent_thread_info
                            .branch_traces
                            .insert(branch_trace_id.to_owned(), Vec::new());
                    }
                }
            };

            weel_position = Some(self.weel_progress(
                position.to_owned(),
                connection_wrapper.handler_activity_uuid.clone(),
                false,
            )?);
            // Local information should not change outside of this thread TODO: add this to actual thread_local_storage
            let local = thread_info.local.clone();
            // Drop the thread_info here already as for a manipulate we do not need it at all and a call we need to acquire the lock every 'again loop anyway
            drop(thread_info);
            drop(thread_info_map);
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
                            connection_wrapper.activity_manipulate_handle(label);
                            connection_wrapper.inform_activity_manipulate()?;
                            let result = match self.clone().execute_code(
                                false,
                                finalize_code,
                                local,
                                &connection_wrapper,
                                &format!("Activity {}", position),
                            ) {
                                Ok(res) => res,
                                Err(err) => break 'raise Err(err),
                            };
                            connection_wrapper.inform_manipulate_change(result)?;
                        }
                        None => (),
                    };
                    connection_wrapper.inform_activity_done()?;
                    weel_position.as_mut().unwrap().detail = Mark::After;
                    let mut ipc = HashMap::new();
                    ipc.insert(
                        "after".to_owned(),
                        serde_json::to_string(weel_position.as_ref().unwrap())?,
                    );
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
                        let parameters = connection_wrapper.prepare(
                            prepare_code,
                            thread_info.local.clone(),
                            &vec![endpoint_name.unwrap()],
                            parameters.as_ref().expect(
                                "The activity type call requires parameters to be provided",
                            ),
                        )?;

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
                                .as_ref()
                                .unwrap()
                                .handler_passthrough
                                .as_ref()
                                .map(|x| x.as_str()),
                            parameters,
                        )?;
                        let connection_wrapper = connection_wrapper_mutex.lock().unwrap();
                        weel_position.as_mut().unwrap().handler_passthrough =
                            connection_wrapper.handler_passthrough.clone();
                        if let Some(position) = &weel_position.as_ref().unwrap().handler_passthrough
                        {
                            let connection_wrapper = ConnectionWrapper::new(
                                self.clone(),
                                // Do not need this data for the inform:
                                None,
                                None,
                            );
                            let mut content = HashMap::new();
                            content.insert(
                                "wait".to_owned(),
                                serde_json::to_string(weel_position.as_ref().unwrap())?,
                            );
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
                                // TODO: issue this will block the whole thread -> We need to have no instance level mutexed locked at this point or the whole instance will block!
                                wait_result = Some(thread_queue.dequeue());
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
                                weel_position.as_mut().unwrap().handler_passthrough =
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

                            let mut code_type = "";
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

                            let signaled_again = false;
                            connection_wrapper.inform_activity_manipulate()?;
                            if let Some(code) = code {
                                // TODO: I do not get this line in the original with the catch Signal::Again and the the Signal::Proceed
                                let signaled_again = false;
                                let result = match self.execute_code(
                                    false,
                                    code,
                                    local,
                                    &connection_wrapper,
                                    &format!("Activity {} {}", position, code_type),
                                ) // TODO: Even in signal case we need the eval result
                                {
                                    Ok(res) => res,
                                    Err(err) => match err {
                                        Error::Signal(signal) => {
                                            match signal {
                                                Signal::Again => {
                                                    signaled_again = true;
                                                },
                                                x => {todo!()}
                                            }
                                        },
                                        err => break 'raise Err(err)
                                    }
                                };
                                 
                                connection_wrapper.inform_manipulate_change(result)?;

                                // TODO: What would this ma.nil? result in rust?
                            }
                            if !signaled_update_again {
                                // If wait result was not UpdateAgain -> Break out, otherwise continue inner loop
                                break 'inner;
                            }
                        }
                        let connection_wrapper = connection_wrapper_mutex.lock().unwrap();
                        if connection_wrapper.activity_passthrough_value().is_none() {
                            connection_wrapper.inform_activity_done()?;
                            weel_position.as_mut().unwrap().handler_passthrough = None;
                            weel_position.as_mut().unwrap().detail = Mark::After;
                            let mut content = HashMap::new();
                            content.insert(
                                "after".to_owned(),
                                serde_json::to_string(weel_position.as_ref().unwrap())?,
                            );
                            ConnectionWrapper::new(self.clone(), None, None)
                                .inform_position_change(Some(content))?;
                        }
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
                            *self.state.lock().unwrap() = State::Stopping;
                            weel_position.detail = Mark::Unmark;
                        }
                    }
                    Signal::NoLongerNecessary => {
                        connection_wrapper.inform_activity_done();
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
                        weel_position.detail = Mark::Unmark;
                        let mut ipc = HashMap::new();
                        ipc.insert(
                            "unmark".to_owned(),
                            serde_json::to_string(&vec![weel_position])?,
                        );
                        ConnectionWrapper::new(self.clone(), None, None)
                            .inform_position_change(Some(ipc))?;
                    }
                    Signal::Stop | Signal::StopSkipManipulate => {
                        *self.state.lock().unwrap() = State::Stopping;
                    }
                    Signal::Skip => {
                        log::info!("Received skip signal. Do nothing")
                    }
                    Signal::Salvage => {}
                    x => {
                        log::error!("Received unexpected signal: {:?}", x);
                    }
                },
                Error::EvalError(eval_error) => match eval_error {
                    EvalError::GeneralEvalError(_) => todo!(),
                    EvalError::SyntaxError(_) => todo!(),
                    EvalError::Signal(_) => todo!(),
                    EvalError::RuntimeError(_) => todo!(),
                },
                err => {
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
        };
        Ok(())
    }

    /**
     * Will execute the provided ruby code using the eval_helper and the evaluation backend
     *
     * The EvaluationResult will contain the complete data and endpoints after the code is executed (cody might change them even in readonly mode)
     * If the call failed, it will contain a signal and signal_text
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
        local: String,
        connection_wrapper: &ConnectionWrapper,
        location: &str,
    ) -> Result<eval_helper::EvaluationResult> {
        // We clone the dynamic data and status dto here which is expensive but allows us to not block the whole weel until the eval call returns
        let dynamic_data = self.dynamic_data.lock().unwrap().clone();
        let status = self.status.lock().unwrap().to_dto();
        if read_only {
            let result = eval_helper::evaluate_expression(
                &dynamic_data,
                &self.static_data,
                code,
                Some(status),
                local,
                connection_wrapper.additional(),
                None,
                None,
            )?;
            Ok(result)
        } else {
            // Lock down all evaluation calls to prevent race condition
            let eval_lock = EVALUATION_LOCK.lock().unwrap();
            let result = eval_helper::evaluate_expression(
                &dynamic_data,
                &self.static_data,
                code,
                Some(status),
                local,
                connection_wrapper.additional(),
                None,
                None,
                location,
            )?;
            // Apply changes to instance
            let mut dynamic_data = self.dynamic_data.lock().unwrap();
            dynamic_data.data = result.data.clone();
            dynamic_data.endpoints = result.endpoints.clone();
            drop(dynamic_data);
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

    /**
     * Checks whether the provided label is valid
     */
    fn position_test<'a>(&self, label: &'a str) -> Result<&'a str> {
        if label.chars().all(char::is_alphanumeric) {
            Ok(label)
        } else {
            *self.state.lock().unwrap() = State::Stopping;
            Err(Error::GeneralError(format!("position: {label} not valid")))
        }
    }

    /**
     * Checks whether the instance is in search mode w.r.t. the current position
     *
     */
    fn in_search_mode(&self, label: Option<&str>) -> bool {
        let thread = thread::current();
        let thread_info_map = self.thread_information.lock().unwrap();
        // We unwrap here but we need to ensure that when the weel creates a thread, it registers the thread info!
        let mut thread_info = thread_info_map.get(&thread.id()).unwrap().borrow_mut();

        if !thread_info.in_search_mode {
            return false;
        }

        if let Some(label) = label {
            // Whether the current position was searched for
            let found_position = self
                .search_positions
                .lock()
                .unwrap()
                .contains_key(&label.to_owned());
            if found_position {
                // We found the first position on this branch -> We do not need to search futher along this branch of execution
                thread_info.in_search_mode = false;
                thread_info.branch_search_now = true;
                while let Some(parent) = thread_info.parent {
                    // Each parent thread has to have some thread information. In general all threads should, when they spawn via weel register and add their thread information
                    // Communicate to ancestor branches that in one of its childs a label was found and the search is done.
                    thread_info = thread_info_map.get(&parent).unwrap().borrow_mut();
                    thread_info.in_search_mode = false;
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
        let mut ipc = HashMap::new();
        let current_thread = thread::current();
        let thread_info_map = self.thread_information.lock().unwrap();

        let (parent_thread_id, weel_position) = {
            // We need to limit the borrow of current_thread_info s.t. we can access the parents infor afterwards -> scope it
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
                let mut set = HashSet::new();
                set.insert(branch_position.clone());
                ipc.insert("unmark".to_owned(), set);
            };
            let mut search_positions = self.search_positions.lock().unwrap();
            let search_position = search_positions.remove(&position);
            let passthrough = search_position.map(|pos| pos.handler_passthrough).flatten();
            let weel_position = if current_thread_info.branch_search_now {
                current_thread_info.branch_search_now = false;
                Position::new(
                    position.clone(),
                    uuid,
                    if skip { Mark::After } else { Mark::At },
                    passthrough,
                )
            } else {
                Position::new(
                    position.clone(),
                    uuid,
                    if skip { Mark::After } else { Mark::At },
                    None,
                )
            };

            let mut set = HashSet::new();
            if skip {
                set.insert(weel_position.clone());
                ipc.insert("after".to_owned(), set);
            } else {
                set.insert(weel_position.clone());
                ipc.insert("at".to_owned(), set);
            }

            if !search_positions.is_empty() {
                ipc.insert("unmark".to_owned(), HashSet::new());
            }
            search_positions.iter().for_each(|(_, value)| {
                ipc.get_mut("unmark")
                    .expect("we added unmark above")
                    .insert(value.clone());
            });
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
                let mut set = HashSet::new();
                set.insert(branch_position);
                ipc.insert("unmark".to_owned(), set);
            };
        };
        let ipc: HashMap<String, String> = ipc
            .into_iter()
            .map(|(k, v)| (k, serde_json::to_string(&v).unwrap()))
            .collect();
        ConnectionWrapper::new(self.clone(), None, None).inform_position_change(Some(ipc))?;
        Ok(weel_position)
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
    Signal(Signal, EvaluationResult),
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
    UpdateAgain
}

pub enum ActivityType {
    Call,
    Manipulate,
}

#[derive(PartialEq, Eq, Debug, Clone, Hash, Serialize)]
pub struct Position {
    position: String,
    uuid: String,
    detail: Mark,
    handler_passthrough: Option<String>,
}
impl Position {
    fn new(
        position: String,
        uuid: String,
        detail: Mark,
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

#[derive(PartialEq, Eq, Debug, Clone, Hash, Serialize)]
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
            Error::Signal(_, _) => todo!(),
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
