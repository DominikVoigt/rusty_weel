use std::{
    collections::HashMap,
    fmt::Display,
    io::{Read, Seek, Write},
    sync::{Mutex, MutexGuard},
    thread,
};

use http_helper::{Client, Parameter};
use lazy_static::lazy_static;
use log;
use mime::{APPLICATION_JSON, TEXT_PLAIN_UTF_8};
use serde_json::Value;
use tempfile::tempfile;

use crate::{
    connection_wrapper::ConnectionWrapper,
    data_types::{Context, Opts, StatusDTO},
    dsl_realization::{Error, Result, Signal},
};

static NUMBER_CLIENTS: u8 = 6;

lazy_static! {
    static ref pool: Vec<Mutex<reqwest::blocking::Client>> = construct_clients(NUMBER_CLIENTS);
}

fn construct_clients(number_clients: u8) -> Vec<Mutex<reqwest::blocking::Client>> {
    let mut new_pool = Vec::new();
    for _ in 0..number_clients {
        new_pool.push(Mutex::new(reqwest::blocking::Client::new()));
    }
    new_pool
}

pub fn test_condition(
    dynamic_context: &Context,
    static_context: &Opts,
    code: &str,
    thread_local: &Option<Value>,
    connection_wrapper: &ConnectionWrapper,
) -> Result<bool> {
    let ex_client = get_client();

    let mut client = Client::new_with_existing_client(
        &static_context.eval_backend_exec_full,
        http_helper::Method::PUT,
        ex_client,
    )?;
    // Construct multipart request
    client.add_parameter(Parameter::SimpleParameter {
        name: "code".to_owned(),
        value: code.to_owned(),
        param_type: http_helper::ParameterType::Body,
    });
    client.add_complex_parameter(
        "dataelements",
        APPLICATION_JSON,
        serde_json::to_string_pretty(&dynamic_context.data)?.as_bytes(),
    )?;

    if let Some(context) = thread_local {
        client.add_complex_parameter(
            "local",
            APPLICATION_JSON,
            serde_json::to_string_pretty(context)?.as_bytes(),
        )?;
    }

    let endpoints = serde_json::to_string(&dynamic_context.endpoints)?;
    client.add_complex_parameter("endpoints", APPLICATION_JSON, endpoints.as_bytes())?;
    let additional = connection_wrapper.additional();
    let additional = if additional.is_null() {
        "{}".to_owned()
    } else {
        serde_json::to_string(&additional)?
    };
    client.add_complex_parameter("additional", APPLICATION_JSON, additional.as_bytes())?;

    let mut result = client.execute()?;

    let status = result.status_code;

    let status_ok = status >= 200 || status < 300;
    if status_ok {
        let mut eval_res: Option<bool> = None;
        while let Some(parameter) = result.content.pop() {
            match parameter {
                Parameter::SimpleParameter { name, value, .. } => {
                    if name == "result" {
                        let value = value.replace("\"", "");
                        if value.len() == 0 {
                            return Err(Error::EvalError(EvalError::SyntaxError(
                                "Provided code is not an expression! Evaluation returned empty"
                                    .to_owned(),
                            )));
                        }
                        // In case we have a string, strip them
                        match serde_json::from_str(&value) {
                            Ok(res) => eval_res = Some(res),
                            Err(err) => {
                                log::error!(
                                    "Encountered error deserializing expression: {:?}, received: {}",
                                    err,
                                    value
                                );
                                return Err(Error::JsonError(err));
                            }
                        }
                        break;
                    } else {
                        log::error!(
                            "Received simple parameter with name {}. We ignore these currently",
                            name
                        );
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
                    let content = content.replace("\"", "");
                    if content.len() == 0 {
                        return Err(Error::EvalError(EvalError::SyntaxError(
                            "Provided code is not an expression! Evaluation returned empty"
                                .to_owned(),
                        )));
                    }
                    match name.as_str() {
                        "result" => {
                            // In case we have a string, strip them
                            match serde_json::from_str(&content) {
                                Ok(res) => eval_res = Some(res),
                                Err(err) => {
                                    log::error!("Encountered error deserializing expression: {:?}, received: {}", err, content);
                                    return Err(Error::JsonError(err));
                                }
                            };
                            break;
                        }
                        x => {
                            log::info!("Skipping entry: {x}");
                            continue;
                        }
                    };
                }
            };
        }
        let condition = match eval_res {
            Some(x) => x,
            None => {
                return Err(Error::EvalError(EvalError::GeneralEvalError(
                    "Response for evaluation request returned without result parameter".to_owned(),
                )));
            }
        };
        connection_wrapper.gateway_decide(thread::current().id(), code, condition)?;
        Ok(condition)
    } else {
        let mut signal: Option<Signal> = None;
        let mut signal_text: Option<String> = None;
        while let Some(param) = result.content.pop() {
            let p_name: String;
            let mut p_content = String::new();
            match param {
                Parameter::SimpleParameter { name, value, .. } => {
                    p_name = name;
                    p_content = value;
                }
                Parameter::ComplexParameter {
                    name,
                    mut content_handle,
                    ..
                } => {
                    p_name = name;
                    content_handle.read_to_string(&mut p_content)?;
                }
            };
            match p_name.as_str() {
                // If this is set -> loop and try again on Signal::Again
                // Handle others based on ruby code
                "signal" => {
                    signal = {
                        let signal_enum = if let Some(enum_name) = p_content.split("::").last() {
                            enum_name.trim()
                        } else {
                            &p_content
                        };
                        // Enums are serialized as strings!
                        let signal_enum = format!("\"{signal_enum}\"");
                        match serde_json::from_str(&signal_enum) {
                            Ok(res) => res,
                            Err(err) => {
                                log::error!(
                                    "Encountered error deserializing signal: {:?}, received: {}",
                                    err,
                                    p_content
                                );
                                return Err(Error::JsonError(err));
                            }
                        }
                    };
                }
                "signal_text" => {
                    signal_text = if p_content.starts_with("\"") {
                        // In case we have a string, strip them
                        match serde_json::from_str(&p_content) {
                            Ok(res) => res,
                            Err(err) => {
                                log::error!("Encountered error deserializing signal_text: {:?}, received: {}", err, p_content);
                                return Err(Error::JsonError(err));
                            }
                        }
                    } else {
                        // In case we have a hash
                        Some(p_content)
                    };
                }
                x => {
                    log::info!("Skipping param: {x}");
                }
            }
        }
        let signal_text = match signal_text {
            Some(text) => text,
            None => "".to_owned(),
        };
        match signal.as_ref() {
            Some(signal) => {
                match signal {
                    // Actual signals are handed over as Signals (different from Error::Signal as the eval result will be required here)
                    Signal::Error => Err(Error::EvalError(EvalError::RuntimeError(format!(
                        "{} {}",
                        "Condition", signal_text
                    )))),
                    // The code related error signals are converted to actual errors and handled separately
                    Signal::SyntaxError => {
                        Err(Error::EvalError(EvalError::SyntaxError(signal_text)))
                    }
                    x => {
                        log::error!(
                            "Got signaled: {:?} with text: {} when evaluating {}",
                            x,
                            signal_text,
                            code
                        );
                        Err(Error::EvalError(EvalError::GeneralEvalError(format!(
                            "Got signaled: {:?} with text: {} when evaluating {}",
                            x, signal_text, code
                        ))))
                    }
                }
            }
            None => Err(Error::EvalError(EvalError::GeneralEvalError(format!(
                "Response of Eval Service is {}, eval result was returned but signal is missing",
                status
            )))),
        }
    }
}

fn get_client() -> MutexGuard<'static, reqwest::blocking::Client> {
    let client ;
    // try locking a client
    'spin: loop {
        for client_mut in pool.iter() {
            match client_mut.try_lock() {
                Ok(lock) => {
                    client = lock;
                    break 'spin;
                },
                Err(_) => {}
            };
        }
    };
    return client;
}

