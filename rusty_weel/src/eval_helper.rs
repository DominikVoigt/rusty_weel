use std::{
    collections::HashMap,
    fmt::Display,
    fs,
    hash::Hash,
    io::{Read, Seek, Write},
};

use http_helper::{Client, Parameter};
use log;
use mime::{Mime, APPLICATION_JSON, APPLICATION_OCTET_STREAM, APPLICATION_WWW_FORM_URLENCODED};
use reqwest::{header::CONTENT_TYPE, Method};
use serde_json::Value;
use tempfile::tempfile;
use urlencoding::encode;

use crate::{
    data_types::{DynamicData, Status, StaticData},
    dsl_realization::{Error, Result, Signal},
};

/**
 * Sends an expression and the context to evaluate it in to the evaluation backend
 * Returns an error if:
 *  - Request body could not be parsed
 *  - Post request fails / Evaluation fails
 */
pub fn evaluate_expression(
    dynamic_context: &DynamicData,
    static_context: &StaticData,
    expression: &str,
    weel_status: Option<&Status>,
    local: Option<String>,
    additional: Value,
    call_result: Option<String>,
    call_headers: Option<String>
) -> Result<EvaluationResult> {
    let mut client = Client::new(&static_context.eval_backend_url, http_helper::Method::PUT)?;

    {
        // Construct multipart request
        let expression = encode(expression);
        client.add_complex_parameter("code", APPLICATION_WWW_FORM_URLENCODED, expression.as_bytes())?;
        
        client.add_complex_parameter("dataelements", APPLICATION_JSON, dynamic_context.data.as_bytes())?;
        
        if let Some(local) = local {
            client.add_complex_parameter("local", APPLICATION_JSON, local.as_bytes())?;
        }

        let endpoints = serde_json::to_string(&dynamic_context.endpoints)?;
        client.add_complex_parameter("endpoints", APPLICATION_JSON, endpoints.as_bytes())?;
        
        let additional = if additional.is_null() {"{}".to_owned()} else {serde_json::to_string(&additional)?}; 
        client.add_complex_parameter("additional", APPLICATION_JSON, additional.as_bytes())?;
        
        if let Some(status) = weel_status {
            let status = serde_json::to_string(status)?;
            client.add_complex_parameter("status", APPLICATION_JSON, status.as_bytes())?;
        }
        
        if let Some(call_result) = call_result {
            client.add_complex_parameter("call_result", APPLICATION_JSON, call_result.as_bytes())?;
        }

        if let Some(call_headers) = call_headers {
            client.add_complex_parameter("call_headers", APPLICATION_JSON, call_headers.as_bytes())?;
        }
    }
    
    let mut result = client.execute()?;
    let status = result.status_code;
    // Error in the provided code
    if status == 555 {
    } else if status < 100 || status >= 300 {
    }
    println!("{:?}", result.headers);
    // Get the expressions parameter from the parsed response
    let mut expression_result: Option<String> = None;
    let mut data: Option<HashMap<String, String>> = None;
    let mut endpoints: Option<HashMap<String, String>> = None;
    let mut status: Option<Status> = None;
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
                if name == "result" {
                    expression_result = Some(serde_json::from_str(&value)?);
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
                content_handle.read_to_string(&mut content)?;
                
                match name.as_str() {
                    "result" => {
                        expression_result = Some(content);
                    }
                    "changed_dataelements" => {
                        data = Some(serde_json::from_str(&content)?);
                    }
                    "changed_endpoints" => {
                        endpoints = Some(serde_json::from_str(&content)?);
                    }
                    "changed_status" => {
                        status = Some(serde_json::from_str(&content)?);
                    }
                    // If this is set -> loop and try again on Signal::Again
                    // Handle others based on ruby code
                    "signal" => {
                        signal = Some(serde_json::from_str(&content)?);
                    }
                    "local" => {
                        local = Some(serde_json::from_str(&content)?);
                    }
                    x => {
                        log::info!("Eval endpoint send unexpected part: {x}");
                        log::info!("Content: {}", content);
                        continue;
                    }
                };
            }
        };
    }
    match expression_result {
        Some(expression_result) => Ok(EvaluationResult {
            expression_result,
            changed_data: data,
            changed_endpoints: endpoints,
            changed_state: status,
        }),
        None => Err(Error::EvalError(EvalError::GeneralEvalError(
            "Response does not contain the evaluation results".to_owned(),
        ))),
    }
}

/**
 * Returns the result of the evaluation,
 * If data, endpoints, state or local information changed, the corresponding field will be Some containing the new value
 * If a field is none, then it did not change
 */
