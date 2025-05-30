use crate::{
    data_types::{
        BlockingQueue, CallbackType, Context, HTTPParams, InstanceMetaData, Opts, StatusDTO,
    },
    dsl_realization::{generate_random_key, Error, Result, Signal, Weel},
    eval_helper::{self, evaluate_expression, EvalError},
};
use core::str;
use http_helper::{header_map_to_hash_map, Method, Parameter};
use regex::Regex;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    io::{Read, Seek},
    str::FromStr,
    sync::{Arc, Mutex, Weak},
    thread::{self, sleep, ThreadId},
    time::{Duration, SystemTime},
};
use urlencoding::encode;

#[derive(Debug)]
pub struct ConnectionWrapper {
    weel: Weak<Weel>,
    handler_position: Option<String>,
    // The queue the calling thread is blocking on. -> Uses handle_callback to signal thread to continue running
    pub handler_continue: Option<Arc<crate::data_types::BlockingQueue<Signal>>>,
    // The identifier of the callback the connection wrapper is waiting for (if none, it is not waiting for it)
    pub handler_passthrough: Option<String>,
    pub handler_return_status: Option<u16>,
    pub handler_return_value: Option<Value>,
    pub handler_return_options: Option<HashMap<String, String>>,
    // We keep them as arrays to be flexible but will only contain one element for now
    // Contains the actual endpoint URL
    handler_endpoints: Vec<String>,
    // Original endpoint without the sim_translate
    handler_endpoint_origin: Vec<String>,
    // Unique identifier (randomly created)
    pub handler_activity_uuid: String,
    activity_label: String,
    annotations: Option<Value>,
    error_regex: Regex,
}
// Determines whether recurring calls are too close together (in seconds)
const LOOP_GUARD_DELTA: f32 = 2.0;
// Determines how many calls can be made in total before an activity might be throttled
const UNGUARDED_CALLS: u32 = 100;
// Determines how many seconds the call should be delayed (for throttling)
const SLEEP_DURATION: u64 = 2;