/**
 * Sends an expression and the context to evaluate it in to the evaluation backend
 * Returns an error if:
 *  - Request body could not be parsed
 *  - Post request fails / Evaluation fails
 */
pub fn evaluate_expression(
    dynamic_context: &Context,
    static_context: &Opts,
    expression: &str,
    weel_status: Option<StatusDTO>,
    thread_local: &Option<Value>,
    additional: Value,
    call_result: Option<String>,
    call_headers: Option<HashMap<String, String>>,
    location: &str,
) -> Result<EvaluationResult> {
    log::debug!("evaluating expression: {expression}");
    // This url has to be the full path to the exec-full endpoint
    let ex_client = get_client();

    let mut client = Client::new_with_existing_client(
        &static_context.eval_backend_exec_full,
        http_helper::Method::PUT,
        ex_client,
    )?;
    {
        // Construct multipart request
        client.add_parameter(Parameter::SimpleParameter {
            name: "code".to_owned(),
            value: expression.to_owned(),
            param_type: http_helper::ParameterType::Body,
        });
        client.add_complex_parameter(
            "dataelements",
            APPLICATION_JSON,
            serde_json::to_string_pretty(&dynamic_context.data)?.as_bytes(),
        )?;

        if let Some(context) = thread_local {
            client.add_complex_parameter(
                "local",
                APPLICATION_JSON,
                serde_json::to_string_pretty(context)?.as_bytes(),
            )?;
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
    // Get the expressions parameter from the parsed response
    let mut expression_result: Option<Value> = None;
    let mut changed_data: Option<Value> = None;
    let mut changed_endpoints: Option<HashMap<String, Option<String>>> = None;
    let mut changed_status: Option<StatusDTO> = None;
    let mut data: Option<Value> = None;
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
                    if value.len() == 0 {
                        expression_result = Some(Value::Null);
                    } else {
                        // In case we have a string, strip them
                        match serde_json::from_str(&value) {
                            Ok(res) => expression_result = Some(res),
                            Err(err) => {
                                log::error!(
                                    "Encountered error deserializing expression: {:?}, received: {}",
                                    err,
                                    value
                                );
                                return Err(Error::JsonError(err));
                            }
                        }
                    }
                } else {
                    log::error!(
                        "Received simple parameter with name {}. We ignore these currently",
                        name
                    );
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
                        if content.len() == 0 {
                            expression_result = Some(Value::Null)
                        } else {
                            // In case we have a string, strip them
                            match serde_json::from_str(&content) {
                                Ok(res) => expression_result = Some(res),
                                Err(err) => {
                                    log::error!("Encountered error deserializing expression: {:?}, received: {}", err, content);
                                    return Err(Error::JsonError(err));
                                }
                            }
                        }
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
                        data = match serde_json::from_str(&content) {
                            Ok(res) => res,
                            Err(err) => {
                                log::error!("Encountered error deserializing data elements: {:?}, received: {}", err, content);
                                return Err(Error::JsonError(err));
                            }
                        }
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
                        log::info!("Skipping param: {x}");
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
                            format!("Response of Eval Service is {}, eval result was returned but signal is missing", status))))
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
            format!("Response of Eval Service is {} and the body does not contain the evaluation result -> General issue with the service", status))))
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
    pub expression_result: Value,
    pub data: Option<Value>,
    pub endpoints: Option<HashMap<String, String>>,
    pub changed_data: Option<Value>,
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
    let mut client =
        http_helper::Client::new(eval_backend_structurize_url, http_helper::Method::PUT)?;
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
