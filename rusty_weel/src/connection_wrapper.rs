use base64::Engine;
use http_helper::{header_map_to_hash_map, Mime, Parameter};
use reqwest::{
    header::{HeaderMap, HeaderName, HeaderValue},
    Method,
};
use serde::Serialize;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    io::{Read, Seek},
    str::FromStr,
    sync::{Arc, Mutex, MutexGuard, Weak},
    thread::sleep,
    time::{Duration, SystemTime},
};
use urlencoding::encode;

use crate::{
    data_types::{BlockingQueue, HTTPParams, InstanceMetaData},
    dsl_realization::{generate_random_key, Error, Result, Signal, Weel}, eval_helper,
};

// Expected to be guarded with mutex to sensure that method invocations do not deadlock
// Deadlocks can still occur since weel is not mutex locked
pub struct ConnectionWrapper {
    weel: Weak<Weel>,
    handler_position: Option<String>,
    // Continue object for thread synchronization -> TODO: See whether we need this/how we implement this
    handler_continue: Option<Arc<crate::data_types::BlockingQueue<Signal>>>,
    // See proto_curl in connection.rb
    handler_passthrough: Option<String>,
    // TODO: Unsure about this type:
    handler_return_value: Option<String>,
    // TODO: Determine this type:
    handler_return_options: Option<HashMap<String, String>>,
    // We keep them as arrays to be flexible but will only contain one element for now
    // Contains the actual endpoint URL
    handler_endpoints: Vec<String>,
    // Original endpoint without the twintranslate
    handler_endpoint_origin: Vec<String>,
    handler_activity_uuid: String,
    label: String,
    annotations: Option<String>,
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
        handler_position: Option<String>,
        handler_continue: Option<Arc<BlockingQueue<Signal>>>,
    ) -> Self {
        let weel = Arc::downgrade(&weel);
        ConnectionWrapper {
            weel,
            handler_position,
            handler_continue,
            handler_passthrough: None,
            handler_return_value: None,
            handler_return_options: None,
            handler_endpoints: Vec::new(),
            handler_activity_uuid: generate_random_key(),
            label: "".to_owned(),
            handler_endpoint_origin: Vec::new(),
            annotations: None,
        }
    }

    pub fn loop_guard(&self, id: String) {
        let attributes = &self.weel().static_data.attributes;
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

    pub fn inform_syntax_error(&self, err: Error, code: Option<&str>) -> Result<()> {
        let mut content = HashMap::new();
        // TODO: mess = err.backtrace ? err.backtrace[0].gsub(/([\w -_]+):(\d+):in.*/,'\\1, Line \2: ') : ''
        content.insert("message".to_owned(), err.as_str().to_owned());

        self.inform("description/error", Some(content))
    }

    pub fn inform_connectionwrapper_error(&self, err: Error) -> Result<()> {
        let mut content = HashMap::new();
        // TODO: mess = err.backtrace ? err.backtrace[0].gsub(/([\w -_]+):(\d+):in.*/,'\\1, Line \2: ') : ''
        content.insert("message".to_owned(), err.as_str().to_owned());

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

    fn inform(&self, what: &str, content: Option<HashMap<String, String>>) -> Result<()> {
        let weel = self.weel();
        weel.redis_notifications_client
            .lock()
            .expect("Could not acquire mutex")
            .notify(what, content, weel.static_data.get_instance_meta_data())?;
        Ok(())
    }

    /**
     * Resolves the endpoints to their actual URLs
     */
    pub fn prepare(&mut self, endpoints: &Vec<String>, parameters: &HTTPParams) -> HTTPParams {
        if endpoints.len() > 0 {
            let weel = self.weel();
            self.resolve_endpoints(endpoints, &weel);

            match weel.static_data.attributes.get("twin_engine") {
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
            }
        }
        parameters.clone()
    }

    /**
     * Resolves the endpoint names in endpoints to the actual endpoint URLs
     */
    fn resolve_endpoints(&mut self, endpoints: &Vec<String>, weel: &Arc<Weel>) {
        let weel_endpoint_urls = weel.dynamic_data.lock().unwrap(); 
        self.handler_endpoints = endpoints
            .iter()
            .map(|ep| weel_endpoint_urls.endpoints.get(ep))
            .filter_map(|item| match item {
                Some(item) => Some(item.clone()),
                None => None,
            })
            .collect();
    }

    pub fn additional(&self) -> Value {
        let weel = self.weel();
        let data = &weel.static_data;
        json!(
            {
                "attributes": weel.static_data.attributes,
                "cpee": {
                    "base": data.base_url,
                    "instance": data.instance_id,
                    "instance_url": data.instance_url(),
                    "instance_uuid": data.uuid()
                },
                "task": {
                    "label": self.label,
                    "id": self.handler_position
                }
            }
        )
    }

    pub fn activity_result_options(&self) -> Option<HashMap<String, String>> {
        self.handler_return_options.clone()
    }

    pub fn activity_result_value(&self) -> Option<String> {
        self.handler_return_value.clone()
    }

    pub fn activity_passthrough_value(&self) -> Option<String> {
        self.handler_passthrough.clone()
    }

    pub fn activity_manipulate_handle(&mut self, label: &str) {
        self.label = label.to_owned();
    }

    pub fn activity_stop(&self) {
        if let Some(passthrough) = &self.handler_passthrough {
            self.weel().cancel_callback(passthrough);
        }
    }

    pub fn activity_handle(
        selfy: &Arc<Mutex<Self>>,
        passthrough: &str,
        parameters: &HTTPParams,
    ) -> Result<()> {
        let mut this = selfy.lock()?;
        let weel = this.weel();

        if this.handler_endpoints.is_empty() {
            return Err(Error::GeneralError("Wrong endpoint".to_owned()));
        }
        this.label = parameters.label.to_owned();
        // We do not model annotations anyway -> Can skip this from the original code
        {
            let mut redis = weel.redis_notifications_client.lock()?;
            // TODO: Resource utilization seems to be non-straight forward, especially Memory usage
            // redis.notify("status/resource_utilization", content, instace_meta_data)
            let mut content = this.construct_basic_content()?;
            content.insert("label".to_owned(), this.label.clone());
            content.insert("passthrough".to_owned(), passthrough.to_owned());
            // parameters do not look exactly like in the original (string representation looks different):
            content.insert("parameters".to_owned(), parameters.clone().try_into()?);
            redis.notify(
                "activity/calling",
                Some(content),
                weel.static_data.get_instance_meta_data(),
            )?
        }
        if passthrough.is_empty() {
            Self::curl(this, selfy, parameters, weel)?;
        } else {
            let mut content = this.construct_basic_content()?;
            content.insert("label".to_owned(), this.label.clone());
            content.remove("endpoint");
            weel.callback(selfy.clone(), passthrough, content)?;
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
     */
    pub fn curl(
        mut this: MutexGuard<ConnectionWrapper>,
        selfy: &Arc<Mutex<Self>>,
        parameters: &HTTPParams,
        weel: Arc<Weel>,
    ) -> Result<()> {
        let callback_id = generate_random_key();
        this.handler_passthrough = Some(callback_id.clone());

        // Generate headers
        let mut headers: HeaderMap =
            this.construct_headers(weel.static_data.get_instance_meta_data(), &callback_id)?;

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

            weel.callback(Arc::clone(selfy), &callback_id, content_json)?;
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
                match weel.static_data.attributes.get("twin_translate") {
                    Some(twin_translate) => {
                        handle_twin_translate(twin_translate, &mut headers, &mut this)?;
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
            this.callback(
                &body,
                response_headers
            )?
        } else {
            let callback_header_set = match response_headers.get("CPEE_CALLBACK") {
                Some(header) => header == "true",
                None => false,
            };

            if callback_header_set {
                if !body.len() > 0 {
                    response_headers.insert("CPEE_UPDATE".to_owned(), "true".to_owned());
                    this.callback(&body, response_headers)?
                } else {
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
                        content.insert("received".to_owned(), response_headers.get("CPEE_INSTANTIATION").unwrap().clone());
                        weel.redis_notifications_client.lock().unwrap().notify(
                            "task/instantiation",
                            Some(content.clone()),
                            weel.static_data.get_instance_meta_data(),
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
                            weel.static_data.get_instance_meta_data(),
                        )?;
                    }
                }
            } else {
                this.callback(&body, response_headers)?
            }
        }
        Ok(())
    }

    pub fn callback(
        &mut self,
        body: &[u8],
        options: HashMap<String, String>, // Headers
    ) -> Result<()> {
        let weel = self.weel();
        let recv = eval_helper::structurize_result(&weel.static_data.eval_backend_url, &options, body)?;
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
                weel.static_data.get_instance_meta_data(),
            )?;
        }

        if contains_non_empty(&options, "CPEE_INSTANTIATION") {
            let mut content = content.clone();
            // TODO: Unsure whether this is equivalent to: CPEE::ValueHelper.parse(options['CPEE_INSTANTIATION'])
            content.insert("received".to_owned(), options.get("CPEE_INSTANTIATION").unwrap().clone());

            redis.notify(
                "activity/receiving",
                Some(content),
                weel.static_data.get_instance_meta_data(),
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
                weel.static_data.get_instance_meta_data(),
            )?;
        } else {
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
                Some(x) => x.enqueue(Signal::Again),
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
                    None => log::error!("Received neither salvage or stop but handler_continue is empty?"),
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
                // Todo: What should we do here?
                panic!()
            }
        }
    }
}

