use std::{
    collections::HashMap,
    fmt::Display,
    io::{Read, Seek, Write},
};

use http_helper::{Client, Parameter};
use log;
use tempfile::tempfile;

use crate::{
    data_types::{DynamicData, State, StaticData},
    dsl_realization::{Error, Result},
};

/**
 * Sends a list of expressions and the context to evaluate them in to the evaluation backend
 * Returns an error if:
 *  - Request body could not be parsed
 *  - Post request fails / Evaluation fails
 */
pub fn evaluate_expressions(
    dynamic_context: &DynamicData,
    static_context: &StaticData,
    expressions: HashMap<String, String>,
    weel_state: &State,
    local: Option<&HashMap<String, String>>
) -> Result<HashMap<String, String>> {
    let mut client = Client::new(&static_context.eval_backend_url, http_helper::Method::POST)?;

    // Handle serialization outside of json! macro as it will panic otherwise
    let static_context = serde_json::to_string(&static_context)?;
    let mut static_context_file = tempfile()?;
    static_context_file.write_all(static_context.as_bytes())?;
    static_context_file.rewind()?;

    let dynamic_context = serde_json::to_string(&dynamic_context)?;
    let mut dynamic_context_file = tempfile()?;
    dynamic_context_file.write_all(dynamic_context.as_bytes())?;
    dynamic_context_file.rewind()?;

    let mut status_file = tempfile()?;
    status_file.write_all(format!("{:?}", weel_state).as_bytes())?;
    status_file.rewind()?;

    let expressions = serde_json::to_string(&expressions)?;

    let mut expressions_file = tempfile()?;
    expressions_file.write_all(expressions.as_bytes())?;
    expressions_file.rewind()?;

    client.add_parameter(Parameter::ComplexParameter {
        name: "static_context".to_owned(),
        mime_type: mime::APPLICATION_JSON,
        content_handle: static_context_file,
    });
    client.add_parameter(Parameter::ComplexParameter {
        name: "dynamic_context".to_owned(),
        mime_type: mime::APPLICATION_JSON,
        content_handle: dynamic_context_file,
    });
    client.add_parameter(Parameter::ComplexParameter {
        name: "expressions".to_owned(),
        mime_type: mime::TEXT_PLAIN_UTF_8,
        content_handle: expressions_file,
    });

    if let Some(local) = local {
        let mut local_file = tempfile()?;
        let local = serde_json::to_string(local)?;
        local_file.write_all(local.as_bytes())?;
        local_file.rewind()?;
        client.add_parameter(Parameter::ComplexParameter { name: "local".to_owned(), mime_type: mime::APPLICATION_JSON, content_handle: local_file })
    }


    let mut result = client.execute()?;
    // Get the expressions parameter from the parsed response
    let mut expression_results = None;
    while let Some(parameter) = result.content.pop() {
        // Find the expressions parameter, will result in None if it does not exist
        expression_results = match parameter {
            Parameter::SimpleParameter { name, value, .. } => {
                if name == "expressions" {
                    Some(value.clone())
                } else {
                    continue;
                }
            }
            Parameter::ComplexParameter {
                name,
                mut content_handle,
                ..
            } => {
                if name == "expressions" {
                    let mut result = String::new();
                    content_handle.read_to_string(&mut result)?;
                    Some(result)
                } else {
                    continue;
                }
            }
        };
        if expression_results.is_some() {
            break;
        }
    }
    match expression_results {
        Some(result1) => {
            let evaluations: HashMap<String, String> = serde_json::from_str(&result1)?;
            Ok(evaluations)
        }
        None => Err(Error::EvalError(EvalError::GeneralEvalError("Result does not contain result".to_owned()))),
    }
}

pub fn evaluate_expression(
    dynamic_context: &DynamicData,
    static_context: &StaticData,
    expression: &str,
    weel_state: &State,
    local: Option<&HashMap<String, String>>
) -> Result<String> {
    let mut expressions = HashMap::new();
    expressions.insert("k".to_owned(), expression.to_owned());
    let result = evaluate_expressions(dynamic_context, static_context, expressions, weel_state, local)?;
    match result.get("k") {
        // TODO: Some kind of error handling: depends on service implementation
        Some(x) => Ok(x.clone()),
        None => {
            log::error!("failure creating new key value pair. Evaluation failed");
            Err(Error::EvalError(EvalError::GeneralEvalError(format!(
                "Result misses value"
            ))))
        }
    }
}

#[derive(Debug)]
pub enum EvalError {
    GeneralEvalError(String),
    SyntaxError,
}

impl Display for EvalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Error occured when evaluating an expression in an external language: {:?}",
            match self {
                EvalError::GeneralEvalError(err) => format!("general error: {}", err),
                EvalError::SyntaxError => "Syntax error".to_owned(),
            }
        )
    }
}
