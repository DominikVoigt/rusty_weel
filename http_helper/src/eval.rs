use std::{collections::HashMap};

use serde_json::json;

/**
 * Sends a list of expressions and the context to evaluate them in to the evaluation backend
 * If any error occurs then this function panics
 */
pub fn evaluate(eval_backend: &str, context: String, statements: HashMap<String, String>) -> HashMap<String, String> {
    let client = reqwest::blocking::Client::new();
    let body = json!(
        {
            "context": context,
            "statements": statements
        });
    
    let evaluation_request = client.post(eval_backend)
          .body(
            serde_json::to_string(&body).expect("Could not parse evaluation request body")
          )
          .send();
    match evaluation_request {
        Ok(response) => {
            let response_text = response.text().expect("Response from evaluation is malformed");
            let evaluations: HashMap<String, String> = serde_json::from_str(&response_text).expect("Failed to parse evaluations from response body");
            evaluations
        },
        Err(err) => {
            log::error!("Error evaluating code via eval backend: {eval_backend}, Error: {err}");
            panic!()
        }
    }
}