fn contains_non_empty(options: &HashMap<String, String>, key: &str) -> bool {
    options.get(key).map(|e| !e.is_empty()).unwrap_or(false)
}

/**
 * Reformats the resulting parameters into a string representation
 */
fn simplify_result(parameters: &mut Vec<Parameter>) -> Result<String> {
    let mut result = Vec::with_capacity(parameters.len());
    for parameter in parameters {
        let element = match parameter {
            Parameter::SimpleParameter { value, .. } => value.clone(),
            Parameter::ComplexParameter { content_handle, .. } => {
                let mut buf = String::new();
                content_handle.read_to_string(&mut buf)?;
                content_handle.rewind()?;
                buf
            }
        };
        result.push(element);
    }
    if result.is_empty() {
        Ok("".to_owned())
    } else if result.len() == 1 {
        Ok(result.pop().unwrap())
    } else {
        let result = result.join(", ");
        Ok(format!("[{result}]"))
    }
}

#[derive(Serialize)]
struct StructuredResultElement {
    name: String,
    data: String,
    mime_type: Option<String>,
}

/**
 * Reformats the result into a simple DTO form.
 * All file handles are read and content saved into the DTO
 * TODO: Determine whether this has to be the exact same as the ruby impl
 */
fn structurize_result(parameters: &mut Vec<Parameter>) -> Result<Vec<StructuredResultElement>> {
    todo!();
    let mut result = Vec::new();
    for parameter in parameters.iter_mut() {
        match parameter {
            Parameter::SimpleParameter { name, value, .. } => {
                result.push(StructuredResultElement {
                    name: name.clone(),
                    data: value.clone(),
                    mime_type: None,
                })
            }
            Parameter::ComplexParameter {
                name,
                mime_type,
                content_handle,
            } => {
                let mut data: Vec<u8> = Vec::new();
                content_handle.read_to_end(&mut data)?;
                content_handle.rewind()?;
                // Use the provided mime type
                let data = convert_to_utf8(mime_type.clone(), data);
                result.push(StructuredResultElement {
                    name: name.clone(),
                    mime_type: Some(mime_type.to_string().clone()),
                    data,
                })
            }
        }
    }
    Ok(result)
}

