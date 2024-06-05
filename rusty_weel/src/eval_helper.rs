use std::{collections::HashMap, error::Error, fmt::Display};

use log;
use serde_json::json;

/**
 * Sends a list of expressions and the context to evaluate them in to the evaluation backend
 * If any error occurs then this function panics
 * Returns an error if:
 *  - Request body could not be parsed
 *  - Post request fails
 */
// TODO: Currently the context passed to the evaluated is currently only the data, not endpoints/attributes -> Do we also need these?
// TODO: Yes we need to pass all contained in ManipulateStructure (weel.rb)?
pub fn evaluate(
    eval_backend: &str,
    context: String,
    statements: HashMap<String, String>,
) -> Result<HashMap<String, String>, EvalError> {
    let client = reqwest::blocking::Client::new();
    let body = json!(
    {
        "context": context,
        "statements": statements
    });
    let body = match serde_json::to_string(&body) {
        Ok(x) => x,
        Err(err) => {
            log::error!("Error parsing evaluation request body. Error: {err}");
            return Err(EvalError {
                internal_error: Box::new(err),
            });
        }
    };

    let evaluation_request = client.post(eval_backend).body(body).send();

    // TODO: Think about how we handle errors occuring during the execution of the individual expressions (results in hashmap?)
    match evaluation_request {
        Ok(response) => {
            let response_text = response
                .text()
                .expect("Response from evaluation is malformed");
            let evaluations: HashMap<String, String> = serde_json::from_str(&response_text)
                .expect("Failed to parse evaluations from response body");
            Ok(evaluations)
        }
        Err(err) => {
            log::error!("Error evaluating code via eval backend: {eval_backend}, Error: {err}");
            Err(EvalError {
                internal_error: Box::new(err),
            })
        }
    }
}

#[derive(Debug)]
pub struct EvalError {
    internal_error: Box<dyn Error>,
}

impl Display for EvalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Error occured when evaluating an expression in an external language: {:?}",
            self.internal_error
        )
    }
}

impl Error for EvalError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.internal_error.as_ref())
    }

    fn description(&self) -> &str {
        "description() is deprecated; use Display"
    }

    fn cause(&self) -> Option<&dyn Error> {
        self.source()
    }
}
