use std::{
    collections::HashMap, fmt::Display, fs, hash::Hash, io::{Read, Seek, Write}
};

use http_helper::{Client, Parameter};
use log;
use mime::APPLICATION_OCTET_STREAM;
use reqwest::{header::CONTENT_TYPE, Method};
use serde_json::Value;
use tempfile::tempfile;
use urlencoding::encode;

use crate::{
    data_types::{DynamicData, State, StaticData},
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
    weel_state: &State,
    local: Option<String>,
    additional: Value
) -> Result<EvaluationResult> {
    let mut client = Client::new(&static_context.eval_backend_url, http_helper::Method::PUT)?;

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

    
    let mut additional_file = tempfile()?;
    additional_file.write_all(serde_json::to_string(&additional)?.as_bytes())?;
    additional_file.rewind()?;
    
    let mut code_file = tempfile()?;
    let expression = encode(expression);
    code_file.write_all(expression.as_bytes())?;
    code_file.rewind()?;
    
    client.add_parameter(Parameter::ComplexParameter {
        name: "code".to_owned(),
        mime_type: mime::APPLICATION_WWW_FORM_URLENCODED,
        content_handle: code_file,
    });

    client.add_parameter(Parameter::ComplexParameter {
        name: "dataelements".to_owned(),
        mime_type: mime::APPLICATION_JSON,
        content_handle: data_file,
    });

    client.add_parameter(Parameter::ComplexParameter {
        name: "endpoints".to_owned(),
        mime_type: mime::APPLICATION_JSON,
        content_handle: endpoints_file,
    });
    
    
    client.add_parameter(Parameter::ComplexParameter {
        name: "additional".to_owned(),
        mime_type: mime::APPLICATION_JSON,
        content_handle: additional_file,
    });
    
    
    // -> Optional only for some cases
    if let Some(local) = local {
        let mut local_file = tempfile()?;
        local_file.write_all(local.as_bytes())?;
        local_file.rewind()?;
        
        client.add_parameter(Parameter::ComplexParameter {
            name: "local".to_owned(),
            mime_type: mime::APPLICATION_JSON,
            content_handle: local_file,
        });
    }

    client.add_parameter(Parameter::ComplexParameter {
        name: "status".to_owned(),
        mime_type: mime::APPLICATION_JSON,
        content_handle: status_file,
    });
        
    /* TODO: Add all these optional ones
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
        local_file.write_all(local.as_bytes())?;
        local_file.rewind()?;
        client.add_parameter(Parameter::ComplexParameter { name: "local".to_owned(), mime_type: mime::APPLICATION_JSON, content_handle: local_file })
    }
    */

    let mut result = client.execute()?;
    let status = result.status_code;
    // Error in the provided code
    if status == 555 {
        
    } else if status < 100 || status >= 300 {
        
    } 
    // Get the expressions parameter from the parsed response
    let mut expression_result: Option<String> = None;
    let mut data: Option<HashMap<String, String>> = None;
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
    match expression_result {
        Some(expression_result) => Ok(EvaluationResult { expression_result, changed_data: data, changed_endpoints: endpoints, changed_state: state }),
        None => Err(Error::EvalError(EvalError::GeneralEvalError("Response does not contain the evaluation results".to_owned()))),
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
    pub changed_state : Option<State>,
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
pub fn structurize_result(eval_backend_url: &str, options: &HashMap<String, String>, body: &[u8]) -> Result<String> {
    let mut client = http_helper::Client::new(eval_backend_url, Method::PUT)?;
    client.add_request_header(CONTENT_TYPE.as_str(), APPLICATION_OCTET_STREAM.essence_str())?;
    client.add_request_headers(options.clone())?;
    let mut body_file = tempfile()?;
    body_file.write_all(body)?;
    body_file.rewind()?;
    client.add_parameter(Parameter::ComplexParameter { name: "body".to_owned(), mime_type: APPLICATION_OCTET_STREAM, content_handle: body_file });
    let response = client.execute()?;
    let status = response.status_code;
    let mut content = response.content;
    if status == 200 {
        if content.len() != 1 {
            log::error!("Structurization call returned not one but {} parameters", content.len());
            Err(Error::GeneralError(format!("Structurization call returned not one but {} parameters", content.len())))
        } else {
            Ok(match content.pop().unwrap() {
                Parameter::SimpleParameter { value, ..} => value,
                Parameter::ComplexParameter { mut content_handle, .. } => {let mut content = String::new(); content_handle.rewind()?; content_handle.read_to_string(&mut content)?; content},
            })
        }
    } else {
        log::error!("Structurization call returned with status code {status}. Body: {:?}", content);
        Err(Error::GeneralError(format!("Call to structurize service was unsuccessful. Code: {status}, Message: {:?}", content)))
    }
}

#[cfg(test)]
mod test {
    use core::str;
    use std::collections::HashMap;

    use base64::Engine;
    use http_helper::Parameter;
    use mime::{Mime, TEXT_PLAIN_UTF_8};
    use reqwest::Method;

    use crate::data_types::{DynamicData, State, StaticData};

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
        let state = State::Running;

        let result = evaluate_expression(&dynamic_data, &static_data, "data.name = 'Tom'", &state, None, serde_json::Value::Null).unwrap();
        println!("Result: {:?}", result)
    }


    #[test]
    fn test_structurize_result() {
        let test_endpoint = "http://gruppe.wst.univie.ac.at/~mangler/services/airline.php";
        let params = vec![
            Parameter::SimpleParameter { name: "from".to_owned(), value: "Vienna".to_owned(), param_type: http_helper::ParameterType::Query },
            Parameter::SimpleParameter { name: "to".to_owned(), value: "Prague".to_owned(), param_type: http_helper::ParameterType::Query },
            Parameter::SimpleParameter { name: "persons".to_owned(), value: "2".to_owned(), param_type: http_helper::ParameterType::Query }
        ];

        let mut client = http_helper::Client::new(test_endpoint, Method::POST).unwrap();
        client.add_parameters(params);
        let response = client.execute_raw().unwrap(); 
        let body = str::from_utf8(&response.body).unwrap();
        println!("Received response: {}", body);
        // SSH connect 8550 to 9302 `ssh -L 8550:localhost:9302 echo`
        let result = structurize_result("http://localhost:8550/structurize", &http_helper::header_map_to_hash_map(&response.headers).unwrap(), &response.body).unwrap();
        println!("Result: {result}");
        let result = base64::engine::general_purpose::STANDARD.decode("aWQ9QVVBJmNvc3RzPTEzNg==").unwrap();
        println!("{}", String::from_utf8_lossy(&result))
    }
}