/**
 * Will convert the given data into a UTF-8 string.
 * If the data is binary (based on mime type) the data is base64 encoded -> In a string with meta data
 * If the data is text data and a charset is provided, conversion happens based on that.
 * If the data is text data and the charset is not provided, it is attempted to be detected.
 *  If detection confidence is sufficient, conversion is done. Otherwise the data is treated as binary
 */
fn convert_to_utf8(mime_type: Mime, data: Vec<u8>) -> String {
    // If no charset encoding was provided via the mimetype, try utf 8 and otherwise try some detection...
    match String::from_utf8(data.clone()) {
        Ok(string) => string,
        Err(err) => {
            log::error!(
                "data seems not to be UTF-8 encoded: {:?}, try detecting encoding...",
                err
            );
            let encoding = detect_encoding(&data);
            let confidence = encoding.1;
            let decode_result = match encoding_rs::Encoding::for_label(encoding.0.as_bytes()) {
                Some(x) => {
                    if confidence > 30 {
                        x.decode(&data).0.into_owned()
                    } else {
                        convert_to_base64(mime::APPLICATION_OCTET_STREAM, data)
                    }
                }
                // If the detected encoding is not identifiable by the decode library, emit error and decode as UTF-8 anyway
                None => {
                    log::error!(
                        "Encoding could not be found by the provided label: {}",
                        encoding.0
                    );
                    convert_to_base64(mime::APPLICATION_OCTET_STREAM, data)
                }
            };
            decode_result
            // TODO: What to do with this Hash part????
        }
    }
}