impl ConnectionWrapper {
    pub fn new(
        weel: Arc<Weel>,
        // Corresponds to the label of the activity the handler is initialized for
        handler_position: Option<String>,
        handler_continue: Option<Arc<BlockingQueue<Signal>>>,
    ) -> Self {
        let weel = Arc::downgrade(&weel);
        ConnectionWrapper {
            weel,
            handler_position,
            handler_continue,
            handler_passthrough: None,
            handler_return_status: None,
            handler_return_value: None,
            handler_return_options: None,
            handler_endpoints: Vec::new(),
            handler_activity_uuid: generate_random_key(),
            activity_label: "".to_owned(),
            handler_endpoint_origin: Vec::new(),
            annotations: None,
            error_regex: Regex::new(r#"(.*?)(, Line |:)(\d+):\s(.*)"#).unwrap(),
        }
    }

    /**
     * If too many request are issued to an address by the same wrapper, will throttle these requests
     */
    pub fn loop_guard(weel: Arc<Weel>, id: &str) {
        let attributes = &weel.opts.attributes;
        let loop_guard_attribute = attributes.get("nednoamol");
        if loop_guard_attribute.is_some_and(|attrib| attrib == "true") {
            return;
        }
        match weel.loop_guard.lock().as_mut() {
            Ok(map) => {
                let last = map.get(id);
                let should_throttle = match last {
                    // Some: loop guard was hite prior to this -> increase count
                    Some(entry) => {
                        let count = entry.0 + 1;
                        let last_call_time = entry.1;
                        map.insert(id.to_owned(), (count, SystemTime::now()));

                        let last_call_too_close = last_call_time
                            .elapsed()
                            .expect("last call is in the future")
                            .as_secs_f32()
                            < LOOP_GUARD_DELTA;
                        let threshold_passed = count > UNGUARDED_CALLS;
                        last_call_too_close && threshold_passed
                    }
                    None => {
                        map.insert(id.to_owned(), (1, SystemTime::now()));
                        false
                    }
                };
                if should_throttle {
                    sleep(Duration::from_secs(SLEEP_DURATION));
                }
            }
            Err(err) => {
                eprintln!("Could not acquire lock {err}");
                panic!("Could not acquire lock in loopguard")
            }
        };
    }

    pub fn inform_state_change(&self, new_state: crate::data_types::State) -> Result<()> {
        let content = json!({
            "state": new_state
        });
        self.inform("state/change", Some(content))
    }

    pub fn inform_syntax_error(&self, err: Error, _code: Option<&str>) -> Result<()> {
        let mut content = json!({});
        self.add_error_information(&mut content, err);

        self.inform("description/error", Some(content))
    }

    pub fn inform_connectionwrapper_error(&self, err: Error) -> Result<()> {
        let mut content = json!({});
        self.add_error_information(&mut content, err);

        self.inform("executionhandler/error", Some(content))
    }

    pub fn inform_position_change(&self, ipc: Option<Value>) -> Result<()> {
        self.inform("position/change", ipc)
    }

    pub fn inform_activity_manipulate(&self) -> Result<()> {
        let mut content: Value = self.construct_basic_content();
        content
            .as_object_mut()
            .unwrap()
            .insert("label".to_owned(), json!(self.activity_label));
        self.inform("activity/manipulating", Some(content))
    }

    pub fn inform_activity_done(&self) -> Result<()> {
        let content = self.construct_basic_content();
        self.inform("activity/done", Some(content))?;
        self.inform_resource_utilization()
    }

    pub fn inform_activity_cancelled(&self) -> Result<()> {
        let content = self.construct_basic_content();
        self.inform("activity/cancelled", Some(content))?;
        self.inform_resource_utilization()
    }

    pub fn inform_activity_failed(&self, err: Error) -> Result<()> {
        let mut content = self.construct_basic_content();
        self.add_error_information(&mut content, err);
        self.inform("activity/failed", Some(content))
    }

    fn inform_resource_utilization(&self) -> Result<()> {
        let mut content = match crate::proc::get_cpu_times() {
            Ok(x) => x,
            Err(err) => match err {
                crate::proc::Error::ParseFloatError(_) => {
                    return Err(Error::GeneralError(
                        "Error parsing floats when calculating CPU Times".to_owned(),
                    ))
                }
                crate::proc::Error::IOError(err) => return Err(Error::IOError(err)),
                crate::proc::Error::Utf8Error(err) => return Err(Error::StrUTF8Error(err)),
            },
        };
        content.insert(
            "mb".to_owned(),
            match crate::proc::get_prop_set_size() {
                Ok(x) => x,
                Err(err) => {
                    return Err(Error::GeneralError(format!(
                        "An error occured when calculating the memory usage: {:?}",
                        err
                    )))
                }
            },
        );

        self.inform("status/resource_utilization", Some(json!(content)))?;
        Ok(())
    }

    pub fn inform_manipulate_change(
        &self,
        evaluation_result: eval_helper::EvaluationResult,
    ) -> Result<()> {
        let content_node = self.construct_basic_content();
        if let Some(changed_status) = evaluation_result.changed_status {
            let mut content_node = content_node.clone();
            let content = content_node
                .as_object_mut()
                .expect("Construct basic content has to return json object");
            content.insert(
                "id".to_owned(),
                serde_json::Value::String(changed_status.id.to_string()),
            );
            content.insert(
                "message".to_owned(),
                serde_json::Value::String(changed_status.message),
            );
            self.inform("status/change", Some(content_node))?;
        }
        if let Some(changed_data) = evaluation_result.changed_data {
            let mut content_node = content_node.clone();
            let content = content_node
                .as_object_mut()
                .expect("Construct basic content has to return json object");
            content.insert(
                "changed".to_owned(),
                json!(changed_data
                    .as_object()
                    .unwrap()
                    .keys()
                    .map(|e| e.to_owned())
                    .collect::<Vec<String>>()),
            );
            content.insert("values".to_owned(), changed_data);
            self.inform("dataelements/change", Some(content_node))?;
        }
        if let Some(changed_endpoints) = evaluation_result.changed_endpoints {
            let mut content_node = content_node.clone();
            let content = content_node
                .as_object_mut()
                .expect("Construct basic content has to return json object");
            content.insert(
                "changed".to_owned(),
                json!(changed_endpoints
                    .keys()
                    .map(|e| e.to_owned())
                    .collect::<Vec<String>>()),
            );
            content.insert("values".to_owned(), json!(changed_endpoints));
            self.inform("endpoints/change", Some(content_node))?;
        }
        Ok(())
    }

    /*
     * Locks:
     *  - Locks the redis_notification_client (shortly)
     */
    fn inform(&self, what: &str, content: Option<Value>) -> Result<()> {
        let weel = self.weel();
        weel.redis_notifications_client
            .lock()
            .expect("Could not acquire mutex")
            .notify(what, content, weel.get_instance_meta_data())?;
        Ok(())
    }

    /**
     * Handles all preparations to execute the activity:
     * - Executes the prepare code
     * - Resolves the endpoints to their actual URLs
     *
     * Locks:
     *  - dynamic data of the weel instance (shortly)
     *  - what the `execute_code()` call locks
     *  -  
     */
    pub fn prepare(
        &mut self,
        prepare_code: Option<&str>,
        thread_local: &Option<Value>,
        endpoint_names: &Vec<&str>,
        mut parameters: HTTPParams,
    ) -> Result<HTTPParams> {
        let weel = self.weel();
        // Execute the prepare code and use the modified context for the rest of this metod (prepare_result) (Note: This context can differ as the prepare will not modify the global context)
        let contex_snapshot = match prepare_code {
            Some(code) => {
                let result =
                    weel.execute_code(true, code, thread_local, self, "prepare", None, None)?;
                // Create snapshot of the context after the code is executed, if nothing changes, use the current dynamic data
                let context = weel.context.lock().unwrap();
                Context {
                    data: result.data.unwrap_or(context.data.clone()),
                    endpoints: result.endpoints.unwrap_or(context.endpoints.clone()),
                    search_positions: HashMap::new(), // We can ignore them as they are not relevant to the evaluation context
                }
            }
            None => {
                let dynamic_data = weel.context.lock().unwrap();
                Context {
                    data: dynamic_data.data.clone(),
                    endpoints: dynamic_data.endpoints.clone(),
                    search_positions: HashMap::new(), // We can ignore them as they are not relevant to the evaluation context
                }
            }
        };

        // Resolve the endpoint name to the actual correct endpoint (incl. sim_translate)
        if endpoint_names.len() > 0 {
            self.resolve_endpoints(&contex_snapshot.endpoints, endpoint_names);

            match weel.opts.attributes.get("sim_engine") {
                Some(sim_engine_url) => {
                    if !sim_engine_url.is_empty() {
                        self.handler_endpoint_origin = self.handler_endpoints.clone();

                        let endpoint = encode(self.handler_endpoints.get(0).expect(""));
                        self.handler_endpoints =
                            vec![format!("{}?original_endpoint={}", sim_engine_url, endpoint)];
                    }
                }
                None => {
                    // Do nothing with the endpoints
                }
            };
        };
        self.evaluate_arguments(
            &mut parameters.arguments,
            &contex_snapshot,
            &weel.opts,
            None,
            thread_local,
        )?;
        Ok(parameters)
    }

    fn evaluate_arguments(
        &self,
        arguments: &mut Value,
        context: &Context,
        opts: &Opts,
        weel_status: Option<StatusDTO>,
        thread_local: &Option<Value>,
    ) -> Result<()> {
        if arguments.is_array() {
            for node in arguments.as_array_mut().unwrap() {
                if node.is_array() || node.is_object() {
                    self.evaluate_arguments(
                        node,
                        context,
                        opts,
                        weel_status.clone(),
                        thread_local,
                    )?;
                } else {
                    self.eval_node_and_replace(node, context, opts, thread_local)?;
                }
            }
        } else if arguments.is_object() {
            for (_name, node) in arguments.as_object_mut().unwrap() {
                if node.is_array() || node.is_object() {
                    self.evaluate_arguments(
                        node,
                        context,
                        opts,
                        weel_status.clone(),
                        thread_local,
                    )?;
                } else {
                    self.eval_node_and_replace(node, context, opts, thread_local)?;
                }
            }
        }
        Ok(())
    }

    fn eval_node_and_replace(
        &self,
        node: &mut Value,
        context: &Context,
        opts: &Opts,
        thread_local: &Option<Value>,
    ) -> Result<()> {
        if node.is_null() {
            return Ok(());
        }
        if let Some(text) = node.as_str() {
            if text.starts_with("!") {
                let eval_result = evaluate_expression(
                    context,
                    opts,
                    &text[1..],
                    None,
                    thread_local,
                    self.additional(),
                    // In prepare we do not have access to the call result yet
                    None,
                    None,
                    "prepare",
                )?;
                let evaluated_expression = eval_result.expression_result;
                *node = evaluated_expression;
            }
        };
        Ok(())
    }

    /**
     * Resolves the endpoint names in endpoints to the actual endpoint URLs
     */
    fn resolve_endpoints(
        &mut self,
        endpoint_urls: &HashMap<String, String>,
        endpoint_names: &Vec<&str>,
    ) {
        self.handler_endpoints = endpoint_names
            .iter()
            .map(|ep| endpoint_urls.get(*ep))
            .filter_map(|item| match item {
                Some(item) => Some(item.clone()),
                None => None,
            })
            .collect();
    }

    pub fn activity_passthrough_value(&self) -> Option<String> {
        self.handler_passthrough.clone()
    }

    pub fn activity_manipulate_handle(&mut self, label: &str) {
        self.activity_label = label.to_owned();
    }

    /**
     * Will cancel an activity via the redis_helper thread
     *
     * May lock redis_notification client due to cancel_callback call on weel
     */
    pub fn activity_stop(&self) -> Result<()> {
        if let Some(passthrough) = &self.handler_passthrough {
            self.weel().cancel_callback(passthrough)
        } else {
            Ok(())
        }
    }

    /**
     * Executes the actual service call
     * Locks the connection wrapper for the duration of the call
     * Locks:
     *  - connection_wrapper (provided as selfy)
     *  - locks the redis_notification client (shortly)
     */
    pub fn activity_handle(
        selfy: &Arc<Mutex<Self>>,
        passthrough: Option<&str>,
        mut parameters: HTTPParams,
        annotations: &Value,
    ) -> Result<()> {
        let mut this = selfy.lock()?;
        let weel = this.weel();
        if this.handler_endpoints.is_empty() {
            return Err(Error::GeneralError(format!(
                "No endpoint provided for connection wrapper of activity: {}",
                this.activity_label
            )));
        }
        // We do not model annotations anyway -> Can skip this from the original code
        {
            this.activity_label = parameters.label.to_owned();
            this.annotations = Some(annotations.clone());
            this.inform_resource_utilization()?;
            let mut content_node = this.construct_basic_content();
            let content = content_node
                .as_object_mut()
                .expect("Construct basic content should return json object");
            content.insert("label".to_owned(), json!(this.activity_label));
            content.insert("passthrough".to_owned(), json!(passthrough));
            // parameters do not look exactly like in the original (string representation looks different):
            if let Some(annotations) = this.annotations.as_ref() {
                content.insert("annotations".to_owned(), annotations.clone());
            }

            let protocol_regex = match regex::Regex::new(r"^http(s)?-(get|put|post|delete):") {
                Ok(regex) => regex,
                Err(err) => {
                    eprintln!("Could not compile static regex: {err} -> SHOULD NOT HAPPEN");
                    panic!()
                }
            };
            let mut https_enabled = false;
            // cannot be empty, as we checked this in the previous block
            let endpoint = this.handler_endpoints.pop();
            let endpoint = match &endpoint {
                // TODO: Set method by matched method in url
                Some(endpoint) => {
                    match protocol_regex.captures(&endpoint) {
                        Some(capture) => {
                            match capture.get(1) {
                                Some(captured_suffix) => {
                                    if captured_suffix.as_str() == "s" {
                                        https_enabled = true;
                                    }
                                }
                                None => {}
                            }
                            match capture.get(2) {
                                Some(captured_method) => {
                                    match captured_method.as_str().to_lowercase().as_str() {
                                        "post" => {
                                            parameters.method = Method::POST;
                                        }
                                        "get" => {
                                            parameters.method = Method::GET;
                                        }
                                        "put" => {
                                            parameters.method = Method::PUT;
                                        }
                                        "delete" => {
                                            parameters.method = Method::DELETE;
                                        }
                                        "patch" => {
                                            parameters.method = Method::PATCH;
                                        }
                                        "head" => {
                                            parameters.method = Method::HEAD;
                                        }
                                        x => {
                                            eprintln!("Captured unsupported method: {x}")
                                        }
                                    }
                                }
                                None => {}
                            }
                        }
                        None => {}
                    };
                    protocol_regex
                        .replace_all(endpoint, if https_enabled { "https:" } else { "http;" })
                }
                None => {
                    return Err(Error::GeneralError(
                        "No endpoint for curl configured.".to_owned(),
                    ))
                }
            };
            this.handler_endpoints.push((*endpoint).to_owned());

            content.insert("parameters".to_owned(), json!(parameters));
            weel.redis_notifications_client.lock()?.notify(
                "activity/calling",
                Some(content_node),
                weel.get_instance_meta_data(),
            )?
        }
        match passthrough {
            Some(passthrough) => {
                let mut content_node = this.construct_basic_content();
                let content = content_node
                    .as_object_mut()
                    .expect("Construct basic content has to return json object");
                content.insert(
                    "label".to_owned(),
                    serde_json::Value::String(this.activity_label.clone()),
                );
                content.remove("endpoint");
                weel.register_callback(selfy.clone(), passthrough, content_node)?;
                this.handler_passthrough = Some(passthrough.to_owned());
            }
            None => {
                // Drop to allow relocking in the method
                drop(this);
                Self::curl(selfy, &parameters, weel)?
            }
        }
        Ok(())
    }

    /**
     * Variation of original proto curl implementation:
     *      - We no longer support the special prefixed arguments that are transformed into parameters, we convert all parameters into simple url encoded body parameters
     *      - Only the standard headers that are generated in the `henerate_headers` method are send.
     *      - All arguments within the HTTPParams are send as Key-Value pairs as part of the body (application/x-www-form-urlencoded)
     *          -> TODO: Our implementation sends this as multipart, is this fine?
     * - Expects prepare to be called before
     * - We explicitly expect the Arc<Mutex> here since we need to add a reference to the callbacks (by cloning the Arc)
     * Locks:
     *  - connection_wrapper (provided as selfy)
     *  - redis_notification_client (shortly)
     */
    pub fn curl(selfy: &Arc<Mutex<Self>>, parameters: &HTTPParams, weel: Arc<Weel>) -> Result<()> {
        let mut this = selfy.lock().unwrap();
        let callback_id = generate_random_key();
        this.handler_passthrough = Some(callback_id.clone());

        // Generate headers
        let mut headers: HeaderMap =
            this.construct_headers(weel.get_instance_meta_data(), &callback_id)?;

        let mut status: u16;
        let mut response_headers: HashMap<String, String>;
        let mut body;

        let event_regex = match regex::Regex::new(r"[^\w_-]") {
            Ok(regex) => regex,
            Err(err) => {
                eprintln!("Could not compile static regex: {err} -> SHOULD NOT HAPPEN");
                panic!()
            }
        };

        loop {
            // Compute parameters
            let mut params = Vec::new();
            // Params could contain file handles (complex parameters) and thus cannot be cloned -> We cannot clone so we recompute them here
            match parameters.arguments.as_object() {
                Some(object) => {
                    for (key, node) in object {
                        let value = if node.is_null() {
                            "".to_owned()
                        } else {
                            match node.as_str() {
                                Some(val_str) => val_str.to_owned(),
                                None => serde_json::to_string(node)?,
                            }
                        };
                        // Just stringify any nested arguments
                        params.push(Parameter::SimpleParameter {
                            name: key.to_owned(),
                            value,
                            param_type: http_helper::ParameterType::Body,
                        });
                    }
                }
                None => match parameters.arguments.as_array() {
                    Some(args) => {
                        for arg in args {
                            match arg.as_object() {
                                Some(arg) => {
                                    let name = arg.get("name").expect(
                                        "argument array entry does not contain a name attribute",
                                    );
                                    let value = match arg.get("value") {
                                        Some(v) => match v.as_str() {
                                            Some(v_str) => v_str.to_owned(),
                                            None => {
                                                if v.is_null() {
                                                    "".to_owned()
                                                } else {
                                                    serde_json::to_string(v).unwrap()
                                                }
                                            }
                                        },
                                        None => "".to_owned(),
                                    };

                                    params.push(Parameter::SimpleParameter {
                                        name: serde_json::to_string(name)?,
                                        value: value,
                                        param_type: http_helper::ParameterType::Body,
                                    });
                                }
                                None => {
                                    eprintln!(
                                        "Argument {:?} is not an json object!",
                                        serde_json::to_string(arg)
                                    )
                                }
                            }
                        }
                    }
                    None => {
                        eprintln!("Parameter arguments should be an json object!")
                    }
                },
            }

            let mut content_node = json!({
                "activity_uuid": this.handler_activity_uuid,
                "label": this.activity_label
            });
            let content = content_node.as_object_mut().expect("Cannot fail");
            let position = this
                .handler_position
                .as_ref()
                .map(|x| x.clone())
                .unwrap_or("".to_owned());
            content.insert("activity".to_owned(), serde_json::Value::String(position));
            weel.register_callback(Arc::clone(selfy), &callback_id, content_node)?;

            let endpoint = this.handler_endpoints.get(0).unwrap();
            let mut client = http_helper::Client::new(&endpoint, parameters.method.clone())?;
            client.set_request_headers(headers.clone());
            client.add_parameters(params);

            let response = client.execute_raw()?;

            status = response.status_code;
            response_headers = header_map_to_hash_map(&response.headers)?;
            body = response.body;

            if status == 561 {
                match weel.opts.attributes.get("sim_translate") {
                    Some(sim_translate) => {
                        Self::handle_sim_translate(sim_translate, &mut headers, &mut this)?;
                    }
                    None => this.handler_endpoints = this.handler_endpoint_origin.clone(),
                }
                headers.remove("original_endpoint");
            } else {
                // equivalent to do-while status == 561 in original code
                break;
            }
        }

        let uniform_headers = uniformize_headers(&response_headers);
        // If status not okay:
        if status < 200 || status >= 300 {
            response_headers.insert("cpee_salvage".to_owned(), "true".to_owned());
            let callback_res = this.handle_callback(Some(status), CallbackType::Raw(&body), response_headers);
            callback_res?
        } else {
            // Accept callback if header is set
            let callback_header_set = uniform_headers.contains_key("cpee_callback");
            // NOTE: For this area, all headers are checked against lowercase and - subsituted with _ due to the reqwest http library!
            if callback_header_set {
                if body.len() > 0 {
                    response_headers.insert("cpee_update".to_owned(), "true".to_owned());
                    let callback_res = this.handle_callback(Some(status), CallbackType::Raw(&body), response_headers);
                    callback_res?
                } else {
                    // In this case we have an asynchroneous task
                    let mut content_node = json!({
                        "activity_uuid": this.handler_activity_uuid,
                        "label": this.activity_label,
                        "activity": this.handler_position,
                        "endpoint": this.handler_endpoints,
                        "ecid": convert_thread_id(thread::current().id())
                    });
                    let content = content_node.as_object_mut().expect("Cannot fail");

                    let instantiation_header_set =
                        uniform_headers.contains_key("cpee_instantiation");
                    println!("Instantiation header set: {}", instantiation_header_set);
                    if instantiation_header_set {
                        content.insert(
                            "received".to_owned(),
                            serde_json::from_str(
                                uniform_headers.get("cpee_instantiation").unwrap(),
                            )?,
                        );
                        weel.redis_notifications_client.lock().unwrap().notify(
                            "task/instantiation",
                            Some(content_node.clone()),
                            weel.get_instance_meta_data(),
                        )?;
                    }
                    
                    let event_header_set = uniform_headers.contains_key("cpee_event");
                    if event_header_set {
                        // TODO What about value_helper
                        let event = uniform_headers.get("cpee_event").unwrap();
                        let event = event_regex.replace_all(event, "");
                        let what = format!("task/{event}");
                        weel.redis_notifications_client.lock().unwrap().notify(
                            &what,
                            Some(content_node),
                            weel.get_instance_meta_data(),
                        )?;
                    }
                }
            } else {
                let callback_res = this.handle_callback(Some(status), CallbackType::Raw(&body), response_headers);
                callback_res?
            }
        }
        Ok(())
    }

    fn handle_sim_translate(
        sim_translate_url: &String,
        headers: &mut HeaderMap,
        this: &mut std::sync::MutexGuard<ConnectionWrapper>,
    ) -> Result<()> {
        let client = http_helper::Client::new(&sim_translate_url, http_helper::Method::GET)?;
        let result = client.execute()?;
        let status = result.status_code;
        let result_headers = result.headers;
        let mut content = result.content;
        Ok(if status >= 200 && status < 300 {
            let translation_type = match headers.get("cpee_sim_tasktype") {
                Some(transl_type) => match transl_type.to_str()? {
                    "i" => "instantiation",
                    "ir" => "ipc-receive",
                    "is" => "ipc-send",
                    _ => "instantiation",
                },
                None => "instantiation",
            };

            // Assumption about "gtresult.first.value.read": first is the array method to get the first element
            let body = match content.pop().unwrap() {
                Parameter::SimpleParameter { value, .. } => value.clone(),
                Parameter::ComplexParameter {
                    mut content_handle, ..
                } => {
                    let mut body = String::new();
                    content_handle.rewind()?;
                    content_handle.read_to_string(&mut body)?;
                    content_handle.rewind()?;
                    body
                }
            };

            let mut array = json!(body);
            assert!(array.is_array());
            if let Some(array) = array.as_array_mut() {
                for element in array.iter_mut() {
                    if let Some(type_) = element.get("type").map(|e| e.as_str()).flatten() {
                        if type_ == translation_type {
                            if let Some(endpoint) =
                                element.get("endpoint").map(|e| e.as_str()).flatten()
                            {
                                this.handler_endpoints = vec![endpoint.to_owned()];
                            }
                            if let Some(arguments) = element
                                .get_mut("arguments")
                                .map(|e| e.as_object_mut())
                                .flatten()
                            {
                                for (a_name, a_value) in arguments.iter_mut() {
                                    if a_value.is_string() {
                                        let header_name =
                                            a_value.as_str().unwrap().replace("-", "_");
                                        if let Some(header) = result_headers.get(&header_name) {
                                            *a_value = Value::from_str(header)?;
                                        }
                                    } else if a_value.is_object() {
                                        let a_value_clone = a_value.clone();
                                        let a_value_map = a_value_clone.as_object().unwrap();
                                        let a_value = a_value.as_object_mut().unwrap();
                                        for (key, value) in a_value_map {
                                            if value.is_string() {
                                                let header_name =
                                                    value.as_str().unwrap().replace("-", "_");
                                                if let Some(header) = headers.get(header_name) {
                                                    a_value.insert(
                                                        key.clone(),
                                                        serde_json::from_str(header.to_str()?)?,
                                                    );
                                                }
                                            }
                                        }
                                    }

                                    for (h_name, h_value) in headers.iter_mut() {
                                        if h_name.as_str() == a_name {
                                            if a_value.is_string() {
                                                *h_value = HeaderValue::from_str(
                                                    a_value.as_str().unwrap(),
                                                )?;
                                            } else if a_value.is_object() {
                                                let mut current: HashMap<String, String> =
                                                    match serde_json::from_str(h_value.to_str()?) {
                                                        Ok(val) => val,
                                                        Err(_err) => {
                                                            // Ignore parsing error like in original code for now
                                                            HashMap::new()
                                                        }
                                                    };
                                                let iter = a_value.as_object().unwrap().iter().map(
                                                    |(key, value)| {
                                                        (key.clone(), value.as_str().map(|e| e.to_owned()).unwrap_or_else(|| {
                                                            eprintln!("Could not convert value to string");
                                                            "".to_owned()
                                                        }))
                                                    }
                                                );
                                                current.extend(iter);
                                                let header = serde_json::to_string(&current)?;
                                                *h_value = HeaderValue::from_str(&header)?;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        })
    }

    /**
     * Handles a returning callback
     * This is called for any response comming back from the service.
     * In case of a synchroneous call, it is called directly (within the curl method) on the raw data
     * In case of an asynchroneous call, it is called from the callback thread (started when the instance is spun up via the `establish_callback_subscriptions` in the redis_helper)
     * In the asynchroneous case, we cannot handle any binary data ATM: 18.11.2024
     *
     * Redis callbacks have no response code -> Optional
     *
     * Locks:
     * redis_notification_client
     */
    pub fn handle_callback(
        &mut self,
        status: Option<u16>,
        body: CallbackType,
        options: HashMap<String, String>, // Headers
    ) -> Result<()> {
        let headers = uniformize_headers(&options);
        let weel = self.weel();
        let recv =
            eval_helper::structurize_result(&weel.opts.eval_backend_structurize, &options, body)?;
        let mut redis = weel.redis_notifications_client.lock()?;
        let content = self.construct_basic_content();
        {
            let mut content_node = content.clone();
            let content = content_node
                .as_object_mut()
                .expect("Construct basic content has to return json object");
            content.insert("received".to_owned(), recv.clone());
            if let Some(annotation) = self.annotations.as_ref() {
                content.insert("annotations".to_owned(), annotation.clone());
            }

            redis.notify(
                "activity/receiving",
                Some(content_node),
                weel.get_instance_meta_data(),
            )?;
        }

        if contains_non_empty(&headers, "cpee_instantiation") {
            let mut content_node = content.clone();
            let content = content_node
                .as_object_mut()
                .expect("Construct basic content has to return json object");
            content.insert(
                "received".to_owned(),
                serde_json::from_str(&headers.get("cpee_instantiation").unwrap().clone())?,
            );

            redis.notify(
                "task/instantiation",
                Some(content_node),
                weel.get_instance_meta_data(),
            )?;
        }

        if contains_non_empty(&headers, "cpee_event") {
            let event_regex = match regex::Regex::new(r"[^\w_-]") {
                Ok(regex) => regex,
                Err(err) => {
                    eprintln!("Could not compile static regex: {err} -> SHOULD NOT HAPPEN");
                    panic!()
                }
            };

            // contains_non_empty ensures it it contained
            let event = headers["cpee_event"].clone();
            let event = event_regex.replace_all(&event, "");

            let mut content_node = content.clone();
            let content = content_node
                .as_object_mut()
                .expect("Construct basic content has to return json object");
            content.insert("received".to_owned(), recv.clone());

            redis.notify(
                &format!("task/{event}"),
                Some(content_node),
                weel.get_instance_meta_data(),
            )?;
        } else {
            self.handler_return_status = status;
            self.handler_return_value = Some(recv);
            self.handler_return_options = Some(options);
        }
        println!("Cpee status present: {:?}", contains_non_empty(&headers, "cpee_status"));

        if contains_non_empty(&headers, "cpee_status") {
            let mut content_node = content.clone();
            let content = content_node
                .as_object_mut()
                .expect("Construct basic content has to return json object");
            // CPEE::ValueHelper.parse(options['CPEE_INSTANTIATION'])
            let res = serde_json::Value::String(headers["cpee_status"].to_owned());
            content.insert(
                "status".to_owned(),
                res,
            );
            redis.notify(
                "activity/status",
                Some(content_node),
                weel.get_instance_meta_data(),
            )?;
        }
        drop(redis);

        if contains_non_empty(&headers, "cpee_update") {
            match &self.handler_continue {
                Some(x) => {
                    x.enqueue(Signal::UpdateAgain);
                }
                None => eprintln!("Received CPEE_UPDATE but handler_continue is empty?"),
            }
        } else {
            if let Some(passthrough) = &self.handler_passthrough {
                weel.cancel_callback(passthrough)?;
                self.handler_passthrough = None;
            }
            if contains_non_empty(&headers, "cpee_salvage") {
                match &self.handler_continue {
                    Some(x) => x.enqueue(Signal::Salvage),
                    None => eprintln!("Received CPEE_SALVAGE but handler_continue is empty?"),
                }
            } else if contains_non_empty(&headers, "cpee_stop") {
                match &self.handler_continue {
                    Some(x) => x.enqueue(Signal::Stop),
                    None => eprintln!("Received CPEE_STOP but handler_continue is empty?"),
                }
            } else {
                match &self.handler_continue {
                    Some(x) => x.enqueue(Signal::None),
                    None => {
                        eprintln!("Received neither salvage or stop but handler_continue is empty?")
                    }
                }
            }
        }
        Ok(())
    }

    /**
     * Contains:
     *  - activity-uuid
     *  - label
     *  - activity
     *  - endpoint
     */
    pub fn construct_basic_content(&self) -> Value {
        let position = self
            .handler_position
            .clone()
            .map(|e| e.clone())
            .unwrap_or("".to_owned());
        json!({
            "activity-uuid": self.handler_activity_uuid,
            "label": self.activity_label,
            "activity": position,
            "endpoint": self.handler_endpoints.get(0),
            "ecid": convert_thread_id(thread::current().id())
        })
    }

    fn construct_headers(&self, data: InstanceMetaData, callback_id: &str) -> Result<HeaderMap> {
        let position = self
            .handler_position
            .as_ref()
            .map(|x| x.as_str())
            .unwrap_or("");
        let mut headers = HeaderMap::new();
        headers.append("CPEE-BASE", HeaderValue::from_str(&data.cpee_base_url)?);
        headers.append(
            "CPEE-Instance",
            HeaderValue::from_str(&data.instance_id.to_string())?,
        );
        headers.append(
            "CPEE-Instance-URL",
            HeaderValue::from_str(&data.instance_url)?,
        );
        headers.append(
            "CPEE-Instance-UUID",
            HeaderValue::from_str(&data.instance_uuid)?,
        );
        headers.append(
            "CPEE-CALLBACK",
            HeaderValue::from_str(&format!(
                "{}/callbacks/{}/",
                &data.instance_url, callback_id
            ))?,
        );
        headers.append("CPEE-CALLBACK-ID", HeaderValue::from_str(callback_id)?);
        headers.append("CPEE-ACTIVITY", HeaderValue::from_str(&position)?);
        headers.append("CPEE-LABEL", HeaderValue::from_str(&self.activity_label)?);

        let sim_target = data.attributes.get("sim_target");
        if let Some(sim_target) = sim_target {
            headers.append("CPEE-SIM-TARGET", HeaderValue::from_str(sim_target)?);
        }

        for attribute in data.attributes.iter() {
            let key: String = format!("CPEE-ATTR-{}", attribute.0.replace("_", "-"));
            headers.append(
                HeaderName::from_str(&key)?,
                HeaderValue::from_str(attribute.1)?,
            );
        }
        Ok(headers)
    }

    /**
     * Tries to acquire weel reference, if it is already dropped, we panic
     */
    fn weel(&self) -> Arc<Weel> {
        match self.weel.upgrade() {
            Some(weel) => weel,
            None => {
                eprintln!("Weel instance no longer exists, this connection wrapper instance should have been dropped...");
                panic!()
            }
        }
    }

    fn add_error_information(&self, content: &mut Value, err: Error) {
        let content = content.as_object_mut().unwrap();
        match self.extract_info_from_message(err) {
            Ok((message, line, location)) => {
                content.insert("line".to_owned(), json!(line));
                content.insert("location".to_owned(), json!(location));
                content.insert("message".to_owned(), json!(message));
            }
            Err(message) => {
                content.insert("message".to_owned(), json!(message));
            }
        }
    }

    fn extract_info_from_message(
        &self,
        err: Error,
    ) -> std::result::Result<(String, String, String), String> {
        match err {
            Error::GeneralError(message) => self.try_extract(&message),
            Error::EvalError(eval_error) => match eval_error {
                eval_helper::EvalError::GeneralEvalError(message) => self.try_extract(&message),
                eval_helper::EvalError::SyntaxError(message) => self.try_extract(&message),
                eval_helper::EvalError::RuntimeError(message) => self.try_extract(&message),
                eval_helper::EvalError::Signal(signal, evaluation_result) => {
                    let signal_error = EvalError::Signal(signal, evaluation_result);
                    eprintln!(
                        "Trying to extract information from error: {:?}, this should not happen as Signal Errors should be handled",
                        &signal_error
                    );
                    Err(signal_error.to_string())
                }
            },
            other => {
                eprintln!("Trying to extract information from error: {:?}", other);
                Err(other.to_string().to_owned())
            }
        }
    }

    /**
     * Will try to extract information from the error in a structurized form (OK), otherwise will return the original message (Err)
     */
    fn try_extract(&self, message: &str) -> std::result::Result<(String, String, String), String> {
        match self.error_regex.captures(message) {
            Some(capture) => {
                let message = capture.get(4).unwrap();
                let line = capture.get(3).unwrap();
                let location = capture.get(1).unwrap();
                Ok((
                    message.as_str().to_owned(),
                    line.as_str().to_owned(),
                    location.as_str().to_owned(),
                ))
            }
            None => Err(message.to_owned()),
        }
    }

    pub fn additional(&self) -> Value {
        let weel = self.weel();
        let data = &weel.opts;
        json!(
            {
                "attributes": self.weel().opts.attributes,
                "cpee": {
                    "base": data.cpee_base_url,
                    "instance": data.instance_id,
                    "instance_url": data.instance_url(),
                    "instance_uuid": self.weel().uuid()
                },
                "task": {
                    "label": self.activity_label,
                    "id": self.handler_position
                }
            }
        )
    }

    pub fn split_branches(
        &self,
        id: ThreadId,
        branches: Option<&HashMap<ThreadId, Vec<String>>>,
    ) -> Result<()> {
        let mut content = json!({
            "instance_uuid": self.weel().uuid(),
            "ecid": convert_thread_id(id)
        });

        if let Some(branches) = branches {
            content
                .as_object_mut()
                .unwrap()
                .insert("branches".to_owned(), json!(branches.len()));
        }

        self.inform("gateway/split", Some(content))
    }

    pub fn gateway_decide(&self, id: ThreadId, code: &str, condition: bool) -> Result<()> {
        let content = json!({
            "instance_uuid": self.weel().uuid(),
            "code": code,
            "condition": condition,
            "ecid": convert_thread_id(id)
        });

        self.inform("gateway/decide", Some(content))
    }

    pub fn join_branches(
        &self,
        id: ThreadId,
        branch_traces: Option<&HashMap<ThreadId, Vec<String>>>,
    ) -> Result<()> {
        let mut content = json!({
            "instance_uuid": self.weel().uuid(),
            "ecid": convert_thread_id(id)
        });

        if let Some(branch_traces) = branch_traces {
            content
                .as_object_mut()
                .unwrap()
                .insert("branches".to_owned(), json!(branch_traces.len()));
        }

        self.inform("gateway/join", Some(content))
    }
}

/**
 * Ensures that all headers arriving at the handling code are uniform: are all lower cased and all -'s are subsituted with _'s
 */
fn uniformize_headers(options: &HashMap<String, String>) -> HashMap<String, String> {
    let options = options.clone();
    options
        .iter()
        .map(|(k, v)| {
            let k = k.to_lowercase().replace("-", "_");
            (k, v.clone())
        })
        .collect()
}

pub fn convert_thread_id(thread_id: ThreadId) -> u64 {
    let string_rep = format!("{:?}", thread_id);
    let end = string_rep.replace("ThreadId(", "");
    let end = end.replace(")", "");
    end.parse().unwrap()
}

fn contains_non_empty(options: &HashMap<String, String>, key: &str) -> bool {
    options.get(key).map(|e| !e.is_empty()).unwrap_or(false)
}
