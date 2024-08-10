use std::{collections::HashMap, fmt::Display, io::{Read, Seek, Write}};

use log;
use serde_json::json;
use http_helper::{Client, Mime, Parameter, APPLICATION_JSON};
use tempfile::tempfile;

use crate::{data_types::{DynamicData, StaticData}, dsl_realization::{Result, Error}};

/**
 * Sends a list of expressions and the context to evaluate them in to the evaluation backend
 * Returns an error if:
 *  - Request body could not be parsed
 *  - Post request fails / Evaluation fails
 */
pub fn evaluate_expressions (
    eval_backend: &str,
    dynamic_context: &DynamicData,
    static_context: &StaticData,
    expressions: HashMap<String, String>,
) -> Result<HashMap<String, String>> {
    let mut client = Client::new(eval_backend, http_helper::Method::POST)?;

    // Handle serialization outside of json! macro as it will panic otherwise
    let static_context = match serde_json::to_string(&static_context) { Ok(x) => x, Err(err) => return Err(Error::EvalError(EvalError::GeneralEvalError(format!("Could not serialize static context: {}", err)))) };
    let static_context_file = tempfile()?;
    static_context_file.write_all(static_context.as_bytes());
    static_context_file.rewind();

    let dynamic_context = match serde_json::to_string(&dynamic_context) { Ok(x) => x, Err(err) => return Err(Error::EvalError(EvalError::GeneralEvalError(format!("Could not serialize static context: {}", err)))) };
    let dynamic_context_file = tempfile()?;
    dynamic_context_file.write_all(dynamic_context.as_bytes());
    dynamic_context_file.rewind();
    
    let expressions = match serde_json::to_string(&expressions) { Ok(x) => x, Err(err) => return Err(Error::EvalError(EvalError::GeneralEvalError(format!("Could not serialize expressions: {}", err)))) };
    let expressions_file = tempfile()?;
    expressions_file.write_all(expressions.as_bytes());
    expressions_file.rewind();

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
    client.add_parameter(Parameter::ComplexParameter { name: "static_context".to_owned(), mime_type: APPLICATION_JSON, content_handle: static_context_file });
    client.add_parameter(Parameter::ComplexParameter { name: "dynamic_context".to_owned(), mime_type: APPLICATION_JSON, content_handle: dynamic_context_file });
    client.add_parameter(Parameter::ComplexParameter { name: "expressions".to_owned(), mime_type: APPLICATION_JSON, content_handle: expressions_file });
    
    let result = client.execute()?;
    // Get the expressions parameter from the parsed response
    let result = result.content.iter().find_map(|parameter| -> Option<String> {
        if parameter.name == "expressions" {
            match parameter {
                Parameter::SimpleParameter { name, value, param_type } => Some(value.clone()),
                Parameter::ComplexParameter { name, mime_type, content_handle } => {
                    let mut result = String::new();
                    content_handle.read_to_string(&mut result);
                    Some(result)
                },
            }
        } else {
            None
        }
    });

    let evaluations: HashMap<String, String> = serde_json::from_str(&result)?;
    Ok(evaluations)
}

pub fn evaluate_expression (
    eval_backend: &str,
    dynamic_context: &DynamicData,
    static_context: &StaticData,
    expression: &str
) -> Result<String> {
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
