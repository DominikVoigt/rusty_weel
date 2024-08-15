
use http_helper::Parameter;
use reqwest::{header::{HeaderMap, HeaderName, HeaderValue, CONTENT_TYPE}, Method};
use serde_json::{json, Value};
use std::{
    collections::HashMap, os::linux::raw::stat, str::FromStr, sync::{Arc, Mutex, Weak}, thread::sleep, time::{Duration, SystemTime}
};



use crate::{data_types::{HTTPParams, InstanceMetaData}, dsl_realization::{self, generate_random_key, Error, Result, Weel}};

pub struct ConnectionWrapper {
    weel: Weak<Weel>,
    position: Option<String>,
    // Continue object for thread synchronization -> TODO: See whether we need this/how we implement this
    handler_continue: Option<()>,
    // See proto_curl in connection.rb
    handler_passthrough: Mutex<Option<String>>,
    // TODO: Unsure about this type:
    handler_return_value: Option<String>,
    // TODO: Determine this type:
    handler_return_options: Option<()>,
    // We keep them as arrays to be flexible but will only contain one element for now
    handler_endpoints: Vec<String>,
    handler_endpoint_origin: Vec<String>,
    handler_activity_uuid: String,
    label: String,
}

// Determines whether recurring calls are too close together (in seconds)
const LOOP_GUARD_DELTA: f32 = 2.0;
// Determines how many calls can be made in total before an activity might be throttled
const UNGUARDED_CALLS: u32 = 100;
// Determines how many seconds the call should be delayed (for throttling)
const SLEEP_DURATION: u64 = 2;

impl ConnectionWrapper {
    pub fn new(weel: Arc<Weel>, position: Option<String>, handler_continue: Option<()>) -> Self {
        let weel = Arc::downgrade(&weel);
        ConnectionWrapper {
            weel,
            position,
            handler_continue,
            handler_passthrough: Mutex::new(None),
            handler_return_value: None,
            handler_return_options: None,
            handler_endpoints: Vec::new(),
            handler_activity_uuid: generate_random_key(),
            label: "".to_owned(),
            handler_endpoint_origin: Vec::new(),
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

    pub fn inform_syntax_error(&self, err: Error, code: &str) -> Result<()> {
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

    fn inform(&self, what: &str, content: Option<HashMap<String, String>>) -> Result<()>{
        let weel = self.weel();
        weel
            .redis_notifications_client
            .lock()
            .expect("Could not acquire mutex")
            .notify(
                what,
                content,
                weel.static_data.get_instance_meta_data(),
            )?;
            Ok(())
    }

    /**
     * Normaly provides copy of parameters, we do not need this.
     * Also adapts the endpoint
     */
    pub fn prepare(&mut self, endpoints: &Vec<String>) {
        if endpoints.len() > 0 {
            let weel = self.weel();
            self.handler_endpoints = endpoints.iter().map(|ep| {
                weel
                    .dynamic_data
                    .endpoints
                    .get(ep)            
            })
            .filter(|item| item.is_some())
            .map(|item| item.expect("safe to unwrap").clone())
            .collect();

            if weel.static_data.attributes.get("twin_engine").map(|attr| !attr.is_empty()).unwrap_or(false) {
                self.handler_endpoint_origin = self.handler_endpoints.clone();
                let twin_engine: &str = &weel.static_data.attributes.get("twin_engine").expect("Cannot happen");

                // TODO: Replace the endpoints part: `Riddl::Protocols::Utils::escape`
                let endpoints = self.handler_endpoints.get(0).expect("");
                self.handler_endpoints = vec![format!("{}?original_endpoint={}", twin_engine, endpoints)];
            }


        }
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
                    "id": self.position
                }
            }
        )
    }

