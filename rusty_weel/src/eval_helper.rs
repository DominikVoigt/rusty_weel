use std::{collections::HashMap, fmt::Display};

use log;
use serde_json::json;

use crate::{data_types::{DynamicData, StaticData}, dsl_realization::Error};

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
) -> Result<HashMap<String, String>, Error> {
    let mut client = Easy::new();

    // Handle serialization outside of json! macro as it will panic otherwise
    let static_context = match serde_json::to_string(&static_context) { Ok(x) => x, Err(err) => return Err(Error::EvalError(EvalError::GeneralEvalError(format!("Could not serialize static context: {}", err)))) };
    let dynamic_context = match serde_json::to_string(&dynamic_context) { Ok(x) => x, Err(err) => return Err(Error::EvalError(EvalError::GeneralEvalError(format!("Could not serialize static context: {}", err)))) };
    let expressions = match serde_json::to_string(&expressions) { Ok(x) => x, Err(err) => return Err(Error::EvalError(EvalError::GeneralEvalError(format!("Could not serialize expressions: {}", err)))) };
    
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
            return Err(Error::EvalError(EvalError::GeneralEvalError(format!("Could not serialize json objection for serailization request: {}", err))));
        }
    };
    client.url(eval_backend);
    client.post(true);
    client.post_fields_copy(body.as_bytes());

    let response_body = client.respons

    let evaluation_request = client.perform()?;
    let response_code = client.response_code()?;
    let response_text = String::from_utf8(response_body)?;
    let evaluations: HashMap<String, String> = serde_json::from_str(&response_text)
        .expect("Failed to parse evaluations from response body");
    Ok(evaluations)
}

pub fn evaluate_expression (
    eval_backend: &str,
    dynamic_context: &DynamicData,
    static_context: &StaticData,
    expression: &str
) -> Result<String, Error> {
    let mut expressions = HashMap::new();
    expressions.insert("k".to_owned(), expression.to_owned());
    let result = evaluate_expressions(eval_backend, dynamic_context, static_context,  expressions)?;
    match result.get("k") {
        Some(x) => Ok(x.clone()),
        None => {
            log::error!("failure creating new key value pair. Evaluation failed");
            Err(Error::EvalError(EvalError::GeneralEvalError(format!("Result misses value"))))
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
