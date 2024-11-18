use derive_more::From;

use once_map::OnceMap;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fmt::Debug;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, ThreadId};
use std::time::SystemTime;

use rand::distributions::Alphanumeric;
use rand::Rng;
use reqwest::header::ToStrError;

use crate::connection_wrapper::ConnectionWrapper;
use crate::data_types::{
    BlockingQueue, CancelCondition, ChooseVariant, Context, HTTPParams, InstanceMetaData, Opts,
    State, Status, ThreadInfo,
};
use crate::dsl::DSL;
use crate::eval_helper::{self, EvalError};
use crate::redis_helper::{RedisHelper, Topic};

static EVALUATION_LOCK: Mutex<()> = Mutex::new(());
static PRECON_THREAD_INFO: &str = 
"The thread information was not available even though the thread was already running!\nThe thread information should be created for every thread when it is spawned (either in main or parallel branch)";

pub struct Weel {
    pub opts: Opts,
    pub context: Mutex<Context>,
    pub state: Mutex<State>,
    pub status: Mutex<Status>,
    // Positions are shared witin the program
    pub positions: Mutex<Vec<Arc<Position>>>,
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
    // We use once map here: For each critical section the mutex should be created only once!
    // Locking a critical secition should not block other threads from entering other critical sections -> We cannot lock the hashmap with either a mutex or rwlock
    pub critical_section_mutexes: OnceMap<String, Box<Mutex<()>>>,
    pub stop_signal_receiver: Mutex<Option<Receiver<()>>>,
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

