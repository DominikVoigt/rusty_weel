#![allow(dead_code)]

pub mod dsl;
pub mod dsl_realization;
pub mod data_types;

pub mod connection_wrapper;
pub mod eval_helper;
pub mod redis_helper;

#[cfg(test)]
mod test {
    
    #[test]
    fn test_reqwest() {
        let client = reqwest::Client::new();
        let request = client.post("http://testing.com");
    }
}