// TODO: Pretty sure this should work
fn convert_to_base64(mime_type: Mime, data: Vec<u8>) -> String {
    match infer::get(&data) {
        Some(mime_type) => {
            format!(
                "data:{};base64,{}",
                mime_type.mime_type(),
                base64::prelude::BASE64_STANDARD.encode(data)
            )
        }
        None => {
            format!(
                "data:application/octet-stream;base64,{}",
                base64::prelude::BASE64_STANDARD.encode(data)
            )
        }
    }
}

// TODO: Very unsure about this
/**
 * Returns the charset and a confidence
 */
fn detect_encoding(data: &[u8]) -> (String, i32) {
    todo!()
    // TODO: I believe this is no longer required with the eval service
    /*
    let mut detector = match CharsetDetector::new() {
        Ok(x) => x,
        Err(err) => {
            log::error!("Could not initialize charset detector: {:?}", err);
            return ("OTHER".to_owned(), 0);
        }
    };
    match detector.set_text(data) {
        Ok(_) => {}
        Err(err) => {
            log::error!("Error setting text: {err}");
            panic!()
        }
    }
    match detector.detect() {
        Ok(encoding) => (
            match encoding.name() {
                Ok(name) => name.to_owned(),
                Err(err) => {
                    log::error!("Error detecting encoding: {err}");
                    "OTHER".to_owned()
                }
            },
            match encoding.confidence() {
                Ok(conf) => conf,
                Err(err) => {
                    log::error!("Error getting confidence: {err}");
                    0
                }
            },
        ),
        Err(err) => {
            log::error!("Could not detect encoding: {:?}", err);
            ("OTHER".to_owned(), 0)
        }
    }
     */
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
                                    let header_name = a_value.as_str().unwrap().replace("-", "_");
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
                                            *h_value =
                                                HeaderValue::from_str(a_value.as_str().unwrap())?;
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

mod test {
    use std::fs;

    use crate::connection_wrapper::{convert_to_utf8, detect_encoding};

    #[test]
    fn test_pattern() {
        let regex = regex::Regex::new(r"^http(s)?-(get|put|post|delete):/").unwrap();
        let replacement = r"http\\1:";
        assert_eq!(
            regex.replace_all("http-get://test.com", replacement),
            r"http\\1:test.com"
        );
    }

    #[test]
    fn test_negation_pattern() {
        let regex = regex::Regex::new(r"[^\w_-]").unwrap();
        let replacement = r"";
        assert_eq!(
            regex.replace_all("my_name-is_testcase  1", replacement),
            r"my_name-is_testcase1"
        );
    }

    #[test]
    fn test_detect() {
        // Detect the most relevant web: https://w3techs.com/technologies/overview/character_encoding
        let data = fs::read("./test_files/utf8.txt").unwrap();
        let encoding = detect_encoding(&data);
        assert_eq!("UTF-8", encoding.0);
        println!(
            "Detected encoding: {} Confidence: {}",
            encoding.0, encoding.1
        );

        let result = convert_to_utf8(mime::TEXT_PLAIN, data);
        println!("{}", result);
        assert_eq!(
            indoc::indoc! {r#"première is first
                    première is slightly different
                    Кириллица is Cyrillic
                    𐐀 am Deseret
                    "#},
            result
        );

        let data = fs::read("./test_files/latin1.txt").unwrap();
        let encoding = detect_encoding(&data);
        println!(
            "Detected encoding: {} Confidence: {}",
            encoding.0, encoding.1
        );
        assert_eq!("ISO-8859-1", encoding.0);

        let result = convert_to_utf8(mime::TEXT_PLAIN, data);
        println!("{}", result);
        assert_eq!(
            indoc::indoc! {r#"première is first
                    premie?re is slightly different
                    ????????? is Cyrillic
                    ?? am Deseret
                    "#},
            result
        );
    }

    #[test]
    fn test_detection_binary() {
        // ELF file (binary)
        let data = fs::read("./test_files/ab").unwrap();
        let encoding = detect_encoding(&data);

        // This does not work -> windows-1252 but low confidence: < 30
        println!(
            "Detected encoding: {} Confidence: {}",
            encoding.0, encoding.1
        );
    }
}
