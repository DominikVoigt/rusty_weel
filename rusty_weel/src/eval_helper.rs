use std::{
    collections::HashMap,
    fmt::Display,
    io::{Read, Seek, Write}, ops::Index,
};

use http_helper::{Client, Parameter};
use log;
use mime::{APPLICATION_JSON, APPLICATION_OCTET_STREAM, TEXT_PLAIN, TEXT_PLAIN_UTF_8};
use reqwest::{header::CONTENT_TYPE, Method};
use serde_json::Value;
use tempfile::tempfile;

use crate::{
    data_types::{DynamicData, StaticData, StatusDTO},
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
    weel_status: Option<StatusDTO>,
    thread_local: &str,
    additional: Value,
    call_result: Option<String>,
    call_headers: Option<HashMap<String, String>>,
    location: &str,
) -> Result<EvaluationResult> {
    log::info!("Evaluating expression: {expression}");
    // This url has to be the full path to the exec-full endpoint
    let mut client = Client::new(
        &static_context.eval_backend_exec_full,
        http_helper::Method::PUT,
    )?;
    {
        // Construct multipart request
        //let expression = encode(expression);
        client.add_parameter(Parameter::SimpleParameter {
            name: "code".to_owned(),
            value: expression.to_owned(),
            param_type: http_helper::ParameterType::Body,
        });
        let data_map: HashMap<String, String> = match serde_yaml::from_str(&dynamic_context.data) {
            Ok(res) => res,
            Err(err) => {
                log::error!(
                    "Failed to deserialize data to send in JSON format: {:?}",
                    err
                );
                panic!(
                    "Failed to deserialize data to send in JSON format: {:?}",
                    err
                )
            }
        };
        client.add_complex_parameter(
            "dataelements",
            APPLICATION_JSON,
            serde_json::to_string_pretty(&data_map)?.as_bytes(),
        )?;

        if !thread_local.is_empty() {
            client.add_complex_parameter("local", APPLICATION_JSON, thread_local.as_bytes())?;
        }

        let endpoints = serde_json::to_string(&dynamic_context.endpoints)?;
        client.add_complex_parameter("endpoints", APPLICATION_JSON, endpoints.as_bytes())?;

        let additional = if additional.is_null() {
            "{}".to_owned()
        } else {
            serde_json::to_string(&additional)?
        };
        client.add_complex_parameter("additional", APPLICATION_JSON, additional.as_bytes())?;

        if let Some(status) = weel_status {
            client.add_complex_parameter(
                "status",
                APPLICATION_JSON,
                serde_json::to_string(&status)?.as_bytes(),
            )?;
        }
        if let Some(call_result) = call_result {
            client.add_complex_parameter(
                "call_result",
                APPLICATION_JSON,
                call_result.as_bytes(),
            )?;
        }

        if let Some(call_headers) = call_headers {
            client.add_complex_parameter(
                "call_headers",
                APPLICATION_JSON,
                serde_json::to_string(&call_headers)?.as_bytes(),
            )?;
        }
    }

    let mut result = client.execute()?;
    let status = result.status_code;
    // Error in the provided code
    log::info!(
        "Received response headers from eval request: {:?}",
        result.headers
    );
    // Get the expressions parameter from the parsed response
    let mut expression_result: Option<String> = None;
    let mut changed_data: Option<HashMap<String, Option<String>>> = None;
    let mut changed_endpoints: Option<HashMap<String, Option<String>>> = None;
    let mut changed_status: Option<StatusDTO> = None;
    let mut data: Option<String> = None;
    let mut endpoints: Option<HashMap<String, String>> = None;
    let mut signal: Option<Signal> = None;
    let mut signal_text: Option<String> = None;

    /*
     * Retrieve the result of the expression
     * Also retrieves the data endpoints state and local data if there is some
     * There will only be this additional data if the corresponding field was changed by the code.
     */
    while let Some(parameter) = result.content.pop() {
        match parameter {
            Parameter::SimpleParameter { name, value, .. } => {
                if name == "result" {
                    expression_result = if value.starts_with("\"") {
                        // In case we have a string, strip them
                        match serde_json::from_str(&value) {
                            Ok(res) => {res},
                            Err(err) => {
                                log::error!("Encountered error deserializing expression: {:?}, received: {}", err, value);
                                return Err(Error::JsonError(err));
                            },
                        }
                    } else {
                        // In case we have a hash
                        Some(value)
                    };
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
                log::info!("Received complex param: name:{name} content:{content}");
                match name.as_str() {
                    "result" => {
                        expression_result = if content.starts_with("\"") {
                            // In case we have a string, strip them
                            match serde_json::from_str(&content) {
                                Ok(res) => res,
                                Err(err) => {
                                    log::error!("Encountered error deserializing expression: {:?}, received: {}", err, content);
                                    return Err(Error::JsonError(err));
                                }
                            }
                        } else {
                            // In case we have a hash
                            Some(content)
                        };
                    }
                    "changed_dataelements" => {
                        changed_data = {
                            match serde_json::from_str(&content) {
                                Ok(res) => res,
                                Err(err) => {
                                    log::error!("Encountered error deserializing changed data elements: {:?}, received: {}", err, content);
                                    return Err(Error::JsonError(err));
                                }
                            }
                        };
                    }
                    "changed_endpoints" => {
                        changed_endpoints = {
                            match serde_json::from_str(&content) {
                                Ok(res) => res,
                                Err(err) => {
                                    log::error!("Encountered error deserializing changed data elements: {:?}, received: {}", err, content);
                                    return Err(Error::JsonError(err));
                                }
                            }
                        };
                    }
                    "changed_status" => {
                        changed_status = {
                            match serde_json::from_str(&content) {
                                Ok(res) => res,
                                Err(err) => {
                                    log::error!("Encountered error deserializing changed data elements: {:?}, received: {}", err, content);
                                    return Err(Error::JsonError(err));
                                }
                            }
                        };
                    }
                    "dataelements" => {
                        data = if content.starts_with("\"") {
                            // In case we have a string, strip them
                            match serde_json::from_str(&content) {
                                Ok(res) => res,
                                Err(err) => {
                                    log::error!("Encountered error deserializing data elements: {:?}, received: {}", err, content);
                                    return Err(Error::JsonError(err));
                                }
                            }
                        } else {
                            // In case we have a hash
                            Some(content)
                        };
                    }
                    "endpoints" => {
                        endpoints = {
                            match serde_json::from_str(&content) {
                                Ok(res) => res,
                                Err(err) => {
                                    log::error!("Encountered error deserializing endpoints: {:?}, received: {}", err, content);
                                    return Err(Error::JsonError(err));
                                }
                            }
                        };
                    }
                    // If this is set -> loop and try again on Signal::Again
                    // Handle others based on ruby code
                    "signal" => {
                        signal = {
                            let signal_enum = if let Some(enum_name) = content.split("::").last() {
                                enum_name.trim()
                            } else {
                                &content
                            };
                            // Enums are serialized as strings!
                            let signal_enum = format!("\"{signal_enum}\"");
                            match serde_json::from_str(&signal_enum) {
                                Ok(res) => res,
                                Err(err) => {
                                    log::error!("Encountered error deserializing signal: {:?}, received: {}", err, content);
                                    return Err(Error::JsonError(err));
                                }
                            }
                        };
                    }
                    "signal_text" => {
                        signal_text = if content.starts_with("\"") {
                            // In case we have a string, strip them
                            match serde_json::from_str(&content) {
                                Ok(res) => res,
                                Err(err) => {
                                    log::error!("Encountered error deserializing signal_text: {:?}, received: {}", err, content);
                                    return Err(Error::JsonError(err));
                                }
                            }
                        } else {
                            // In case we have a hash
                            Some(content)
                        };
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

    let status_not_ok = status < 200 || status >= 300;

    match expression_result {
        Some(expression_result) => {
            let eval_result = EvaluationResult {
                expression_result,
                changed_data,
                changed_endpoints,
                changed_status,
                data,
                endpoints,
            };
            if status_not_ok {
                let signal_text = match signal_text {
                    Some(text) => text,
                    None => "".to_owned(),
                };
                match signal.as_ref() {
                    Some(signal) => {
                        match signal {
                            // Actual signals are handed over as Signals (different from Error::Signal as the eval result will be required here)
                            Signal::Again | Signal::Stop => Err(Error::EvalError(
                                EvalError::Signal(signal.clone(), eval_result),
                            )),
                            Signal::Error => Err(Error::EvalError(EvalError::RuntimeError(
                                format!("{} {}", location, signal_text),
                            ))),
                            // The code related error signals are converted to actual errors and handled separately
                            Signal::SyntaxError => {
                                Err(Error::EvalError(EvalError::SyntaxError(signal_text)))
                            }
                            x => {
                                log::error!(
                                    "Got signaled: {:?} with text: {} when evaluating {}",
                                    x,
                                    signal_text,
                                    expression
                                );
                                Err(Error::EvalError(EvalError::GeneralEvalError(
                                    format!(
                                        "Got signaled: {:?} with text: {} when evaluating {}",
                                        x,
                                        signal_text,
                                        expression))))
                            }
                        }
                    }
                    None => {
                        Err(Error::EvalError(EvalError::GeneralEvalError(
                            "Response of Eval Service is not 2xx, eval result was returned but signal is missing".to_owned())))
                    }
                }
            } else {
                Ok(eval_result)
            }
        }
        None => {
            log::error!(
                "
                Received from evaluation service:
                data:{:?}\n
                endpoints:{:?}\n
                expression_result:{:?}\n
                changed_data:{:?}\n 
                changed_endpoints:{:?}\n
                changed_status:{:?}
                ",
                data,
                endpoints,
                expression_result,
                changed_data,
                changed_endpoints,
                changed_status
            );
            Err(Error::EvalError(EvalError::GeneralEvalError(
            "Response of Eval Service is not 2xx and the body does not contain the evaluation result -> General issue with the service".to_owned())))
        }
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
    pub data: Option<String>,
    pub endpoints: Option<HashMap<String, String>>,
    pub changed_data: Option<HashMap<String, Option<String>>>,
    pub changed_endpoints: Option<HashMap<String, Option<String>>>,
    pub changed_status: Option<StatusDTO>,
}

#[derive(Debug)]
pub enum EvalError {
    GeneralEvalError(String),
    SyntaxError(String),
    RuntimeError(String),
    Signal(Signal, EvaluationResult),
}

impl Display for EvalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Error occured when evaluating an expression in an external language: {:?}",
            match self {
                EvalError::GeneralEvalError(err) => format!("general error: {}", err),
                EvalError::SyntaxError(message) => format!("Syntax error: {message}"),
                EvalError::RuntimeError(err) => format!("Runtime error: {err}"),
                EvalError::Signal(signal, _) => format!("Signal error: {:?}", signal),
            }
        )
    }
}

/**
 * Sends the raw response body and headers to an external ruby service for structurization
 * Receives back an application/json
 */
pub fn structurize_result(
    eval_backend_structurize_url: &str,
    options: &HashMap<String, String>,
    body: &[u8],
) -> Result<String> {
    let mut client = http_helper::Client::new(eval_backend_structurize_url, Method::PUT)?;
    let mut body_file = tempfile()?;
    body_file.write_all(body)?;
    body_file.rewind()?;
    client.add_parameter(Parameter::ComplexParameter {
        name: "body".to_owned(),
        mime_type: TEXT_PLAIN_UTF_8,
        content_handle: body_file,
    });
    client.add_request_headers(options.clone())?;
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
                    log::info!("Structurized result is: {content}");
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

    use crate::data_types::{DynamicData, StaticData, Status};
    use std::io::Write;

    use super::{evaluate_expression, structurize_result};

    #[test]
    fn test_evaluation() {
        init_logger();
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
            cpee_base_url: "".to_owned(),
            redis_url: None,
            redis_path: Some("".to_owned()),
            redis_db: 0,
            redis_workers: 1,
            executionhandlers: "".to_owned(),
            executionhandler: "".to_owned(),
            eval_language: "".to_owned(),
            eval_backend_exec_full: "http://localhost:9302/exec-full".to_owned(),
            eval_backend_structurize: "http://localhost:9302/structurize".to_owned(),
        };
        let status = Status::new(0, "test".to_owned());

        let result = evaluate_expression(
            &dynamic_data,
            &static_data,
            "data.name = 'Tom'",
            Some(status.to_dto()),
            "",
            serde_json::Value::Null,
            None,
            None,
            "",
        )
        .unwrap();
        assert!(result.data.is_some());
        assert!(result.changed_data.is_some());
        assert!(result.endpoints.is_none());
        assert!(result.changed_endpoints.is_none());
        assert!(result.changed_status.is_none());
        println!("Result: {:?}", result)
    }

    #[test]
    fn test_structurize_result() {
        init_logger();
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
        let result = structurize_result(
            "http://localhost:9302/structurize",
            &http_helper::header_map_to_hash_map(&response.headers).unwrap(),
            &response.body,
        )
        .unwrap();
        println!("Result: {result}");
    }

    fn init_logger() -> () {
        env_logger::Builder::from_default_env()
            .filter_level(log::LevelFilter::Debug)
            .format(|buf, record| {
                let style = buf.default_level_style(record.level());
                //buf.default_level_style(record.level());
                writeln!(
                    buf,
                    "{}:{} {} {style}[{}]{style:#} - {}",
                    record.file().unwrap_or("unknown"),
                    record.line().unwrap_or(0),
                    chrono::Local::now().format("%Y-%m-%dT%H:%M:%S"),
                    record.level(),
                    record.args()
                )
            })
            .write_style(env_logger::WriteStyle::Auto)
            .init();
    }
}
