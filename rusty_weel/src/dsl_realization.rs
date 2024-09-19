use derive_more::From;
use serde::{Deserialize, Serialize};
use std::any::TypeId;
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
use crate::eval_helper::{self, EvalError};
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
        let search_mode = self.in_search_mode(Some(label));
        if search_mode {
            return Ok(());
        }

        let connection_wrapper =
            ConnectionWrapper::new(self.clone(), Some(position.to_owned()), None);
        let connection_wrapper_mutex = Arc::new(Mutex::new(connection_wrapper));

        let mut connection_wrapper = connection_wrapper_mutex.lock().unwrap();
        let state = self.state.lock().unwrap();
        let invalid_state = match *state {
            State::Running => false,
            _ => true,
        };

        let result: Result<()> = 'raise: {
            let current_thread = thread::current().id();
            let thread_info_map = self.thread_information.lock().unwrap();
            // Unwrap as we have precondition that thread info is available on spawning
            let mut thread_info = thread_info_map.get(&current_thread).unwrap().borrow_mut();
            {
                // Skip execution if the branch was set to no longer necessary or the instance is supposed to stop
                let no_longer_necessary = thread_info.no_longer_necessary;

                if no_longer_necessary {
                    break 'raise Err(Signal::NoLongerNecessary.into());
                }
                if invalid_state {
                    // Consistent with further below
                    break 'raise Err(Signal::Skip.into());
                }
            }
            thread_info.blocking_queue = Arc::new(BlockingQueue::new());
            connection_wrapper.handler_continue = Some(thread_info.blocking_queue.clone());
            let parent = thread_info.parent.clone();
            let branch_traces_id = thread_info.branch_traces_id.clone();
            let local = thread_info.local.clone();
            if parent.is_some() && branch_traces_id.is_some() {
                let branch_trace_id = branch_traces_id.as_ref().unwrap();
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

            let mut weel_position = self.weel_progress(
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
                            connection_wrapper.activity_manipulate_handle(label);
                            connection_wrapper.inform_activity_manipulate()?;
                            let result = self.clone().execute_code(
                                false,
                                finalize_code,
                                local,
                                &connection_wrapper,
                            )?;
                            connection_wrapper.inform_manipulate_change(result)?;
                        }
                        None => (),
                    };
                    connection_wrapper.inform_activity_done()?;
                    weel_position.detail = Mark::After;
                    let mut ipc = HashMap::new();
                    ipc.insert("after".to_owned(), serde_json::to_string(&weel_position)?);
                    ConnectionWrapper::new(self.clone(), None, None)
                        .inform_position_change(Some(ipc))?;
                }
                ActivityType::Call => {
                    drop(thread_info);
                    drop(thread_info_map);
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
                            parameters.expect("The activity type call requires parameters to be provided"),
                        )?;
                        // Drop info before we enter blocking vote_sync_before
                        drop(thread_info);
                        drop(thread_info_map);

                        let state_stopping_or_finishing = matches!(
                            *self.state.lock().unwrap(),
                            State::Stopping | State::Finishing
                        );
                        // TODO: Maybe drop the thread info here too? This call will block all
                        if !self.vote_sync_before(&connection_wrapper, None)? {
                            break 'raise Err(Signal::Stop.into());
                        } else if state_stopping_or_finishing {
                            break 'raise Err(Signal::Skip.into());
                        }

                        // This executes the actual call
                        ConnectionWrapper::activity_handle(
                            &connection_wrapper_mutex,
                            weel_position
                                .handler_passthrough
                                .as_ref()
                                .map(|x| x.as_str()),
                            parameters.as_ref().unwrap(),
                        )?;
                        weel_position.handler_passthrough =
                            connection_wrapper.handler_passthrough.clone();
                        if let Some(position) = &weel_position.handler_passthrough {
                            let connection_wrapper = ConnectionWrapper::new(
                                self.clone(),
                                Some(position.to_owned()),
                                Some(thread_info.blocking_queue.clone()),
                            );
                            let mut content = HashMap::new();
                            content
                                .insert("wait".to_owned(), serde_json::to_string(&weel_position)?);
                            connection_wrapper.inform_position_change(Some(content))?;
                        };
                        drop(thread_info);
                        drop(thread_info_map);
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

                            let thread_sleep =
                                !state_stopping_or_finishing && !thread_info.no_longer_necessary;
                            let mut wait_result = None;
                            let thread_queue = thread_info.blocking_queue.clone();
                            drop(thread_info);
                            drop(thread_info_map);
                            drop(connection_wrapper);
                            if thread_sleep {
                                // TODO: issue this will block the whole thread -> We need to have no mutexed locked at this point or the whole instance will block!
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

                            if wait_result
                                .as_ref()
                                .map(|res| matches!(res, Signal::Again))
                                .unwrap_or(false)
                                && connection_wrapper
                                    .handler_return_value
                                    .clone()
                                    .map(|x| x.is_empty())
                                    .unwrap_or(true)
                            {
                                continue;
                            }

                            let code = if wait_result
                                .as_ref()
                                .map(|res| matches!(res, Signal::Again))
                                .unwrap_or(false)
                            {
                                update_code
                            } else if wait_result
                                .as_ref()
                                .map(|res| matches!(res, Signal::Salvage))
                                .unwrap_or(false)
                            {
                                if rescue_code.is_some() {
                                    rescue_code
                                } else {
                                    // We return actual errors
                                    break 'raise Err(Error::GeneralError(format!(
                                        "Service returned status code {:?}",
                                        connection_wrapper.handler_return_status
                                    )));
                                }
                            } else {
                                finalize_code
                            };

                            connection_wrapper.inform_activity_manipulate()?;
                            if let Some(code) = code {
                                // TODO: I do not get this line in the original with the catch Signal::Again and the the Signal::Proceed
                                let evaluation_result = self.execute_code(
                                    false,
                                    code,
                                    thread_info.local.clone(),
                                    &connection_wrapper,
                                )?;
                                // TODO: We need to handle a signal -> Signal Again, Signal Error
                                connection_wrapper.inform_manipulate_change(evaluation_result)?;
                                // TODO: What would this ma.nil? result in rust?
                                let cond = false;
                                if cond {
                                    continue 'again; // ->jumps to next execution of outer loop
                                }
                                if wait_result
                                    .as_ref()
                                    .map(|res: &Signal| !matches!(res, Signal::Again))
                                    .unwrap_or(true)
                                {
                                    break 'inner;
                                }
                            }
                        }
                        let connection_wrapper = connection_wrapper_mutex.lock().unwrap();
                        if connection_wrapper.activity_passthrough_value().is_none() {
                            connection_wrapper.inform_activity_done()?;
                            weel_position.handler_passthrough = None;
                            weel_position.detail = Mark::After;
                            let mut content = HashMap::new();
                            content
                                .insert("after".to_owned(), serde_json::to_string(&weel_position)?);
                            ConnectionWrapper::new(self.clone(), None, None)
                                .inform_position_change(Some(content))?;
                        }
                    }
                }
            };
            // -> Feels very wrong, to do this but in this case the code treats this as an error, so will we
            return Err(Signal::Proceed.into());
        };

        let connection_wrapper = connection_wrapper_mutex.lock().unwrap();

        if let Err(error) = result {
            match error {
                Error::Signal(signal) => match signal {
                    Signal::Proceed => {
                        let state_stopping_or_finishing = matches!(
                            *self.state.lock().unwrap(),
                            State::Stopping | State::Finishing
                        );

                        if !state_stopping_or_finishing
                            && !self.vote_sync_after(&connection_wrapper)?
                        {}
                    }
                    Signal::NoLongerNecessary => todo!(),
                    Signal::Stop => todo!(),
                    Signal::Skip => todo!(),
                    Signal::Salvage => todo!(),
                    x => {
                        log::error!("Received unexpected signal: {:?}", x);
                    }
                },
                err => {
                    log::error!("Encountered error: {:?}", err);
                    match ConnectionWrapper::new(self.clone(), Some(position.to_owned()), None)
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
            }
        };
        Ok(())
    }

    /**
     * Will execute the provided ruby code using the eval_helper and the evaluation backend
     *
     * The EvaluationResult will contain the complete data and endpoints after the code is executed (cody might change them even in readonly mode)
     *
     * The read_only flag governs whether changes to dataelements and endpoints are applied to the instance (read_only=false) or not (read_only=true)
     */
    pub fn execute_code(
        self: &Arc<Self>,
        read_only: bool,
        code: &str,
        local: String,
        connection_wrapper: &ConnectionWrapper,
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
                Some(local),
                connection_wrapper.additional(),
                None,
                None,
            )?;
            Ok(result)
        } else {
            let mut dynamic_data: DynamicData = dynamic_data;
            // Lock down all evaluation calls to prevent race condition
            let eval_lock = EVALUATION_LOCK.lock().unwrap();
            let result = eval_helper::evaluate_expression(
                &dynamic_data,
                &self.static_data,
                code,
                Some(status),
                Some(local),
                connection_wrapper.additional(),
                None,
                None,
            )?;
            // Apply changes to instance
            dynamic_data.data = result.data.clone();
            dynamic_data.endpoints = result.endpoints.clone();
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
                    .retain(|x| x != branch_position);
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
    Signal(Signal),
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Signal {
    Again,
    Salvage,
    Stop,
    Proceed,
    Skip,
    None,
    NoLongerNecessary,
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
            Error::Signal(_) => todo!(),
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
