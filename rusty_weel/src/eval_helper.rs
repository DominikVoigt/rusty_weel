use std::{
    collections::HashMap,
    fmt::Display,
    io::{Read, Seek, Write},
};

use http_helper::{Client, Parameter};
use log;
use serde_json::Value;
use tempfile::tempfile;

use crate::{
    data_types::{DynamicData, State, StaticData},
    dsl_realization::{Error, Result, Signal},
};

/**
 * Sends a list of expressions and the context to evaluate them in to the evaluation backend
 * Returns an error if:
 *  - Request body could not be parsed
 *  - Post request fails / Evaluation fails
 */
pub fn evaluate_expression(
    dynamic_context: &DynamicData,
    static_context: &StaticData,
    expression: &str,
    weel_state: &State,
    local: Option<String>,
    additional: Value
) -> Result<EvaluationResult> {
    let mut client = Client::new(&static_context.eval_backend_url, http_helper::Method::POST)?;

    // Construct multipart request from data, expression, state and thread local data: 
    let attributes = serde_json::to_string(&static_context.attributes)?;
    let mut attributes_file = tempfile()?;
    attributes_file.write_all(attributes.as_bytes())?;
    attributes_file.rewind()?;

    let endpoints = serde_json::to_string(&dynamic_context.endpoints)?;
    let mut endpoints_file = tempfile()?;
    endpoints_file.write_all(endpoints.as_bytes())?;
    endpoints_file.rewind()?;

    let mut data_file = tempfile()?;
    data_file.write_all(&dynamic_context.data.as_bytes())?;
    data_file.rewind()?;

    let mut status_file = tempfile()?;
    status_file.write_all(serde_json::to_string(weel_state)?.as_bytes())?;
    status_file.rewind()?;

    let mut code_file = tempfile()?;
    code_file.write_all(expression.as_bytes())?;
    code_file.rewind()?;


    let mut code_file = tempfile()?;
    code_file.write_all(expression.as_bytes())?;
    code_file.rewind()?;
    
    client.add_parameter(Parameter::ComplexParameter {
        name: "endpoints".to_owned(),
        mime_type: mime::APPLICATION_JSON,
        content_handle: endpoints_file,
    });

    client.add_parameter(Parameter::ComplexParameter {
        name: "dataelements".to_owned(),
        mime_type: mime::APPLICATION_JSON,
        content_handle: data_file,
    });

    client.add_parameter(Parameter::ComplexParameter {
        name: "code".to_owned(),
        mime_type: mime::TEXT_PLAIN_UTF_8,
        content_handle: code_file,
    });

    client.add_parameter(Parameter::ComplexParameter {
        name: "additional".to_owned(),
        mime_type: mime::TEXT_PLAIN_UTF_8,
        content_handle: code_file,
    });
    
    
    // -> Optional only for some cases
    client.add_parameter(Parameter::ComplexParameter {
        name: "local".to_owned(),
        mime_type: mime::TEXT_PLAIN_UTF_8,
        content_handle: code_file,
    });

    client.add_parameter(Parameter::ComplexParameter {
        name: "status".to_owned(),
        mime_type: mime::TEXT_PLAIN_UTF_8,
        content_handle: code_file,
    });

    client.add_parameter(Parameter::ComplexParameter {
        name: "info".to_owned(),
        mime_type: mime::TEXT_PLAIN_UTF_8,
        content_handle: code_file,
    });

    client.add_parameter(Parameter::ComplexParameter {
        name: "call_result".to_owned(),
        mime_type: mime::TEXT_PLAIN_UTF_8,
        content_handle: code_file,
    });

    client.add_parameter(Parameter::ComplexParameter {
        name: "call_headers".to_owned(),
        mime_type: mime::TEXT_PLAIN_UTF_8,
        content_handle: code_file,
    });

    if let Some(local) = local {
        let mut local_file = tempfile()?;
        let local = serde_json::to_string(local)?;
        local_file.write_all(local.as_bytes())?;
        local_file.rewind()?;
        client.add_parameter(Parameter::ComplexParameter { name: "local".to_owned(), mime_type: mime::APPLICATION_JSON, content_handle: local_file })
    }


    let mut result = client.execute()?;
    let status = result.status_code;
    // Error in the provided code
    if status == 555 {
        
    } else if status < 100 || status >= 300 {
        
    } 
    // Get the expressions parameter from the parsed response
    let mut expression_results: Option<HashMap<String, String>> = None;
    let mut data: Option<String> = None;
    let mut endpoints: Option<HashMap<String, String>> = None;
    let mut state: Option<State> = None;
    let mut local: Option<HashMap<String, String>> = None;
    let mut signal: Option<Signal> = None;

    /*
     * Retrieve the result of the expression
     * Also retrieves the data endpoints state and local data if there is some
     * There will only be this additional data if the corresponding field was changed by the code.
     */
    while let Some(parameter) = result.content.pop() {
        match parameter {
            Parameter::SimpleParameter { name, value, .. } => {
                if name == "expressions" {
                    expression_results = Some(serde_json::from_str(&value)?);
                } else {
                    continue;
                }
            }
            Parameter::ComplexParameter {
                name,
                mut content_handle,
                ..
            } => {
                let mut content = String::new();
                content_handle.read_to_string(&mut content);
                
                match name.as_str() {
                    "expressions" => {
                        expression_results = Some(serde_json::from_str(&content)?);
                    },
                    "changed_dataelements" => {
                        data = Some(serde_json::from_str(&content)?);
                    },
                    "changed_endpoints" => {
                        endpoints = Some(serde_json::from_str(&content)?);
                    },
                    "changed_status" => {
                        state = Some(serde_json::from_str(&content)?);
                    },
                    // If this is set -> loop and try again on Signal::Again
                    // Handle others based on ruby code
                    "signal" => {
                        signal = Some(serde_json::from_str(&content)?);
                    },
                    "local" => {
                        local = Some(serde_json::from_str(&content)?);
                    },
                    x => {
                        log::info!("Eval endpoint send unexpected part: {x}");
                        log::info!("Content: {}", content);
                        continue;
                    }
                };
            }
        };
    };
    match expression_results {
        Some(expression_results) => Ok(EvaluationResult { expression_results, changed_data: data, changed_endpoints: endpoints, changed_state: state, changed_local: local }),
        None => Err(Error::EvalError(EvalError::GeneralEvalError("Response does not contain the evaluation results".to_owned()))),
    }
}

/**
 * Returns the result of the evaluation,
 * If data, endpoints, state or local information changed, the corresponding field will be Some containing the new value
 * If a field is none, then it did not change
 */
pub struct EvaluationResult {
    expression_results: HashMap<String, String>,
    changed_data: Option<String>,
    changed_endpoints: Option<HashMap<String, String>>,
    changed_state : Option<State>,
    changed_local : Option<HashMap<String, String>>,
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

pub(crate) fn structurize_result(options: HashMap<String, String>, body: &[u8]) -> _ {
    todo!()
}
