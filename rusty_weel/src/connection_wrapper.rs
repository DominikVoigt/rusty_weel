use http_helper::{header_map_to_hash_map, Parameter};
use rayon::iter::{IntoParallelIterator, ParallelIterator};
use regex::Regex;
use reqwest::{
    header::{HeaderMap, HeaderName, HeaderValue},
    Method,
};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    io::{Read, Seek},
    str::FromStr,
    sync::{Arc, Mutex, Weak},
    thread::sleep,
    time::{Duration, SystemTime},
};
use urlencoding::encode;

use crate::{
    data_types::{BlockingQueue, DynamicData, HTTPParams, InstanceMetaData, KeyValuePair},
    dsl_realization::{generate_random_key, Error, Result, Signal, Weel},
    eval_helper::{self, evaluate_expression, EvalError, EvaluationResult},
};

pub struct ConnectionWrapper {
    weel: Weak<Weel>,
    handler_position: Option<String>,
    // The queue the calling thread is blocking on. -> Uses handle_callback to signal thread to continue running
    pub handler_continue: Option<Arc<crate::data_types::BlockingQueue<Signal>>>,
    // The identifier of the callback the connection wrapper is waiting for (if none, it is not waiting for it)
    pub handler_passthrough: Option<String>,
    pub handler_return_status: Option<u16>,
    pub handler_return_value: Option<String>,
    pub handler_return_options: Option<HashMap<String, String>>,
    // We keep them as arrays to be flexible but will only contain one element for now
    // Contains the actual endpoint URL
    handler_endpoints: Vec<String>,
    // Original endpoint without the twintranslate
    handler_endpoint_origin: Vec<String>,
    // Unique identifier (randomly created)
    pub handler_activity_uuid: String,
    label: String,
    annotations: Option<String>,
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
            label: "".to_owned(),
            handler_endpoint_origin: Vec::new(),
            annotations: None,
            error_regex: Regex::new(r#"(.*?)(, Line |:)(\d+):\s(.*)"#).unwrap(),
        }
    }

    /**
     * If too many request are issued to an address by the same wrapper, will throttle these requests
     */
    pub fn loop_guard(&self, id: String) {
        let attributes = &self.weel().attributes;
        let loop_guard_attribute = attributes.get("nednoamol");
        if loop_guard_attribute.is_some_and(|attrib| attrib == "true") {
            return;
        }
        match self.weel().loop_guard.lock().as_mut() {
            Ok(map) => {
                let last = map.get(&id);
                let should_throttle = match last {
                    // Some: loop guard was hite prior to this -> increase count
                    Some(entry) => {
                        let count = entry.0 + 1;
                        let last_call_time = entry.1;
                        map.insert(id, (count, SystemTime::now()));

                        let last_call_too_close = last_call_time
                            .elapsed()
                            .expect("last call is in the future")
                            .as_secs_f32()
                            < LOOP_GUARD_DELTA;
                        let threshold_passed = count > UNGUARDED_CALLS;
                        last_call_too_close && threshold_passed
                    }
                    None => {
                        map.insert(id, (1, SystemTime::now()));
                        false
                    }
                };
                if should_throttle {
                    sleep(Duration::from_secs(SLEEP_DURATION));
                }
            }
            Err(err) => {
                log::error!("Could not acquire lock {err}");
                panic!("Could not acquire lock in loopguard")
            }
        };
    }

    pub fn inform_state_change(&self, new_state: &str) -> Result<()> {
        let mut content = HashMap::new();
        content.insert("state".to_owned(), new_state.to_owned());

        self.inform("state/change", Some(content))
    }

    pub fn inform_syntax_error(&self, err: Error, _code: Option<&str>) -> Result<()> {
        let mut content = HashMap::new();
        self.add_error_information(&mut content, err);

        self.inform("description/error", Some(content))
    }

    pub fn inform_connectionwrapper_error(&self, err: Error) -> Result<()> {
        let mut content = HashMap::new();
        self.add_error_information(&mut content, err);

        self.inform("executionhandler/error", Some(content))
    }

    pub fn inform_position_change(&self, ipc: Option<HashMap<String, String>>) -> Result<()> {
        self.inform("position/change", ipc)
    }

    pub fn inform_activity_manipulate(&self) -> Result<()> {
        let mut content = self.construct_basic_content()?;
        content.insert("label".to_owned(), self.label.clone());
        self.inform("activity/manipulating", Some(content))
    }

    pub fn inform_activity_done(&self) -> Result<()> {
        let content = self.construct_basic_content()?;
        self.inform("activity/done", Some(content))?;
        self.inform_resource_utilization()
    }

    pub fn inform_activity_failed(&self, err: Error) -> Result<()> {
        let mut content: HashMap<String, String> = self.construct_basic_content()?;
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

        self.inform("status/resource_utilization", Some(content))?;
        Ok(())
    }

    pub fn inform_manipulate_change(
        &self,
        evaluation_result: eval_helper::EvaluationResult,
    ) -> Result<()> {
        let content = self.construct_basic_content()?;
        if let Some(changed_status) = evaluation_result.changed_status {
            let mut content = content.clone();
            content.insert("id".to_owned(), changed_status.id.to_string());
            content.insert("message".to_owned(), changed_status.message);
            self.inform("status/change", Some(content))?;
        }
        if let Some(changed_data) = evaluation_result.changed_data {
            let mut content = content.clone();
            // TODO For us, we need the direct pairs of changed data: (name, new value)
            // In the ManipulateStructure implementation, the values where changed directly via the instance_eval => Changed data/endpoints just contained the names
            content.insert("changed".to_owned(), serde_json::to_string(&changed_data)?);
            self.inform("dataelements/change", Some(content))?;
        }
        if let Some(changed_endpoints) = evaluation_result.changed_endpoints {
            let mut content = content.clone();
            content.insert(
                "changed".to_owned(),
                serde_json::to_string(&changed_endpoints)?,
            );
            self.inform("endpoints/change", Some(content))?;
        }

        todo!()
    }

    /*
     * Locks:
     *  - Locks the redis_notification_client (shortly)
     */
    fn inform(&self, what: &str, content: Option<HashMap<String, String>>) -> Result<()> {
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
        thread_local: String,
        endpoint_names: &Vec<&str>,
        parameters: HTTPParams,
    ) -> Result<HTTPParams> {
        let weel = self.weel();
        // Execute the prepare code and use the modified context for the rest of this metod (prepare_result)
        let prepare_result = match prepare_code {
            Some(code) => {
                let result = weel.execute_code(true, code, &thread_local, self, "prepare")?;
                result.into()
            }
            None => {
                let dynamic_data = weel.context.lock().unwrap();
                LocalData {
                    data: dynamic_data.data.clone(),
                    endpoints: dynamic_data.endpoints.clone(),
                }
            }
        };

        // Resolve the endpoint name to the actual correct endpoint (incl. twin_translate)
        if endpoint_names.len() > 0 {
            self.resolve_endpoints(&prepare_result.endpoints, endpoint_names);

            match weel.attributes.get("twin_engine") {
                Some(twin_engine_url) => {
                    if !twin_engine_url.is_empty() {
                        self.handler_endpoint_origin = self.handler_endpoints.clone();

                        let endpoint = encode(self.handler_endpoints.get(0).expect(""));
                        self.handler_endpoints = vec![format!(
                            "{}?original_endpoint={}",
                            twin_engine_url, endpoint
                        )];
                    }
                }
                None => {
                    // Do nothing with the endpoints
                }
            };
        };
        let dyn_data = DynamicData {
            endpoints: prepare_result.endpoints,
            data: prepare_result.data,
        };

        if let Some(arguments) = parameters.arguments {
            let error: Mutex<Option<Error>> = Mutex::new(None);
            // Only translate arguments that are expressions and that have actual expressions in them (expression_value should imply value is not empty)
            let mapped_arguments: Vec<KeyValuePair> = arguments
                .into_par_iter()
                .filter(|argument| argument.expression_value && argument.value.is_some())
                .filter_map(|argument| {
                    if argument.expression_value {
                        if let Some(value) = argument.value.as_ref() {
                            let eval_result = match evaluate_expression(
                                &dyn_data,
                                &weel.opts,
                                value,
                                None,
                                &thread_local,
                                self.additional(),
                                None,
                                None,
                                "prepare",
                            ) {
                                Ok(result) => Some(result),
                                Err(err) => {
                                    log::error!(
                                        "Failure evaluating argument expressions in prepare due to: {:?}", err                                
                                    );
                                    *error.lock().unwrap() = Some(err);
                                    None
                                }
                            };
                            let evaluated_expression = eval_result.map(|eval_result| {
                                eval_result.expression_result
                            });
                            Some(KeyValuePair {
                                key: argument.key,
                                value: evaluated_expression,
                                expression_value: false,
                            })
                        } else { // Expression but empty -> Empty value
                            Some(KeyValuePair {
                                key: argument.key,
                                value: None,
                                expression_value: false,
                            })
                        }
                    } else {
                        Some(argument.clone())
                    }
                })
                .collect();
            let error = error.lock().unwrap().take();
            if let Some(err) = error {
                return Err(err);
            }

            Ok(HTTPParams {
                arguments: Some(mapped_arguments),
                ..parameters
            })
        } else {
            Ok(parameters.clone())
        }
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
        self.label = label.to_owned();
    }

    /**
     * Will cancel an activity via the redis_helper thread
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
        parameters: HTTPParams,
    ) -> Result<()> {
        let mut this = selfy.lock()?;
        let weel = this.weel();

        if this.handler_endpoints.is_empty() {
            return Err(Error::GeneralError("Wrong endpoint".to_owned()));
        }
        this.label = parameters.label.to_owned();
        // We do not model annotations anyway -> Can skip this from the original code
        {
            this.inform_resource_utilization()?;
            let mut content = this.construct_basic_content()?;
            content.insert("label".to_owned(), this.label.clone());
            content.insert(
                "passthrough".to_owned(),
                passthrough.unwrap_or("").to_owned(),
            );
            // parameters do not look exactly like in the original (string representation looks different):
            content.insert("parameters".to_owned(), parameters.clone().try_into()?);
            weel.redis_notifications_client.lock()?.notify(
                "activity/calling",
                Some(content),
                weel.get_instance_meta_data(),
            )?
        }
        match passthrough {
            Some(passthrough) => {
                let mut content = this.construct_basic_content()?;
                content.insert("label".to_owned(), this.label.clone());
                content.remove("endpoint");
                weel.register_callback(selfy.clone(), passthrough, content)?;
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

        let protocol_regex = match regex::Regex::new(r"^http(s)?-(get|put|post|delete):") {
            Ok(regex) => regex,
            Err(err) => {
                log::error!("Could not compile static regex: {err} -> SHOULD NOT HAPPEN");
                panic!()
            }
        };
        let event_regex = match regex::Regex::new(r"[^\w_-]") {
            Ok(regex) => regex,
            Err(err) => {
                log::error!("Could not compile static regex: {err} -> SHOULD NOT HAPPEN");
                panic!()
            }
        };

        loop {
            // Compute parameters
            let mut params = Vec::new();
            // Params could contain file handles (complex parameters) and thus cannot be cloned -> We cannot clone so we recompute them here
            match parameters.arguments.as_ref() {
                Some(args) => args.iter().for_each(|arg| {
                    let value = arg.value.clone().unwrap_or("".to_owned());
                    params.push(http_helper::Parameter::SimpleParameter {
                        name: arg.key.to_owned(),
                        value,
                        param_type: http_helper::ParameterType::Body,
                    });
                }),
                None => {
                    log::info!("Arguments provided to protocurl are empty");
                }
            };

            let mut content_json = HashMap::new();
            content_json.insert(
                "activity_uuid".to_owned(),
                this.handler_activity_uuid.clone(),
            );
            content_json.insert("label".to_owned(), this.label.clone());
            let position = this
                .handler_position
                .as_ref()
                .map(|x| x.clone())
                .unwrap_or("".to_owned());
            content_json.insert("activity".to_owned(), position);

            weel.register_callback(Arc::clone(selfy), &callback_id, content_json)?;
            let endpoint = match this.handler_endpoints.get(0) {
                // TODO: Set method by matched method in url
                Some(endpoint) => protocol_regex.replace_all(&endpoint, r"http\\1:"),
                None => {
                    return Err(Error::GeneralError(
                        "No endpoint for curl configured.".to_owned(),
                    ))
                }
            };
            let mut client = http_helper::Client::new(&endpoint, parameters.method.clone())?;
            client.set_request_headers(headers.clone());
            client.add_parameters(params);

            let response = client.execute_raw()?;
            status = response.status_code;
            response_headers = header_map_to_hash_map(&response.headers)?;
            body = response.body;

            if status == 561 {
                match weel.attributes.get("twin_translate") {
                    Some(twin_translate) => {
                        Self::handle_twin_translate(twin_translate, &mut headers, &mut this)?;
                    }
                    None => this.handler_endpoints = this.handler_endpoint_origin.clone(),
                }
                headers.remove("original_endpoint");
            } else {
                // equivalent to do-while status == 561 in original code
                break;
            }
        }

        // If status not okay:
        if status < 200 || status >= 300 {
            response_headers.insert("CPEE_SALVAGE".to_owned(), "true".to_owned());
            this.handle_callback(Some(status), &body, response_headers)?
        } else {
            let callback_header_set = match response_headers.get("CPEE_CALLBACK") {
                Some(header) => header == "true",
                None => false,
            };

            if callback_header_set {
                if !body.len() > 0 {
                    response_headers.insert("CPEE_UPDATE".to_owned(), "true".to_owned());
                    this.handle_callback(Some(status), &body, response_headers)?
                } else {
                    // In this case we have an asynchroneous task
                    let mut content = HashMap::new();
                    {
                        content.insert(
                            "activity_uuid".to_owned(),
                            this.handler_activity_uuid.clone(),
                        );
                        content.insert("label".to_owned(), this.label.clone());
                        content.insert(
                            "activity".to_owned(),
                            this.handler_position.clone().unwrap_or("".to_owned()),
                        );
                        content.insert(
                            "endpoint".to_owned(),
                            serde_json::to_string(&this.handler_endpoints)?,
                        );
                    }

                    let instantiation_header_set = match response_headers.get("CPEE_INSTANTION") {
                        Some(instantiation_header) => !instantiation_header.is_empty(),
                        None => false,
                    };

                    if instantiation_header_set {
                        // TODO What about value_helper
                        content.insert(
                            "received".to_owned(),
                            response_headers.get("CPEE_INSTANTIATION").unwrap().clone(),
                        );
                        weel.redis_notifications_client.lock().unwrap().notify(
                            "task/instantiation",
                            Some(content.clone()),
                            weel.get_instance_meta_data(),
                        )?;
                    }

                    let event_header_set = match response_headers.get("CPEE_EVENT") {
                        Some(event_header) => !event_header.is_empty(),
                        None => false,
                    };
                    if event_header_set {
                        // TODO What about value_helper
                        let event = response_headers.get("CPEE_EVENT").unwrap();
                        let event = event_regex.replace_all(event, "");
                        let what = format!("task/{event}");
                        weel.redis_notifications_client.lock().unwrap().notify(
                            &what,
                            Some(content),
                            weel.get_instance_meta_data(),
                        )?;
                    }
                }
            } else {
                this.handle_callback(Some(status), &body, response_headers)?
            }
        }
        Ok(())
    }

    fn handle_twin_translate(
        twin_translate_url: &String,
        headers: &mut HeaderMap,
        this: &mut std::sync::MutexGuard<ConnectionWrapper>,
    ) -> Result<()> {
        let client = http_helper::Client::new(&twin_translate_url, Method::GET)?;
        let result = client.execute()?;
        let status = result.status_code;
        let result_headers = result.headers;
        let mut content = result.content;
        Ok(if status >= 200 && status < 300 {
            let translation_type = match headers.get("CPEE-TWIN-TASKTYPE") {
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

            // TODO: Very unsure about the semantics of the original code and the usage
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
                                                            log::error!("Could not convert value to string");
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
     * In case of a synchroneous call, it is called directly (within the curl method)
     * In case of an asynchroneous call, it is called from the callback thread (started when the instance is spun up via the `establish_callback_subscriptions` in the redis_helper)
     * Redis callbacks have no response code -> Optional
     */
    pub fn handle_callback(
        &mut self,
        status: Option<u16>,
        body: &[u8],
        options: HashMap<String, String>, // Headers
    ) -> Result<()> {
        let weel = self.weel();
        let recv =
            eval_helper::structurize_result(&weel.opts.eval_backend_structurize, &options, body)?;
        let mut redis = weel.redis_notifications_client.lock()?;
        let content = self.construct_basic_content()?;
        {
            let mut content = content.clone();
            content.insert("received".to_owned(), recv.clone());
            content.insert(
                "annotations".to_owned(),
                self.annotations.clone().unwrap_or("".to_owned()),
            );

            redis.notify(
                "activity/receiving",
                Some(content),
                weel.get_instance_meta_data(),
            )?;
        }

        if contains_non_empty(&options, "CPEE_INSTANTIATION") {
            let mut content = content.clone();
            content.insert(
                "received".to_owned(),
                options.get("CPEE_INSTANTIATION").unwrap().clone(),
            );

            redis.notify(
                "activity/receiving",
                Some(content),
                weel.get_instance_meta_data(),
            )?;
        }

        if contains_non_empty(&options, "CPEE_EVENT") {
            let event_regex = match regex::Regex::new(r"[^\w_-]") {
                Ok(regex) => regex,
                Err(err) => {
                    log::error!("Could not compile static regex: {err} -> SHOULD NOT HAPPEN");
                    panic!()
                }
            };

            // contains_non_empty ensures it it contained
            let event = options["CPEE_EVENT"].clone();
            let event = event_regex.replace_all(&event, "");

            let mut content = content.clone();
            content.insert("received".to_owned(), recv.clone());

            redis.notify(
                &format!("task/{event}"),
                Some(content),
                weel.get_instance_meta_data(),
            )?;
        } else {
            self.handler_return_status = status;
            self.handler_return_value = Some(recv);
            self.handler_return_options = Some(options.clone());
        }

        if contains_non_empty(&options, "CPEE_STATUS") {
            let mut content = content.clone();
            // CPEE::ValueHelper.parse(options['CPEE_INSTANTIATION'])
            content.insert("status".to_owned(), options["CPEE_STATUS"].clone());
        }
        if contains_non_empty(&options, "CPEE_UPDATE") {
            match &self.handler_continue {
                Some(x) => x.enqueue(Signal::UpdateAgain),
                None => log::error!("Received CPEE_UPDATE but handler_continue is empty?"),
            }
        } else {
            if let Some(passthrough) = &self.handler_passthrough {
                weel.cancel_callback(passthrough)?;
                self.handler_passthrough = None;
            }
            if contains_non_empty(&options, "CPEE_SALVAGE") {
                match &self.handler_continue {
                    Some(x) => x.enqueue(Signal::Salvage),
                    None => log::error!("Received CPEE_SALVAGE but handler_continue is empty?"),
                }
            } else if contains_non_empty(&options, "CPEE_STOP") {
                match &self.handler_continue {
                    Some(x) => x.enqueue(Signal::Stop),
                    None => log::error!("Received CPEE_STOP but handler_continue is empty?"),
                }
            } else {
                match &self.handler_continue {
                    Some(x) => x.enqueue(Signal::None),
                    None => log::error!(
                        "Received neither salvage or stop but handler_continue is empty?"
                    ),
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
    pub fn construct_basic_content(&self) -> Result<HashMap<String, String>> {
        let mut content = HashMap::new();
        content.insert(
            "activity-uuid".to_owned(),
            self.handler_activity_uuid.clone(),
        );
        content.insert("label".to_owned(), self.label.clone());
        content.insert(
            "activity".to_owned(),
            self.handler_position
                .clone()
                .map(|e| e.clone())
                .unwrap_or("".to_owned()),
        );
        content.insert(
            "endpoint".to_owned(),
            serde_json::to_string(&self.handler_endpoints)?,
        );
        Ok(content)
    }

    fn construct_headers(&self, data: InstanceMetaData, callback_id: &str) -> Result<HeaderMap> {
        let position = self
            .handler_position
            .as_ref()
            .map(|x| x.as_str())
            .unwrap_or("");
        let mut headers = HeaderMap::new();
        headers.append("CPEE-BASE", HeaderValue::from_str(&data.cpee_base_url)?);
        headers.append("CPEE-Instance", HeaderValue::from_str(&data.instance_id)?);
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
        headers.append("CPEE-LABEL", HeaderValue::from_str(&self.label)?);

        let twin_target = data.attributes.get("twin_target");
        if let Some(twin_target) = twin_target {
            headers.append("CPEE-TWIN-TARGET", HeaderValue::from_str(twin_target)?);
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
                log::error!("Weel instance no longer exists, this connection wrapper instance should have been dropped...");
                panic!()
            }
        }
    }

    pub fn activity_no_longer_necessary(&self) -> bool {
        true
    }

    fn add_error_information(&self, content: &mut HashMap<String, String>, err: Error) {
        match self.extract_info_from_message(err) {
            Ok((message, line, location)) => {
                content.insert("line".to_owned(), line);
                content.insert("location".to_owned(), location);
                content.insert("message".to_owned(), message);
            }
            Err(message) => {
                content.insert("message".to_owned(), message);
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
                    log::error!(
                        "Trying to extract information from error: {:?}",
                        EvalError::Signal(signal, evaluation_result)
                    );
                    Err("".to_owned())
                }
            },
            other => {
                log::error!("Trying to extract information from error: {:?}", other);
                Err("".to_owned())
            }
        }
    }

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
            None => {
                log::info!("Capture of regex did not work for message: {message}");
                Err(message.to_owned())
            }
        }
    }

    pub fn additional(&self) -> Value {
        let weel = self.weel();
        let data = &weel.opts;
        json!(
            {
                "attributes": self.weel().attributes,
                "cpee": {
                    "base": data.cpee_base_url,
                    "instance": data.instance_id,
                    "instance_url": data.instance_url(),
                    "instance_uuid": self.weel().uuid()
                },
                "task": {
                    "label": self.label,
                    "id": self.handler_position
                }
            }
        )
    }
}

fn contains_non_empty(options: &HashMap<String, String>, key: &str) -> bool {
    options.get(key).map(|e| !e.is_empty()).unwrap_or(false)
}

impl Into<LocalData> for EvaluationResult {
    fn into(self) -> LocalData {
        LocalData {
            data: self.data,
            endpoints: self.endpoints,
        }
    }
}

/*
 * Contains either:
 *  - Snapshot of the weel instance dynamic data and the thread_local information
 *  - Modified version of the snapshot after execution of some provided code (see prepare function in the conection wrapper)
 */
pub struct LocalData {
    pub data: String,
    pub endpoints: HashMap<String, String>,
}
