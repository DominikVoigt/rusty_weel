use http_helper::RiddlParameters;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde::de::value;
use serde_json::{json, Value};
use std::{
    borrow::Borrow, collections::HashMap, fs::File, str::FromStr, sync::Arc, thread::sleep, time::{Duration, SystemTime}
};



use crate::{data_types::{HTTPParams, InstanceMetaData}, dsl_realization::{generate_random_key, Error, Result, Weel}};

pub struct ConnectionWrapper {
    weel: Arc<Weel>,
    position: Option<String>,
    // Continue object for thread synchronization -> TODO: See whether we need this/how we implement this
    handler_continue: Option<()>,
    // See proto_curl in connection.rb
    handler_passthrough: Option<String>,
    // TODO: Unsure about this type:
    handler_returnValue: Option<String>,
    // TODO: Determine this type:
    handler_returnOptions: Option<()>,
    // We keep them as arrays to be flexible but will only contain one element for now
    handler_endpoints: Vec<String>,
    handler_endpoint_origin: Vec<String>,
    handler_activity_uuid: String,
    label: String,
    // TODO: Determine whether we need this:
    guard_files: Vec<()>,
}

const LOOP_GUARD_DELTA: f32 = 2.0;
const UNGUARDED_CALLS: u32 = 100;
const SLEEP_DURATION: u64 = 2;

impl ConnectionWrapper {
    fn new(weel: Arc<Weel>, position: Option<String>, handler_continue: Option<()>) -> Self {
        ConnectionWrapper {
            weel,
            position,
            handler_continue,
            handler_passthrough: None,
            handler_returnValue: None,
            handler_returnOptions: None,
            handler_endpoints: Vec::new(),
            handler_activity_uuid: generate_random_key(),
            label: "".to_owned(),
            guard_files: Vec::new(),
            handler_endpoint_origin: Vec::new(),
        }
    }

    pub fn loop_guard(&self, id: String) {
        let loop_guard_attribute = self.weel.static_data.attributes.get("nednoamol");
        if loop_guard_attribute.is_some_and(|attrib| attrib == "true") {
            return;
        }
        match self.weel.loop_guard.lock().as_mut() {
            Ok(map) => {
                let condition;
                let last = map.get(&id);
                condition = match last {
                    Some(entry) => {
                        let count = entry.0 + 1;
                        let last_call_time = entry.1;
                        map.insert(id, (count, SystemTime::now()));

                        // true if the current loop guard check is within 2 seconds
                        let last_call_too_close = last_call_time
                            .elapsed()
                            .expect("last call is in the future")
                            .as_secs_f32()
                            > LOOP_GUARD_DELTA;
                        let threshold_passed = count > UNGUARDED_CALLS;
                        last_call_too_close && threshold_passed
                    }
                    None => {
                        map.insert(id, (1, SystemTime::now()));
                        false
                    }
                };
            }
            Err(err) => {
                log::error!("Could not acquire lock {err}");
                panic!("Could not acquire lock in loopguard")
            }
        };
        sleep(Duration::from_secs(SLEEP_DURATION));
    }

    pub fn inform_state_change(&self, new_state: &str) {
        let mut content = HashMap::new();
        content.insert("state".to_owned(), new_state.to_owned());
        self.weel
            .redis_notifications_client
            .lock()
            .expect("Failed to lock mutex")
            .notify(
                "state/change",
                Some(content),
                self.weel.static_data.get_instance_meta_data(),
            )
    }

    pub fn inform_syntax_error(&self, err: Error, code: &str) {
        let mut content = HashMap::new();
        // TODO: mess = err.backtrace ? err.backtrace[0].gsub(/([\w -_]+):(\d+):in.*/,'\\1, Line \2: ') : ''
        content.insert("message".to_owned(), err.as_str().to_owned());
        self.weel
            .redis_notifications_client
            .lock()
            .expect("Could not acquire mutex")
            .notify(
                "description/error",
                Some(content),
                self.weel.static_data.get_instance_meta_data(),
            )
    }