    /**
     * Variation of original proto curl implementation:
     *      - We no longer support the special prefixed arguments that are transformed into parameters, we convert all parameters into simple url encoded body parameters
     *      - Only the standard headers that are generated in the `henerate_headers` method are send.
     *      - All arguments within the HTTPParams are send as Key-Value pairs as part of the body (application/x-www-form-urlencoded)
     *          -> TODO: Our implementation does multipart, is this fine?
     */
    pub fn curl(self: &Arc<Self>, parameters: &HTTPParams) -> Result<()>{
        let weel = self.weel();
        let callback_id = generate_random_key();
        *self.handler_passthrough.lock().unwrap() = Some(callback_id.clone());

        // Generate headers
        let headers: HeaderMap = self.generate_headers(weel.static_data.get_instance_meta_data(), &callback_id)?;
        // Put arguemnts into SimpleParameters that will be part of the body-> Since we cannot upload files from the CPEE for now // TODO?

        let mut status: u16;
        let mut response_headers: HeaderMap;
        let mut content: Vec<Parameter>;
        loop {
            // Compute parameters
            let mut params = Vec::new();
            // Params could contain file handles (complex parameters) and thus cannot be cloned -> We cannot clone so we recompute them here
            match parameters.arguments.as_ref() {
                Some(args) => args.iter().for_each(|arg| {
                        let value = arg.value.clone().unwrap_or("".to_owned());
                        params.push(http_helper::Parameter::SimpleParameter { name: arg.key.to_owned(), value, param_type: http_helper::ParameterType::Body });
                    }),
                None => {log::info!("Arguments provided to protocurl are empty");}
            };

            let mut content_json = HashMap::new();
            content_json.insert("activity_uuid".to_owned(), self.handler_activity_uuid.clone());
            content_json.insert("label".to_owned(), self.label.clone());
            let position = self.position.as_ref().map(|x| x.clone()).unwrap_or("".to_owned());
            content_json.insert("activity".to_owned(), position);
            
            // TODO: Handler Passthrough? -> When task is called that was async and instance was stopped in between
            weel.callback(Arc::clone(self), &callback_id, content_json)?;
            
            // TODO: Determine wheter we need to subsitute the url like in connection.rb
            let mut client = http_helper::Client::new(self.handler_endpoints.get(0).expect("No endpoint provided"), parameters.method.clone())?;
            client.set_request_headers(headers.clone());
            client.add_parameters(params);
            
            let response = client.execute()?;

            status = response.status_code;
            response_headers = response.headers;
            content = response.content;

            if status == 561 {
                
            }
            /*
            // Run request
            // TODO: Check whether this access content_type
            let content_type = match response_headers.get(CONTENT_TYPE.as_str()) {
                Some(header) => header.to_str()?,
                None => "application/octet-stream",
            };

            // TODO: decide whether we still need the if status == 561 ...... block -> Yes we need it

            // TODO: rewrite this condition and give it a better name
            if status_code < 200 || status_code >= 300  {
                // TODO: What to do with the commented out ruby code?
                // TODO: Why write to file and not just keep it in memory? Too large?
                let param = Parameter::ComplexParamter { name: "error".to_owned(), mime_type: "application_json".to_owned(), content_handle: tmp};
                self.callback(vec![param], response_headers)
            } else {
                let callback_header_is_set = match response_headers.get("CPEE_CALLBACK") {
                    Some(header) => {
                        header.to_str()? == "true"
                    },
                    None => false,
                };

                if callback_header_is_set {
                    if response_body.is_empty() {
                        
                    } else {
                        response_headers.insert("CPEE_UPDATE".to_owned(), HeaderValue::from_str("true").expect("This cannot fail"));
                        
                        self.callback(vec![Parameter::ComplexParamter { name: "response".to_owned(), mime_type: content_type.to_owned(), content_handle: response_file }], response_headers)
                    }
                }
                 
            }
             */
        }

        todo!()
    }

    pub fn callback(&self, parameters: Vec<Parameter>, headers: HashMap<String, String>) {

    }

    fn generate_headers(&self, data: InstanceMetaData, callback_id: &str) -> Result<HeaderMap> {
        let position = self.position.as_ref().map(|x| x.as_str()).unwrap_or("");
        let mut headers = HeaderMap::new();
        headers.append("CPEE-BASE",            HeaderValue::from_str(&data.cpee_base_url)?);
        headers.append("CPEE-Instance",        HeaderValue::from_str(&data.instance_id)?);
        headers.append("CPEE-Instance-URL",    HeaderValue::from_str(&data.instance_url)?);
        headers.append("CPEE-Instance-UUID",   HeaderValue::from_str(&data.instance_uuid)?);
        headers.append("CPEE-CALLBACK",        HeaderValue::from_str(&format!("{}/callbacks/{}/", &data.instance_url, callback_id))?);
        headers.append("CPEE-CALLBACK-ID",     HeaderValue::from_str(callback_id)?);
        headers.append("CPEE-ACTIVITY",        HeaderValue::from_str(&position)?);
        headers.append("CPEE-LABEL",           HeaderValue::from_str(&self.label)?);

        let twin_target = data.attributes.get("twin_target");
        if let Some(twin_target) = twin_target {
            headers.append("CPEE-TWIN-TARGET", HeaderValue::from_str(twin_target)?);
        }

        for attribute in data.attributes.iter() {
            let key: String = format!("CPEE-ATTR-{}", attribute.0.replace("_", "-"));
            headers.append(HeaderName::from_str(&key)?, HeaderValue::from_str(attribute.1)?);
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
            },
        }
    }
}


