use std::{collections::HashMap, fmt::Display};

use log;
use serde_json::json;

use crate::data_types::{DynamicData, StaticData};

/**
 * Sends a list of expressions and the context to evaluate them in to the evaluation backend
 * Returns an error if:
 *  - Request body could not be parsed
 *  - Post request fails / Evaluation fails
 */
// TODO: Currently the context passed to the evaluated is currently only the data, not endpoints/attributes -> Do we also need these?
// TODO: Yes we need to pass all contained in ManipulateStructure (weel.rb)?
pub fn evaluate_expressions (
    eval_backend: &str,
    dynamic_context: &DynamicData,
    static_context: &StaticData,
    expressions: HashMap<String, String>,
) -> Result<HashMap<String, String>, EvalError> {
    let client = reqwest::blocking::Client::new();

    // Handle serialization outside of json! macro as it will panic otherwise
    let static_context = match serde_json::to_string(&static_context) { Ok(x) => x, Err(err) => return Err(EvalError::GeneralEvalError(format!("Could not serialize static context: {}", err))) };
    let dynamic_context = match serde_json::to_string(&dynamic_context) { Ok(x) => x, Err(err) => return Err(EvalError::GeneralEvalError(format!("Could not serialize static context: {}", err))) };
    let expressions = match serde_json::to_string(&expressions) { Ok(x) => x, Err(err) => return Err(EvalError::GeneralEvalError(format!("Could not serialize expressions: {}", err))) };
    
    let body = json!(
    {
        "static_context": static_context,
        "static_context": dynamic_context,
        "statements": expressions
    });
    let body = match serde_json::to_string(&body) {
        Ok(x) => x,
        Err(err) => {
            log::error!("Error parsing evaluation request body. Error: {err}");
            return Err(EvalError::GeneralEvalError(format!("Could not serialize json objection for serailization request: {}", err)));
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
            return Err(EvalError::GeneralEvalError(format!("{}", err)));
        }
    }
}

pub fn evaluate_expression (
    eval_backend: &str,
    dynamic_context: &DynamicData,
    static_context: &StaticData,
    expression: &str
) -> Result<String, EvalError> {
    let mut expressions = HashMap::new();
    expressions.insert("k".to_owned(), expression.to_owned());
    let result = evaluate_expressions(eval_backend, dynamic_context, static_context,  expressions);
    match result {
        Ok(eval_result) => 
            match eval_result.get("k") {
                Some(x) => Ok(x.clone()),
                None => {
                    log::error!("failure creating new key value pair. Evaluation failed");
                    Err(EvalError::GeneralEvalError(format!("Result misses value")))
                }
            },
        Err(err) => {
            log::error!(
                "failure creating new key value pair. {}", err
            );
            Err(err)
        }
    }
}

#[derive(Debug)]
pub enum EvalError {
    GeneralEvalError(String),
    SyntaxError
}

impl Display for EvalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Error occured when evaluating an expression in an external language: {:?}",
            if matches!(self, EvalError::SyntaxError) {"Syntax error"} else {"general error"}
        )
    }
}