    fn manipulate(
        self: Arc<Self>,
        id: &str,
        label: Option<&'static str>,
        code: Option<&str>,
    ) -> Result<()> {
        self.weel_activity(
            id,
            ActivityType::Manipulate,
            None,
            None,
            None,
            code,
            label.map(|e: &'static str| HTTPParams {
                label: e,
                method: http_helper::Method::GET,
                arguments: json!(""),
            }),
            None,
        )
    }

    fn parallel_do(
        self: Arc<Self>,
        wait: Option<usize>,
        cancel: CancelCondition,
        lambda: &(dyn Fn() -> Result<()> + Sync + Send),
    ) -> Result<()> {
        let current_thread_id = thread::current().id();
        let thread_map = self.thread_information.lock().unwrap();
        let mut thread_info = thread_map
            .get(&current_thread_id)
            .expect(PRECON_THREAD_INFO)
            .borrow_mut();

        if self.should_skip(&thread_info) {
            return Ok(());
        }

        thread_info.branches = Vec::new();
        thread_info.branch_traces = HashMap::new();
        // thread_info.branch_finished_count = 0;

        let barrier_start = Arc::new(BlockingQueue::new());
        thread_info.branch_barrier_start = Some(barrier_start.clone());
        let (branch_event_tx, branch_event_rx) = mpsc::channel::<()>();
        thread_info.branch_event_sender = Some(branch_event_tx);
        drop(thread_info);
        drop(thread_map);
        // Startup the branches
        self.execute_lambda(lambda)?;

        let thread_map = self.thread_information.lock().unwrap();
        let mut thread_info = thread_map
            .get(&current_thread_id)
            .expect(PRECON_THREAD_INFO)
            .borrow_mut();

        thread_info.branch_wait_count = 0;
        // If wait is not set, wait for all branches to terminate
        // Now the branches are setup, the list should be static.
        let branches = thread_info.branches.clone();
        let spawned_branches: usize = branches.len();
        thread_info.branch_wait_threshold = wait.unwrap_or(spawned_branches);
        thread_info.parallel_wait_condition = cancel;

        let connection_wrapper = ConnectionWrapper::new(self.clone(), None, None);
        connection_wrapper.split_branches(current_thread_id, Some(&thread_info.branch_traces))?;

        // Now start all branches
        for thread in &branches {
            if !thread_info.in_search_mode {
                thread_map.get(thread).unwrap().borrow_mut().in_search_mode = false;
            }
            barrier_start.enqueue(());
        }

        drop(thread_info);
        drop(thread_map);
        // Wait for the "final" thread to fulfill the wait condition (wait_threshold = wait_count)
        if !(self.terminating() || spawned_branches == 0) {
            branch_event_rx.recv().unwrap();
        }
        let thread_map = self.thread_information.lock().unwrap();
        let mut thread_info = thread_map
            .get(&current_thread_id)
            .expect(PRECON_THREAD_INFO)
            .borrow_mut();
        thread_info.branch_event_sender = None;

        connection_wrapper.join_branches(current_thread_id, Some(&thread_info.branch_traces))?;
        drop(thread_info);
        // TODO: in original code we did not check on no_longer necessary here, should we or not?
        if !self.terminating() {
            for child in &branches {
                let mut child_info: std::cell::RefMut<'_, ThreadInfo> =
                    thread_map.get(child).unwrap().borrow_mut();
                child_info.no_longer_necessary = true;
                drop(child_info);
                recursive_continue(&thread_map, child);
            }
            drop(thread_map);
            for child_thread in branches {
                // need to acquire in loop, as we need to release lock to allow children to finish
                let thread_map = self.thread_information.lock().unwrap();
                let mut child_info: std::cell::RefMut<'_, ThreadInfo> =
                    thread_map.get(&child_thread).unwrap().borrow_mut();
                child_info.no_longer_necessary = true;
                drop(child_info);
                drop(thread_map);

                self.recursive_join(child_thread)?;
            }
        }

        Ok(())
    }

    fn parallel_branch(
        self: Arc<Self>,
        lambda: Arc<dyn Fn() -> Result<()> + Sync + Send>,
    ) -> Result<()> {
        if self.clone().should_skip_locking() {
            return Ok(());
        };

        let parent_thread = thread::current().id();

        // We cannot let this run on further as we need to get a handle on the thread_info_map, if we do not do this here,
        // then we would need to ensure in parallel gateway that the thread info is dropped again before waiting for ready
        let (setup_done_tx, setup_done_rx) = mpsc::channel::<ThreadId>();
        let weel = self.clone();
        let handle = thread::spawn(move || -> Result<()> {
            let mut thread_info_map = weel.thread_information.lock().unwrap();
            // Unwrap as we have precondition that thread info is available on spawning
            let mut parent_thread_info = thread_info_map
                .get(&parent_thread)
                .expect(PRECON_THREAD_INFO)
                .borrow_mut();
            parent_thread_info.branches.push(thread::current().id());
            // Get a sender to signal wait end

            let branch_event_sender = parent_thread_info
                .branch_event_sender
                .as_ref()
                .expect("Branch event channel has to be established by parallel gateway")
                .clone();

            let mut thread_info = ThreadInfo::default();
            thread_info.in_search_mode = !weel.search_positions.lock().unwrap().is_empty();
            thread_info.parent_thread = Some(parent_thread);
            thread_info.local = Some(weel.context.lock().unwrap().data.clone());
            let (terminate_tx, terminate_rx) = mpsc::channel();
            thread_info.terminated_signal_sender = Some(terminate_tx);
            thread_info.terminated_signal_receiver = Some(terminate_rx);
            let branch_barrier_start = parent_thread_info
                .branch_barrier_start
                .clone()
                .expect("should be there. Initialized by parallel_exec");

            if !parent_thread_info.alternative_executed.is_empty() {
                thread_info.alternative_executed =
                    vec![*parent_thread_info.alternative_executed.last().unwrap()];
                thread_info.alternative_mode =
                    vec![*parent_thread_info.alternative_mode.last().unwrap()];
            };

            drop(parent_thread_info);
            thread_info_map.insert(thread::current().id(), RefCell::new(thread_info));
            drop(thread_info_map);
            // Notify parallel gateway that the thread has completed setup
            setup_done_tx.send(thread::current().id()).unwrap();
            if !weel.terminating() {
                // wait for run signal from parallel gateway
                branch_barrier_start.dequeue();
            }

            if !weel.should_skip_locking() {
                weel.execute_lambda(lambda.as_ref())?;
            }

            // Now the parallel branch terminates
            let thread_info_map = weel.thread_information.lock().unwrap();
            let mut parent_thread_info = thread_info_map
                .get(&parent_thread)
                .expect(PRECON_THREAD_INFO)
                .borrow_mut();
            let thread_info = thread_info_map
                .get(&thread::current().id())
                .expect(PRECON_THREAD_INFO)
                .borrow_mut();
            parent_thread_info.branch_finished_count += 1;
            thread_info
                .terminated_signal_sender
                .as_ref()
                .expect(PRECON_THREAD_INFO)
                .send(())
                .unwrap();
            drop(thread_info);
            // Signal that this thread has terminated

            if parent_thread_info.parallel_wait_condition == CancelCondition::Last {
                let reached_wait_threshold = parent_thread_info.branch_finished_count
                    == parent_thread_info.branch_wait_threshold;
                if reached_wait_threshold
                    && !matches!(
                        *weel.state.lock().unwrap(),
                        State::Stopping | State::Finishing
                    )
                {
                    for child in &parent_thread_info.branches {
                        let mut child_info = thread_info_map.get(child).unwrap().borrow_mut();
                        match child_info
                            .terminated_signal_receiver
                            .as_ref()
                            .expect(PRECON_THREAD_INFO)
                            .try_recv()
                        {
                            Ok(()) => {
                                // Branch already done, nothing to do here
                            }
                            Err(err) => {
                                match err {
                                    mpsc::TryRecvError::Empty => {
                                        // We did not hear back yet, this means that the branch did not finish yet, so we will cancel it
                                        child_info.no_longer_necessary = true;
                                        drop(child_info);
                                        recursive_continue(&thread_info_map, child);
                                    }
                                    mpsc::TryRecvError::Disconnected => {
                                        log::error!("Tried to check whether the branch was terminated, but the sender became disconnected")
                                    }
                                }
                            }
                        }
                    }
                }
            }
            // Case when the wait count is the number of branches?
            if parent_thread_info.branch_finished_count == parent_thread_info.branches.len()
                && !matches!(
                    *weel.state.lock().unwrap(),
                    State::Stopping | State::Finishing
                )
            {
                match branch_event_sender.send(()) {
                    Ok(()) => {}
                    Err(err) => {
                        log::error!("Encountered error when sending branch_event: {:?}", err)
                    }
                }
            }

            if !matches!(
                *weel.state.lock().unwrap(),
                State::Stopping | State::Stopped | State::Finishing
            ) {
                let mut thread_info = thread_info_map
                    .get(&thread::current().id())
                    .unwrap()
                    .borrow_mut();
                if let Some(position) = thread_info.branch_position.take() {
                    weel.positions
                        .lock()
                        .unwrap()
                        .retain(|e| !Arc::ptr_eq(e, &position));
                    let ipc = json!({
                        "unmark": [*position]
                    });
                    drop(thread_info);
                    ConnectionWrapper::new(weel.clone(), None, None)
                        .inform_position_change(Some(ipc))?;
                }
            }
            Ok(())
        });
        let child_thread_id = setup_done_rx.recv().unwrap();
        self.thread_information
            .lock()
            .unwrap()
            .get(&child_thread_id)
            .unwrap()
            .borrow_mut()
            .join_handle = Some(handle);
        Ok(())
    }

    fn choose(
        self: Arc<Self>,
        variant: ChooseVariant,
        lambda: &(dyn Fn() -> Result<()> + Sync),
    ) -> Result<()> {
        let current_thread = thread::current().id();
        let thread_info_map = self.thread_information.lock().unwrap();
        // Unwrap as we have precondition that thread info is available on spawning
        let mut thread_info = thread_info_map
            .get(&current_thread)
            .expect(PRECON_THREAD_INFO)
            .borrow_mut();

        if self.should_skip(&thread_info) {
            return Ok(());
        }

        thread_info.alternative_executed.push(false);
        thread_info.alternative_mode.push(variant);

        let connection_wrapper = ConnectionWrapper::new(self.clone(), None, None);
        drop(thread_info);
        drop(thread_info_map);
        connection_wrapper.split_branches(thread::current().id(), None)?;
        self.clone().execute_lambda(lambda)?;
        connection_wrapper.join_branches(thread::current().id(), None)?;
        let current_thread = thread::current().id();
        let thread_info_map = self.thread_information.lock().unwrap();
        let mut thread_info = thread_info_map
            .get(&current_thread)
            .expect(PRECON_THREAD_INFO)
            .borrow_mut();

        thread_info.alternative_executed.pop();
        thread_info.alternative_mode.pop();
        Ok(())
    }

    /**
     * This is one of the branches
     *
     * TODO: Correctness currently requiers invariant: Branches need to be executed/evaluated sequentially! -> Otherwise dropping and picking up the thread info mutex is problematic!
     *       -> We can fix this by introducing a lock on the level of the gate -> locking here
     *       -> Holding the thread map longer would be bad as it is a global lock
     */
    fn alternative(
        self: Arc<Self>,
        condition: &str,
        lambda: &(dyn Fn() -> Result<()> + Sync),
    ) -> Result<()> {
        if self.should_skip_locking() {
            return Ok(());
        }

        let error_message =
            "Should be present as alternative is called within a choose that pushes element in";
        let current_thread = thread::current().id();

        let thread_info_map = self.thread_information.lock().unwrap();
        let thread_info = thread_info_map
            .get(&current_thread)
            .expect(PRECON_THREAD_INFO)
            .borrow_mut();

        let choice_is_exclusive = matches!(
            thread_info.alternative_mode.last().expect(error_message),
            ChooseVariant::Exclusive
        );

        let other_branch_executed = *thread_info
            .alternative_executed
            .last()
            .expect(error_message);
        if choice_is_exclusive && other_branch_executed {
            return Ok(());
        }

        // Fine to drop here since branches are executed sequentially
        drop(thread_info);
        drop(thread_info_map);
        let condition_res = self.clone().evaluate_condition(condition)?;

        let thread_info_map = self.thread_information.lock().unwrap();
        let mut thread_info = thread_info_map
            .get(&current_thread)
            .expect(PRECON_THREAD_INFO)
            .borrow_mut();
        if condition_res {
            // Make sure only one thread is executed for choice
            *thread_info
                .alternative_executed
                .last_mut()
                .expect(error_message) = true;
        }

        drop(thread_info);
        drop(thread_info_map);
        let in_search_mode = self.in_search_mode(None);

        // This is enough for the first found search position but not for subsequent ones -> If we have 2 search positions in 2 alternatives -> Will only execute first
        if condition_res || in_search_mode {
            self.execute_lambda(lambda)?;
        }

        // True if we changed from search mode (true -> false) -> Found search position in lambda
        if in_search_mode != self.in_search_mode(None) {
            let current_thread = thread::current().id();
            let thread_info_map = self.thread_information.lock().unwrap();
            // Unwrap as we have precondition that thread info is available on spawning
            let mut thread_info = thread_info_map
                .get(&current_thread)
                .expect(PRECON_THREAD_INFO)
                .borrow_mut();
            *thread_info
                .alternative_executed
                .last_mut()
                .expect(error_message) = true;
        }
        Ok(())
    }

    fn otherwise(self: Arc<Self>, lambda: &(dyn Fn() -> Result<()> + Sync)) -> Result<()> {
        if self.should_skip_locking() {
            return Ok(());
        };
        let current_thread = thread::current().id();
        let thread_info_map = self.thread_information.lock().unwrap();
        // Unwrap as we have precondition that thread info is available on spawning
        let thread_info = thread_info_map
            .get(&current_thread)
            .expect(PRECON_THREAD_INFO)
            .borrow_mut();
        let alternative_executed = *thread_info.alternative_executed.last().expect(
            "Should be present as alternative is called within a choose that pushes element in",
        );
        drop(thread_info);
        drop(thread_info_map);
        if self.in_search_mode(None) || !alternative_executed {
            self.execute_lambda(lambda)?;
        }
        Ok(())
    }

    fn loop_exec(
        self: Arc<Self>,
        condition: [&str; 2],
        lambda: &(dyn Fn() -> Result<()> + Sync),
    ) -> Result<()> {
        let mut test_type = *condition.get(0).unwrap();
        let condition = *condition.get(1).unwrap();

        if !matches!(test_type, "pre_test" | "post_test") {
            log::error!("Test type is: {:?}", test_type);
            return Err(Error::GeneralError(
                "Condition must be called pre_test or post_test".to_owned(),
            ));
        }

        if self.should_skip_locking() {
            return Ok(());
        }

        if self.in_search_mode(None) {
            match self.execute_lambda(&lambda) {
                Ok(()) => {}
                Err(err) => {
                    // If the lambda throws up a BreakLoop -> We land here
                    if !matches!(err, Error::BreakLoop()) {
                        return Err(err);
                    }
                }
            };
            if self.in_search_mode(None) {
                return Ok(());
            } else {
                test_type = "pre_test";
            }
        }

        let loop_id = generate_random_key();
        // catch :escape
        match test_type {
            "pre_test" => {
                while self.clone().evaluate_condition(condition)? && !self.should_skip_locking() {
                    match self.execute_lambda(&lambda) {
                        Ok(()) => {}
                        Err(err) => {
                            if matches!(err, Error::BreakLoop()) {
                                // If we got an escape instruction -> Break this loop, other errors are bubbled further up
                                break;
                            } else {
                                return Err(err);
                            }
                        }
                    };
                    ConnectionWrapper::loop_guard(self.clone(), &loop_id);
                }
            }
            "post_test" => {
                loop {
                    // do-while
                    match self.execute_lambda(&lambda) {
                        Ok(()) => {}
                        Err(err) => {
                            if matches!(err, Error::BreakLoop()) {
                                // If we got an escape instruction -> Break this loop, other errors are bubbled further up
                                break;
                            } else {
                                return Err(err);
                            }
                        }
                    };
                    ConnectionWrapper::loop_guard(self.clone(), &loop_id);

                    let continue_loop =
                        self.clone().evaluate_condition(condition)? && !self.should_skip_locking();
                    if !continue_loop {
                        break;
                    }
                }
            }
            x => log::error!("This condition type is not allowed: {}", x),
        }
        // after loop is done, remove the loop guard entry
        self.loop_guard.lock().unwrap().remove(&loop_id);
        Ok(())
    }

    fn pre_test(condition: &str) -> [&str; 2] {
        ["pre_test", condition]
    }

    fn post_test(condition: &str) -> [&str; 2] {
        ["post_test", condition]
    }

    fn critical_do(
        self: Arc<Self>,
        mutex_id: &str,
        lambda: &(dyn Fn() -> Result<()> + Sync),
    ) -> Result<()> {
        if self.should_skip_locking() {
            return Ok(());
        }
        let mutex = self
            .critical_section_mutexes
            .insert(mutex_id.to_owned(), |_key| Box::new(Mutex::new(())));
        // Guard this mutex lock until the lambda is executed
        let mutex_lock = mutex.lock().unwrap();
        self.execute_lambda(lambda)?;
        drop(mutex_lock);
        todo!()
    }

    fn stop(self: Arc<Self>, id: &str) -> Result<()> {
        if self.in_search_mode(None) {
            return Ok(());
        }
        
        let current_thread = thread::current().id();
        let thread_info_map = self.thread_information.lock().unwrap();
        let thread_info = thread_info_map
            .get(&current_thread)
            .expect(PRECON_THREAD_INFO)
            .borrow_mut();

        if self.should_skip(&thread_info) {
            return Ok(());
        }  

        if let Some(parent) = &thread_info.parent_thread {
            let mut parent_thread_info = thread_info_map
                .get(parent)
                .expect("Since we have a reference, threadinfo of parent has to exist")
                .borrow_mut();
            if !parent_thread_info
                .branch_traces
                .contains_key(&thread::current().id())
            {
                parent_thread_info
                    .branch_traces
                    .insert(thread::current().id(), Vec::new());
            }
            parent_thread_info
                .branch_traces
                .get_mut(&thread::current().id())
                .unwrap()
                .push(id.to_owned());
        }

        drop(thread_info);        
        drop(thread_info_map);        
        self.weel_progress(id.to_owned(), "0".to_owned(), true)?;
        self.set_state(State::Stopping)?;
        Ok(())
    }

    fn terminate(self: Arc<Self>) -> Result<()> {
        if self.in_search_mode(None) || self.should_skip_locking() {
            return Ok(());
        }

        *self.state.lock().unwrap() = State::Finishing;
        Ok(())
    }

    fn escape(self: Arc<Self>) -> Result<()> {
        if self.in_search_mode(None) || self.should_skip_locking() {
            return Ok(());
        }
        return Err(Error::BreakLoop());
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
                        self.set_state(State::Running)?;
                    }
                    let result = model();
                    match result {
                        // TODO: Implement __weel_control_flow completely
                        Ok(()) => {
                            let state = self.state.lock().unwrap();
                            match *state {
                                State::Running | State::Finishing => {
                                    let positions: Vec<PositionDTO> = self
                                        .positions
                                        .lock()
                                        .unwrap()
                                        .iter()
                                        .map(|e| PositionDTO {
                                            position: e.position.clone(),
                                            uuid: e.uuid.clone(),
                                            detail: e.detail.lock().unwrap().clone(),
                                            handler_passthrough: e
                                                .handler_passthrough
                                                .lock()
                                                .unwrap()
                                                .clone(),
                                        })
                                        .collect();
                                    let ipc = json!({
                                        "unmark": positions
                                    });
                                    match ConnectionWrapper::new(self.clone(), None, None)
                                        .inform_position_change(Some(ipc))
                                    {
                                        Ok(()) => {
                                            drop(state);
                                            self.set_state(State::Finished)?;
                                        }
                                        Err(err) => {
                                            drop(state);
                                            self.handle_error(err, true);
                                            self.set_state(State::Stopped)?;
                                        }
                                    };
                                }
                                State::Stopping => {
                                    drop(state);
                                    self.recursive_join(thread::current().id())?;
                                    self.set_state(State::Stopped)?;
                                }
                                _ => {
                                    log::error!("Recached end of process in state: {:?}", state)
                                    //Do nothing
                                }
                            }
                        }
                        Err(err) => {
                            self.handle_error(err, true);
                            self.set_state(State::Stopped)?;
                        },
                    }

                    // Signal stop thread that execution of model ended:
                    let send_result = stop_signal_sender.send(());
                    if matches!(send_result, Err(_)) {
                        log::error!("Error sending termination signal for model thread. Receiver must have been dropped.")
                    }
                } else {
                    self.abort_start();
                };
            }
            Err(err) => {
                self.handle_error(err, true);
                self.set_state(State::Stopped)?;
            },
        }
        Ok(())
    }

    fn recursive_join(&self, thread: ThreadId) -> Result<()> {
        let children: Vec<ThreadId>;
        let mut joined_thread = false;
        {
            let thread_map = self.thread_information.lock().unwrap();
            let mut thread_info = thread_map
                .get(&thread)
                .expect(PRECON_THREAD_INFO)
                .borrow_mut();
            children = thread_info.branches.clone();
            // We cannot wait on the main thread
            if thread != thread::current().id() {
                if let Some(handle) = thread_info.join_handle.take() {
                    // Release lock on thread info map to allow threads to run to the end (in case they need to acquire the lock)
                    drop(thread_info);
                    drop(thread_map);
                    // wait for thread to terminate
                    match handle.join() {
                        Ok(res) => {
                            res?;
                            joined_thread = true;
                        }
                        Err(err) => {
                            log::error!("error when joining thread with id {:?}: {:?}", thread, err)
                        }
                    };
                }
            }
        }

        // join all child threads:
        for child in children {
            self.recursive_join(child)?;
        }
        /*
        Disabled for now, would also require locking parent and removing the thread from branches. 
        if joined_thread {
            // cleanup thread info after we joined it and all of its children
            self.thread_information.lock().unwrap().remove(&thread);  
        }
        */
        Ok(())
    }

    fn abort_start(&self) {
        let mut state = self.state.lock().expect("Could not lock state mutex");
        // Should only be called when the start is aborted through voting (aka. weel is still in ready state):
        assert_eq!(*state, State::Ready);
        *state = State::Stopped;
    }

    pub fn stop_weel(self: &Arc<Self>, main_thread_id: ThreadId) -> Result<()> {
        {
            let state = self.state.lock().expect("Could not lock state mutex");

            match *state {
                State::Ready => {
                    drop(state);
                    self.set_state_on_thread(main_thread_id, State::Stopped)?;
                }
                State::Running => {
                    drop(state);
                    self.set_state_on_thread(main_thread_id, State::Stopping)?;
                    let rec_result = self
                        .stop_signal_receiver
                        .lock()
                        .unwrap()
                        .as_ref()
                        .expect("Has been set after init")
                        .recv();
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
                self.get_instance_meta_data(),
                Some(serde_json::Value::Bool(true)),
            )?;
        }
        Ok(())
    }

    /**
     * Requires the threads thread info -> thread info needs to be locked first
     * This version is useful if you need to lock the thread information anyway,
     * then you do not have to release and reaquire the lock  
     */
    fn should_skip(&self, thread_info: &ThreadInfo) -> bool {
        let no_longer_necessary = thread_info.no_longer_necessary;
        self.terminating() || no_longer_necessary
    }

    /**
     * This version acquires the lock itself, this is useful if you do not need the
     * thread_info anyway
     */
    fn should_skip_locking(&self) -> bool {
        if self.terminating() {
            return true;
        }
        let current_thread_id = thread::current().id();
        let thread_info_map = self.thread_information.lock().unwrap();
        // Unwrap as we have precondition that thread info is available on spawning
        let thread_info = thread_info_map
            .get(&current_thread_id)
            .expect(PRECON_THREAD_INFO)
            .borrow();

        let no_longer_necessary = thread_info.no_longer_necessary;
        no_longer_necessary
    }

    fn terminating(&self) -> bool {
        matches!(
            *self.state.lock().unwrap(),
            State::Stopping | State::Stopped | State::Finishing
        )
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
                json!(self.opts.attributes),
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
                        .remove(message["name"].as_str().unwrap());
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
        log::debug!("At activity: {activity_id}, search mode is: {in_search_mode}");
        if in_search_mode {
            return Ok(());
        }
        let connection_wrapper =
            ConnectionWrapper::new(self.clone(), Some(position.to_owned()), None);
        let connection_wrapper_mutex = Arc::new(Mutex::new(connection_wrapper));

        let mut weel_position: Option<Arc<Position>> = None;

        /*
         * We use a block computation here to mimick the exception handling -> If an exception in the original ruby code is raised, we return it here
         */
        let result: Result<()> = 'raise: {
            let current_thread_id = thread::current().id();
            let thread_info_map = self.thread_information.lock().unwrap();
            // Unwrap as we have precondition that thread info is available on spawning
            let mut thread_info = thread_info_map
                .get(&current_thread_id)
                .expect(PRECON_THREAD_INFO)
                .borrow_mut();

            log::debug!("Activity {activity_id} in state {:?} no longer necessary: {}",*self.state.lock().unwrap(), thread_info.no_longer_necessary);
            if self.should_skip(&thread_info) {
                break 'raise Ok(()); // Will execute the finalize (ensure block)
            }

            thread_info.callback_signals = Arc::new(BlockingQueue::new());
            let mut connection_wrapper = connection_wrapper_mutex.lock().unwrap();
            connection_wrapper.handler_continue = Some(thread_info.callback_signals.clone());

            let parent = thread_info.parent_thread.clone();

            // Register position/label of this thread in the branch traces of the parent thread
            if parent.is_some() {
                let mut parent_thread_info = thread_info_map
                    .get(&parent.unwrap())
                    .expect(PRECON_THREAD_INFO)
                    .borrow_mut();
                let traces = parent_thread_info
                    .branch_traces
                    .get_mut(&thread::current().id());
                match traces {
                    Some(traces) => traces.push(position.to_owned()),
                    None => {
                        parent_thread_info
                            .branch_traces
                            .insert(thread::current().id(), Vec::new());
                    }
                }
            };

            // Local information should not change outside of this thread TODO: add this to actual thread_local_storage
            let local = thread_info.local.clone();
            // Drop the thread_info here already as for a manipulate we do not need it at all and a call we need to acquire the lock every 'again loop anyway
            drop(thread_info);
            drop(thread_info_map);
            weel_position = Some(self.weel_progress(
                position.to_owned(),
                connection_wrapper.handler_activity_uuid.clone(),
                false,
            )?);
            log::debug!("Reached activity type for activity {activity_id}");
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
                            connection_wrapper.activity_manipulate_handle(parameters.map(|p| p.label).unwrap_or(""));
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
                    *weel_position.as_ref().unwrap().detail.lock().unwrap() = "after".to_owned();
                    let ipc = json!({
                        "after": [**weel_position.as_ref().unwrap()]
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
                        let thread_info = thread_info_map
                            .get(&current_thread)
                            .expect(PRECON_THREAD_INFO)
                            .borrow_mut();
                        // TODO: In manipulate we directly "abort" and do not run code, here we run code and then check for abort, is this correct?
                        let mut connection_wrapper = connection_wrapper_mutex.lock().unwrap();
                        let parameters = match connection_wrapper.prepare(
                            prepare_code,
                            &thread_info.local,
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
                        log::debug!("Before activity-handle for activity {activity_id}");
                        ConnectionWrapper::activity_handle(
                            &connection_wrapper_mutex,
                            weel_position
                                .as_mut()
                                .unwrap()
                                .handler_passthrough
                                .lock()
                                .unwrap()
                                .as_ref()
                                .map(|x| x.as_str()),
                            parameters,
                        )?;

                        let connection_wrapper = connection_wrapper_mutex.lock().unwrap();
                        *weel_position
                            .as_ref()
                            .unwrap()
                            .handler_passthrough
                            .lock()
                            .unwrap() = connection_wrapper.handler_passthrough.clone();
                        let passthrough_set = weel_position.as_ref().unwrap().handler_passthrough.lock().unwrap().is_some();
                        if passthrough_set
                        {
                            let connection_wrapper = ConnectionWrapper::new(
                                self.clone(),
                                // Do not need this data for the inform:
                                None,
                                None,
                            );
                            let content = json!({
                                "wait": **weel_position.as_ref().unwrap()
                            });
                            connection_wrapper.inform_position_change(Some(content))?;
                        };
                        drop(connection_wrapper);
                        
                        'inner: loop {
                            log::debug!("Enterted loop");
                            let current_thread = thread::current().id();
                            let thread_info_map = self.thread_information.lock().unwrap();
                            // Unwrap as we have precondition that thread info is available on spawning
                            let thread_info = thread_info_map
                                .get(&current_thread)
                                .expect(PRECON_THREAD_INFO)
                                .borrow();
                            let state_stopping_or_finishing = matches!(
                                *self.state.lock().unwrap(),
                                State::Stopping | State::Stopped | State::Finishing
                            );
                            let connection_wrapper = connection_wrapper_mutex.lock().unwrap();

                            let should_block =
                                !state_stopping_or_finishing && !thread_info.no_longer_necessary;
                            let mut wait_result = None;

                            // Get reference on the queue to allow us to unlock the rest of the thread info
                            let thread_queue = thread_info.callback_signals.clone();
                            // We need to release the locks on the thread_info_map to allow other parallel branches to execute while we wait for the callback (can take long for async case)
                            drop(thread_info);
                            drop(thread_info_map);
                            // We need to release the connection_wrapper lock here to allow callbacks from redis to lock the wrapper
                            drop(connection_wrapper);

                            log::debug!("Should block: {should_block}");
                            if should_block {
                                wait_result = Some(thread_queue.dequeue());
                            };

                            // Reacquire locks after waiting
                            let current_thread = thread::current().id();
                            let thread_info_map = self.thread_information.lock().unwrap();
                            // Unwrap as we have precondition that thread info is available on spawning
                            let thread_info = thread_info_map
                                .get(&current_thread)
                                .expect(PRECON_THREAD_INFO)
                                .borrow();
                            let connection_wrapper = connection_wrapper_mutex.lock().unwrap();

                            if thread_info.no_longer_necessary {
                                break 'raise Err(Signal::NoLongerNecessary.into());
                            }
                            // Store local for code execution -> allows us to unlock the thread_local_map here and since local is a snapshot, it should be fine
                            let local = thread_info.local.clone();
                            drop(thread_info);
                            drop(thread_info_map);

                            let state_stopping_or_finishing = matches!(
                                *self.state.lock().unwrap(),
                                State::Stopping | State::Stopped | State::Finishing
                            );
                            if state_stopping_or_finishing {
                                connection_wrapper.activity_stop()?;
                                *weel_position
                                    .as_mut()
                                    .unwrap()
                                    .handler_passthrough
                                    .lock()
                                    .unwrap() = connection_wrapper.activity_passthrough_value();
                                if weel_position
                                    .as_ref()
                                    .unwrap()
                                    .handler_passthrough
                                    .lock()
                                    .unwrap()
                                    .is_some()
                                {
                                    break 'raise Err(Signal::Proceed.into());
                                }
                            };

                            let signaled_update_again = wait_result
                                .as_ref()
                                .map(|res| matches!(res, Signal::Again))
                                .unwrap_or(false);
                            let return_value_empty = connection_wrapper
                                .handler_return_value
                                .clone()
                                .map(|x| x.is_null() || x.as_array().map(|arr| arr.is_empty()).unwrap_or(false))
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
                                        "Service call for activity {activity_id} returned status code {:?}, and no salvage/rescue code was provided",
                                        connection_wrapper.handler_return_status
                                    )));
                                }
                            } else {
                                code_type = "finalize";
                                finalize_code
                            };
                            log::debug!("Code type: {}", code_type);
                            log::debug!("Return value: {:?}", connection_wrapper.handler_return_value);

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
                                    connection_wrapper.handler_return_options.clone(),
                                ) {
                                    // When error/signal returned, pass it downwards for handling, except for Signal::Again that has some direct effects
                                    Ok(res) => res,
                                    Err(err) => match err {
                                        Error::EvalError(eval_error) => match eval_error {
                                            EvalError::Signal(signal, evaluation_result) => {
                                                match signal {
                                                    Signal::Again => {
                                                        signaled_again = true;
                                                        evaluation_result
                                                    }
                                                    other => {
                                                        break 'raise Err(Error::Signal(other))
                                                    }
                                                }
                                            }
                                            other_eval_error => {
                                                break 'raise Err(Error::EvalError(
                                                    other_eval_error,
                                                ))
                                            }
                                        },
                                        other_error => break 'raise Err(other_error),
                                    },
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
                            *weel_position
                                .as_ref()
                                .unwrap()
                                .handler_passthrough
                                .lock()
                                .unwrap() = None;
                            *weel_position.as_ref().unwrap().detail.lock().unwrap() =
                                "after".to_owned();
                            let content = json!({
                                "after": [**weel_position.as_ref().unwrap()]
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
                Error::Signal(signal) => {
                    let weel_position = weel_position.expect(&format!("Somehow reached signal handling on signal: {:?} without initializing position", signal));
                    match signal {
                        Signal::Proceed | Signal::SkipManipulate => {
                            let state_stopping_or_finishing = matches!(
                                *self.state.lock().unwrap(),
                                State::Stopping | State::Finishing
                            );
                            if !state_stopping_or_finishing
                                && !self.vote_sync_after(&connection_wrapper)?
                            {
                                self.set_state(State::Stopping)?;
                                *weel_position.detail.lock().unwrap() = "unmark".to_owned();
                            }
                        }
                        Signal::NoLongerNecessary => {
                            connection_wrapper.inform_activity_cancelled()?;
                            connection_wrapper.inform_activity_done()?;
                            self.positions
                                .lock()
                                .unwrap()
                                .retain(|pos| !Arc::ptr_eq(pos, &weel_position));
                            let current_thread = thread::current().id();
                            let thread_info_map = self.thread_information.lock().unwrap();
                            // Unwrap as we have precondition that thread info is available on spawning
                            let mut thread_info = thread_info_map
                                .get(&current_thread)
                                .expect(PRECON_THREAD_INFO)
                                .borrow_mut();
                            thread_info.branch_position = None;

                            *weel_position.handler_passthrough.lock().unwrap() = None;
                            *weel_position.detail.lock().unwrap() = "unmark".to_owned();

                            let ipc = json!({
                                "unmark": [*weel_position]
                            });
                            ConnectionWrapper::new(self.clone(), None, None)
                                .inform_position_change(Some(ipc))?;
                        }
                        Signal::Stop | Signal::StopSkipManipulate => {
                            self.set_state(State::Stopping)?;
                        }
                        Signal::Skip => {}
                        x => {
                            log::error!("Received unexpected signal: {:?}", x);
                        }
                    }
                }
                Error::EvalError(eval_error) => match eval_error {
                    EvalError::SyntaxError(message) => {
                        connection_wrapper.inform_activity_failed(Error::EvalError(
                            EvalError::SyntaxError(message),
                        ))?;
                        self.set_state(State::Stopping)?;
                    }
                    EvalError::Signal(_signal, _evaluation_result) => {
                        log::error!("Handling EvalError::Signal in weel_activity, this should never happen! Should be \"raised\" as Error::Signal");
                        panic!("Handling EvalError::Signal in weel_activity, this should never happen! Should be \"raised\" as Error::Signal");
                    }
                    // Runtime and general evaluation errors use the default error handling
                    other => {
                        self.handle_error(Error::EvalError(other), true);
                    }
                },
                err => {
                    self.handle_error(err, true);
                }
            };
        };
        log::debug!("Reached finalize for activity {activity_id}");
        self.finalize_call_activity();
        Ok(())
    }

    fn finalize_call_activity(self: Arc<Self>) {
        // Check whether we need to handle interactions due to the parallel gate
        let current_thread = thread::current().id();
        let thread_info_map = self.thread_information.lock().unwrap();
        // Unwrap as we have precondition that thread info is available on spawning
        let thread_info = thread_info_map
            .get(&current_thread)
            .expect(PRECON_THREAD_INFO)
            .borrow_mut();

        // If we have a parent thread, that means we are not in the main thread -> We are spawned by a parallel gateway in the parent thread
        if let Some(parent_id) = thread_info.parent_thread {
            self.parallel_gateway_update(&thread_info_map, parent_id, thread_info, current_thread);
        }
    }

    fn parallel_gateway_update(
        &self,
        thread_info_map: &MutexGuard<'_, HashMap<ThreadId, RefCell<ThreadInfo>>>,
        parent_id: ThreadId,
        mut thread_info: std::cell::RefMut<'_, ThreadInfo>,
        current_thread: ThreadId,
    ) {
        let mut parent_info = thread_info_map
            .get(&parent_id)
            .expect(PRECON_THREAD_INFO)
            .borrow_mut();

        if parent_info.parallel_wait_condition == CancelCondition::First {
            if thread_info.first_activity_in_thread
                && parent_info.branch_wait_count < parent_info.branch_wait_threshold
            {
                thread_info.first_activity_in_thread = false;
                parent_info.branch_wait_count = parent_info.branch_wait_count + 1;
            }
        }
        let state_not_stopping_or_finishing = match *self.state.lock().unwrap() {
            State::Stopping | State::Finishing => false,
            _other => true,
        };
        // Drop to reacquire under parent
        drop(thread_info);
        // if we reach the threshold, we can put all the other threads on no longer necessary
        if parent_info.branch_wait_threshold == parent_info.branch_wait_count
            && state_not_stopping_or_finishing
        {
            // Will iteratively mark all children as no longer necessary
            for child_id in &parent_info.branches {
                match thread_info_map.get(child_id) {
                    Some(thread_info) => {
                        let mut thread_info = thread_info.borrow_mut();
                        if thread_info.first_activity_in_thread {
                            thread_info.no_longer_necessary = true;
                            drop(thread_info);
                            // Should be fine w.r.t. mutable borrows, since this will continue recusively down the hierarchy
                            recursive_continue(&thread_info_map, &current_thread)
                        }
                    }
                    None => {
                        log::error!("Child Thread of Thread {:?} with id: {:?} does not have any thread info", parent_id, child_id);
                        log::error!("{}", PRECON_THREAD_INFO)
                    }
                }
            }
        }
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
        local: &Option<Value>,
        connection_wrapper: &ConnectionWrapper,
        location: &str,
        call_result: Option<Value>,
        call_headers: Option<HashMap<String, String>>,
    ) -> Result<eval_helper::EvaluationResult> {
        log::debug!("Executing code at: {}", location);
        log::debug!("Using call result: {:?}", call_result);
        // We clone the dynamic data and status dto here which is expensive but allows us to not block the whole weel until the eval call returns
        if read_only {
            let dynamic_data = self.context.lock().unwrap().clone();
            let status = self.status.lock().unwrap().to_dto();
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
            let dynamic_data = self.context.lock().unwrap().clone();
            let status = self.status.lock().unwrap().to_dto();
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
        let thread_info = thread_info_map
            .get(&current_thread.id())
            .expect(PRECON_THREAD_INFO)
            .borrow();
        let thread_local = thread_info.local.clone();
        drop(thread_info);
        drop(thread_info_map);
        let result = eval_helper::test_condition(
            &self.context.lock().unwrap(),
            &self.opts,
            condition,
            &thread_local,
            &connection_wrapper,
        );
        match result {
            Ok(cond) => Ok(cond),
            Err(err) => {
                self.set_state(State::Stopping)?;
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

    fn execute_lambda(self: &Arc<Self>, lambda: &(dyn Fn() -> Result<()> + Sync)) -> Result<()> {
        let result = lambda();
        match result {
            Ok(()) => Ok(()),
            Err(err) => {
                self.set_state(State::Stopping)?;
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
            self.set_state(State::Stopping)?;
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
        let mut thread_info = thread_info_map
            .get(&thread.id())
            .expect(PRECON_THREAD_INFO)
            .borrow_mut();

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
                log::debug!("Found search position: {activity_id}");
                // We found the first position on this branch -> We do not need to search futher along this branch of execution
                thread_info.in_search_mode = false;
                thread_info.switched_to_execution = true;
                while let Some(parent) = thread_info.parent_thread {
                    // Each parent thread has to have some thread information. In general all threads should, when they spawn via weel register and add their thread information
                    // Communicate to ancestor branches that in one of its childs a label was found and the search is done.
                    thread_info = thread_info_map
                        .get(&parent)
                        .expect(PRECON_THREAD_INFO)
                        .borrow_mut();
                    thread_info.in_search_mode = false;
                    thread_info.switched_to_execution = true;
                }
                // checked earlier for membership, thus we can simply unwrap:
                *self
                    .search_positions
                    .lock()
                    .unwrap()
                    .get(activity_id)
                    .unwrap()
                    .detail
                    .lock()
                    .unwrap()
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
    ) -> Result<Arc<Position>> {
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
                    log::error!("{}", PRECON_THREAD_INFO);
                    panic!("Thread information not present!")
                }
            };
            if let Some(branch_position) = &current_thread_info.branch_position {
                self.positions
                    .lock()
                    .unwrap()
                    .retain(|x| !Arc::ptr_eq(x, branch_position));
                ipc.insert("unmark".to_owned(), json!([**branch_position]));
            };
            let mut search_positions = self.search_positions.lock().unwrap();
            let search_position = search_positions.remove(&position);
            let passthrough = search_position
                .map(|pos| pos.handler_passthrough.lock().unwrap().clone())
                .flatten();
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
            let position = Arc::new(weel_position);
            self.positions.lock().unwrap().push(position.clone());
            current_thread_info.branch_position = Some(position.clone());

            (current_thread_info.parent_thread, position)
        };
        if let Some(parent_thread_id) = parent_thread_id {
            let mut parent_thread_info = match thread_info_map.get(&parent_thread_id) {
                Some(x) => x.borrow_mut(),
                None => {
                    log::error!(
                        "Thread information for branch {:?} is empty",
                        current_thread.id()
                    );
                    log::error!("{}", PRECON_THREAD_INFO);
                    panic!("Thread information not present!")
                }
            };
            if let Some(branch_position) = parent_thread_info.branch_position.take() {
                self.positions
                    .lock()
                    .unwrap()
                    .retain(|x| !Arc::ptr_eq(x, &branch_position));
                if !ipc.contains_key("unmark") {
                    ipc.insert("unmark".to_owned(), json!([]));
                }
                ipc.get_mut("unmark")
                    .expect("has to be present")
                    .as_array_mut()
                    .expect("Has to be array")
                    .push(json!(*branch_position));
            };
        };
        ConnectionWrapper::new(self.clone(), None, None).inform_position_change(Some(ipc_node))?;
        Ok(weel_position)
    }

    /**
     * Locks: state and potentially positions and status (via set_state)
     */
    pub fn handle_error(self: &Arc<Self>, err: Error, should_set_stopping: bool) {
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
        if should_set_stopping {
            match self.set_state(State::Stopping) {
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
        };
    }

    pub fn get_instance_meta_data(&self) -> InstanceMetaData {
        InstanceMetaData {
            cpee_base_url: self.opts.base_url().to_owned(),
            instance_id: self.opts.instance_id.clone(),
            instance_url: self.opts.instance_url(),
            instance_uuid: self.uuid().to_owned(),
            info: self.info().to_owned(),
            attributes: self.opts.attributes.clone(),
        }
    }

    pub fn uuid(&self) -> &str {
        self.opts
            .attributes
            .get("uuid")
            .expect("Attributes do not contain uuid")
    }

    pub fn info(&self) -> &str {
        self.opts
            .attributes
            .get("info")
            .expect("Attributes do not contain info")
    }

    /**
     * Sets the state of the weel
     *
     * Locks: state and potentially positions and status
     */
    pub fn set_state(self: &Arc<Self>, new_state: State) -> Result<()> {
        self.set_state_on_thread(thread::current().id(), new_state)
    }

    /**
     * Sets the state of the weel
     *
     * Locks: state and potentially positions and status
     */
    fn set_state_on_thread(self: &Arc<Self>, thread_id: ThreadId, new_state: State) -> Result<()> {
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
            recursive_continue(&self.thread_information.lock().unwrap(), &thread_id);
        }

        ConnectionWrapper::new(self.clone(), None, None).inform_state_change(new_state)?;
        Ok(())
    }
}

fn recursive_continue(
    thread_info_map: &MutexGuard<HashMap<ThreadId, RefCell<ThreadInfo>>>,
    thread_id: &ThreadId,
) {
    let thread_info = thread_info_map
        .get(thread_id)
        .expect(PRECON_THREAD_INFO)
        .borrow();
    // Make async tasks continue
    thread_info
        .callback_signals
        .enqueue(Signal::None);
    if let Some(branch_event) = &thread_info.branch_event_sender {
        match branch_event.send(()) {
            Ok(()) => {}
            Err(err) => log::error!(
                "Send failed: {:?}, parallel gateway must have terminated already",
                err
            ),
        };
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
    BreakLoop(),
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PositionDTO {
    pub position: String,
    pub uuid: String,
    pub detail: String,
    pub handler_passthrough: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Position {
    position: String,
    uuid: String,
    pub detail: Mutex<String>,
    pub handler_passthrough: Mutex<Option<String>>,
}
impl Position {
    pub fn new(
        position: String,
        uuid: String,
        detail: String,
        handler_passthrough: Option<String>,
    ) -> Self {
        Self {
            position,
            uuid,
            detail: Mutex::new(detail),
            handler_passthrough: Mutex::new(handler_passthrough),
        }
    }
}


pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn to_string(&self) -> String {
        format!("{:?}", self)
    }
}

impl<T> From<PoisonError<T>> for Error {
    fn from(_: PoisonError<T>) -> Self {
        Error::PoisonError()
    }
}

const KEY_LENGTH: usize = 32;

/**
 * Generates random ASCII character string of length KEY_LENGTH -> UUID
 */
pub fn generate_random_key() -> String {
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(KEY_LENGTH)
        .map(char::from)
        .collect()
}