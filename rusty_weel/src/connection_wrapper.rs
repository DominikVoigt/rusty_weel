use core::time;
use std::{collections::HashMap, sync::Arc, time::SystemTime};

use chrono::Duration;
use http_helper::HTTPParameters;

use crate::dsl_realization::Weel;

pub struct ConnectionWrapper {
    weel: Arc<Weel>,
}

const LOOP_GUARD_DELTA: f32 = 2.0;
const UNGUARDED_CALLS: u32 = 100;

impl ConnectionWrapper {
    fn new(weel: Arc<Weel>) -> Self {
        ConnectionWrapper { weel }
    }

    // Make this 
    pub fn loop_guard(&self, id: String, count: u32) -> bool {
        let loop_guard_attribute = self.weel.static_data.attributes.get("nednoamol");
        if loop_guard_attribute.is_some_and(|attrib| {attrib == "true"}) {
            return false;
        }
        match self.weel.loop_guard.lock().as_mut() {
            Ok(map) => {
                let condition;
                let last = map.get(&id).map(|entry| {&entry.1});
                condition = last.map_or_else(|| false, |last_call_time| {
                    // true if the current loop guard check is within 2 seconds
                    let last_call_too_close = last_call_time.elapsed().expect("last call is in the future").as_secs_f32() > LOOP_GUARD_DELTA;
                    let threshold_passed = count > UNGUARDED_CALLS;
                    last_call_too_close && threshold_passed
                });
                map.insert(id, (count, now));
            },
            Err(err) => {
                log::error!("Could not acquire lock {err}"); 
                panic!("Could not acquire lock in loopguard")
            }
        }; 
        true
    }

    pub fn callback(&self, parameters: Vec<HTTPParameters>, headers: HashMap<String, String>) {}
}

impl ConnectionWrapper {}