#[derive(Debug)]
pub struct EvaluationResult {
    pub expression_result: String,
    pub changed_data: Option<HashMap<String, String>>,
    pub changed_endpoints: Option<HashMap<String, String>>,
    pub changed_state: Option<Status>,
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

/**
 * Sends the raw response body and headers to an external ruby service for evaluation
 * Receives back an application/json
 */
pub fn structurize_result(
    eval_backend_url: &str,
    options: &HashMap<String, String>,
    body: &[u8],
) -> Result<String> {
    let mut client = http_helper::Client::new(eval_backend_url, Method::PUT)?;
    client.add_request_header(
        CONTENT_TYPE.as_str(),
        APPLICATION_OCTET_STREAM.essence_str(),
    )?;
    client.add_request_headers(options.clone())?;
    let mut body_file = tempfile()?;
    body_file.write_all(body)?;
    body_file.rewind()?;
    client.add_parameter(Parameter::ComplexParameter {
        name: "body".to_owned(),
        mime_type: APPLICATION_OCTET_STREAM,
        content_handle: body_file,
    });
    let response = client.execute()?;
    let status = response.status_code;
    let mut content = response.content;
    if status == 200 {
        if content.len() != 1 {
            log::error!(
                "Structurization call returned not one but {} parameters",
                content.len()
            );
            Err(Error::GeneralError(format!(
                "Structurization call returned not one but {} parameters",
                content.len()
            )))
        } else {
            Ok(match content.pop().unwrap() {
                Parameter::SimpleParameter { value, .. } => value,
                Parameter::ComplexParameter {
                    mut content_handle, ..
                } => {
                    let mut content = String::new();
                    content_handle.rewind()?;
                    content_handle.read_to_string(&mut content)?;
                    content
                }
            })
        }
    } else {
        log::error!(
            "Structurization call returned with status code {status}. Body: {:?}",
            content
        );
        Err(Error::GeneralError(format!(
            "Call to structurize service was unsuccessful. Code: {status}, Message: {:?}",
            content
        )))
    }
}

#[cfg(test)]
mod test {
    use core::str;
    use std::collections::HashMap;

    use base64::Engine;
    use http_helper::Parameter;
    use reqwest::Method;

    use crate::data_types::{DynamicData, Status, StaticData};

    use super::{evaluate_expression, structurize_result};

    #[test]
    fn test_evaluation() {
        simple_logger::init_with_level(log::Level::Info).unwrap();
        let endpoints = HashMap::new();
        let mut data = HashMap::new();
        data.insert("name".to_owned(), "Testhodor".to_owned());
        data.insert("age".to_owned(), "29".to_owned());
        let data = serde_json::to_string(&data).unwrap();

        let dynamic_data = DynamicData {
            endpoints,
            data: data,
        };

        let static_data = StaticData {
            instance_id: "1".to_owned(),
            host: "".to_owned(),
            base_url: "".to_owned(),
            redis_url: None,
            redis_path: Some("".to_owned()),
            redis_db: 0,
            redis_workers: 1,
            global_executionhandlers: "".to_owned(),
            executionhandlers: "".to_owned(),
            executionhandler: "".to_owned(),
            eval_language: "".to_owned(),
            eval_backend_url: "http://localhost:8550/exec".to_owned(),
            attributes: HashMap::new(),
        };
        let status = Status::new(0, "test".to_owned());

        let result = evaluate_expression(
            &dynamic_data,
            &static_data,
            "data.name = 'Tom'",
            Some(&status),
            None,
            serde_json::Value::Null,
            None,
            None
        )
        .unwrap();
        println!("Result: {:?}", result)
    }

    #[test]
    fn test_structurize_result() {
        let test_endpoint = "http://gruppe.wst.univie.ac.at/~mangler/services/airline.php";
        let params = vec![
            Parameter::SimpleParameter {
                name: "from".to_owned(),
                value: "Vienna".to_owned(),
                param_type: http_helper::ParameterType::Query,
            },
            Parameter::SimpleParameter {
                name: "to".to_owned(),
                value: "Prague".to_owned(),
                param_type: http_helper::ParameterType::Query,
            },
            Parameter::SimpleParameter {
                name: "persons".to_owned(),
                value: "2".to_owned(),
                param_type: http_helper::ParameterType::Query,
            },
        ];

        let mut client = http_helper::Client::new(test_endpoint, Method::POST).unwrap();
        client.add_parameters(params);
        let response = client.execute_raw().unwrap();
        let body = str::from_utf8(&response.body).unwrap();
        println!("Received response: {}", body);
        // SSH connect 8550 to 9302 `ssh -L 8550:localhost:9302 echo`
        let result = structurize_result(
            "http://localhost:8550/structurize",
            &http_helper::header_map_to_hash_map(&response.headers).unwrap(),
            &response.body,
        )
        .unwrap();
        println!("Result: {result}");
        let result = base64::engine::general_purpose::STANDARD
            .decode("aWQ9QVVBJmNvc3RzPTEzNg==")
            .unwrap();
        println!("{}", String::from_utf8_lossy(&result))
    }
}
