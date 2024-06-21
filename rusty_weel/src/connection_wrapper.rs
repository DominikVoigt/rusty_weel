use http_helper::RiddlParameters;
use reqwest::{header::{HeaderMap, HeaderName, HeaderValue}, StatusCode};
use serde_json::{json, Value};
use tempfile::tempfile;
use std::{
    collections::HashMap, io::Write, str::FromStr, sync::Arc, thread::sleep, time::{Duration, SystemTime}
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

            if self.weel.static_data.attributes.get("twin_engine").map(|attr| !attr.is_empty()).unwrap_or(false) {
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

    /**
     * Variation of original proto curl implementation:
     *      - We no longer support the special prefixed arguments that are transformed into parameters, we convert all parameters into simple url encoded body parameters
     */
    pub fn curl(self: &Arc<Self>, parameters: &HTTPParams) -> Result<()>{
        let callback_id = generate_random_key();
        let mut headers: HeaderMap = self.generate_headers(self.weel.static_data.get_instance_meta_data(), &callback_id)?;
        let mut params = Vec::new();
        match parameters.arguments.as_ref() {
            Some(args) => args.iter().for_each(|arg| {
                    let value = arg.value.clone().unwrap_or("".to_owned());
                    params.push(http_helper::RiddlParameters::SimpleParameter { name: arg.key.to_owned(), value, param_type: http_helper::ParameterType::Body });
                }),
            None => {log::info!("Arguments provided to protocurl are empty");}
        };

        let mut status: StatusCode;
        let mut response_body: String; 
        let mut response_headers: HeaderMap;
        
        loop {
            let client = reqwest::blocking::Client::new();
            let mut content = HashMap::new();
            content.insert("activity_uuid".to_owned(), self.handler_activity_uuid.clone());
            content.insert("label".to_owned(), self.label.clone());
            let position = self.position.as_ref().map(|x| x.clone()).unwrap_or("".to_owned());
            content.insert("activity".to_owned(), position);
            
            self.weel.callback(Arc::clone(self), &callback_id, content);

            // TODO: Determine wheter we need to subsitute the url like in connection.rb
            let response = client.request(parameters.method.clone(), self.handler_endpoints.get(0).expect("No endpoint provided")).headers(headers).send()?;
            
            status = response.status();
            let status_code = status.as_u16();
            // need to copy headers as .text() call will consume response struct
            response_headers = response.headers().clone();
            response_body = response.text()?;
            
            // TODO: decide whether we still need the if status == 561 ...... block

            // TODO: rewrite this condition and give it a better name
            if status_code < 200 || status_code >= 300  {
                // TODO: What to do with the commented out ruby code?
                // c = result[0]&.value
                // c = c.read if c.respond_to? :read
                let mut tmp = tempfile()?;
                let content = serde_json::to_string(&json!({"state": status_code, "error": response_body}))?;
                tmp.write(content.as_bytes());
                let param = RiddlParameters::ComplexParamter { name: "error".to_owned(), mime_type: "application_json".to_owned(), content_handle: tmp};
                self.callback(vec![param], response_headers)
            } else {
                let callback_header_set = match response_headers.get("CPEE_CALLBACK") {
                    Some(header) => {
                        header.to_str()? == "true"
                    },
                    None => false,
                };

                 
            }
        }

        todo!()
    }

    pub fn callback(&self, parameters: Vec<RiddlParameters>, headers: HeaderMap) {}

    fn generate_headers(&self, data: InstanceMetaData, callback_id: &str) -> Result<HeaderMap> {
        let position = self.position.as_ref().map(|x| x.as_str()).unwrap_or("");
        let twin_target = data.attributes.get("twin_target");
        let mut headers = HeaderMap::new();
        headers.insert("CPEE-BASE",            HeaderValue::from_str(&data.cpee_base_url)?);
        headers.insert("CPEE-Instance",        HeaderValue::from_str(&data.instance_id)?);
        headers.insert("CPEE-Instance-URL",    HeaderValue::from_str(&data.instance_url)?);
        headers.insert("CPEE-Instance-UUID",   HeaderValue::from_str(&data.instance_uuid)?);
        headers.insert("CPEE-CALLBACK",        HeaderValue::from_str(&format!("{}/callbacks/{}/", &data.instance_url, callback_id))?);
        headers.insert("CPEE-CALLBACK-ID",     HeaderValue::from_str(callback_id)?);
        headers.insert("CPEE-ACTIVITY",        HeaderValue::from_str(&position)?);
        headers.insert("CPEE-LABEL",           HeaderValue::from_str(&self.label)?);
        if let Some(twin_target) = twin_target {
            headers.insert("CPEE-TWIN-TARGET",     HeaderValue::from_str(twin_target)?);
        }

        for attribute in data.attributes.iter() {
            let key: String = format!("CPEE-ATTR-{}", attribute.0.replace("_", "-"));
            headers.insert(HeaderName::from_str(&key)?, HeaderValue::from_str(attribute.1)?);
        }
        
        Ok(headers)
    }
}