    pub fn inform_connectionwrapper_error(&self, err: Error) {
        let mut content = HashMap::new();
        // TODO: mess = err.backtrace ? err.backtrace[0].gsub(/([\w -_]+):(\d+):in.*/,'\\1, Line \2: ') : ''
        content.insert("message".to_owned(), err.as_str().to_owned());
        self.weel
            .redis_notifications_client
            .lock()
            .expect("Could not acquire mutex")
            .notify(
                "executionhandler/error",
                Some(content),
                self.weel.static_data.get_instance_meta_data(),
            )
    }

    pub fn inform_position_change(&self, ipc: Option<HashMap<String, String>>) {
        self.weel
            .redis_notifications_client
            .lock()
            .expect("Could not acquire mutex")
            .notify(
                "position/change",
                ipc,
                self.weel.static_data.get_instance_meta_data(),
            )
    }

    /**
     * Normaly provides copy of parameters, we do not need this.
     * Also adapts the endpoint
     */
    pub fn prepare(&mut self, endpoints: &Vec<String>) {
        if endpoints.len() > 0 {
            self.handler_endpoints = endpoints.iter().map(|ep| {
                self.weel
                    .dynamic_data
                    .endpoints
                    .get(ep)            
            })
            .filter(|item| item.is_some())
            .map(|item| item.expect("safe to unwrap").clone())
            .collect();

            if self.weel.static_data.attributes.get("twin_engine").map(|attr| !attr.is_empty()).unwrap_or_else(|| false) {
                self.handler_endpoint_origin = self.handler_endpoints.clone();
                let twin_engine: &str = &self.weel.static_data.attributes.get("twin_engine").expect("Cannot happen");

                // TODO: Replace the endpoints part: `Riddl::Protocols::Utils::escape`
                let endpoints = self.handler_endpoints.get(0).expect("");
                self.handler_endpoints = vec![format!("{}?original_endpoint={}", twin_engine, endpoints)];
            }
        }
    }

    pub fn additional(&self) -> Value {
        let data = &self.weel.static_data;
        json!(
            {
                "attributes": self.weel.static_data.attributes,
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

    pub fn curl(&self, parameters: &HTTPParams) {
        let callback_id = generate_random_key();
        let mut headers: HeaderMap = self.generate_headers(self.weel.static_data.get_instance_meta_data(), &callback_id);
        let mut params = Vec::new();
        match parameters.arguments.as_ref() {
            Some(args) => args.iter().for_each(|arg| {
                    let value = arg.value.clone().unwrap_or_else(|| "".to_owned());
                    params.push(http_helper::RiddlParameters::SimpleParameter { name: arg.key.to_owned(), value, param_type: http_helper::ParameterType::Body });
                }),
            None => {log::info!("Arguments provided to protocurl are empty");}
        };

        headers.insert(key, val)
    }

    pub fn callback(&self, parameters: Vec<RiddlParameters>, headers: HashMap<String, String>) {}

    fn generate_headers(&self, data: InstanceMetaData, callback_id: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("CPEE-BASE",            HeaderValue::from_str(&data.cpee_base_url).expect("Could not fill header value"));
        headers.insert("CPEE-Instance",        HeaderValue::from_str(&data.instance_id).expect("Could not fill header value"));
        headers.insert("CPEE-Instance-URL",    HeaderValue::from_str(&data.instance_url).expect("Could not fill header value"));
        headers.insert("CPEE-Instance-UUID",   HeaderValue::from_str(&data.instance_uuid).expect("Could not fill header value"));
        headers.insert("CPEE-CALLBACK",        HeaderValue::from_str(&format!("{}/callbacks/{}/", &data.instance_url, callback_id)).expect("Could not fill header value"));
        headers.insert("CPEE-CALLBACK-ID",     HeaderValue::from_str(callback_id).expect("Could not fill header value"));
        headers.insert("CPEE-ACTIVITY",        HeaderValue::from_str(self.position.unwrap_or_else(f)).expect("Could not fill header value"));
        headers.insert("CPEE-LABEL",           HeaderValue::from_str().expect("Could not fill header value"));
        headers.insert("CPEE-TWIN-TARGET",     HeaderValue::from_str().expect("Could not fill header value"));
    
        headers
    }
}


