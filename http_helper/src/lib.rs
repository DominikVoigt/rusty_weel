use bytes::Buf;
use derive_more::From;
use mime::{FromStrError, Mime, APPLICATION_OCTET_STREAM, BOUNDARY};
use multipart::server::{FieldHeaders, ReadEntry};
use reqwest::{
    blocking::{
        multipart::{Form, Part},
        RequestBuilder,
    },
    header::{HeaderMap, HeaderName, HeaderValue, ToStrError, CONTENT_TYPE},
    Method, Url,
};
use std::{
    fs,
    io::{Read, Seek, Write},
    str::FromStr,
};

use urlencoding::encode;

#[derive(Debug, Clone)]
pub enum ParameterType {
    Query,
    Body,
}

// Simple kv parameters, can be in body or url
// If a request is a get request -> All parameters into the query
// The name and value do not have to be escaped yet -> Part of generate_url

#[derive(Debug)]
pub enum Parameter {
    SimpleParameter {
        name: String,
        // Option to enable query parameters without a value
        value: String,
        param_type: ParameterType,
    },

    // Since File is not cloneable, we do not merge simple and complex parameters into an enum
    // For sending/receiving files
    ComplexParameter {
        name: String,
        mime_type: String,
        content_handle: fs::File,
    },
}

#[derive(Debug, Clone)]
pub enum HttpVerb {
    GET,
    PUT,
    POST,
    DELETE,
    PATCH,
}

enum Headers {
    HeaderMap(HeaderMap),
    PartHeaders(FieldHeaders),
}

pub struct RawResponse {
    pub headers: HeaderMap,
    pub body: bytes::Bytes,
    pub status_code: u16,
}


pub struct ParsedResponse {
    pub headers: HeaderMap,
    pub content: Vec<Parameter>,
    pub status_code: u16,
}

/**
 * Abstraction on top of libcurl
 */
pub struct Client {
    method: Method,
    reqwest_client: reqwest::blocking::Client,
    base_url: Url,
    headers: HeaderMap,
    parameters: Vec<Parameter>,
}

#[derive(From, Debug)]
pub enum Error {
    ReqwestGeneralError(reqwest::Error),
    ReqwestInvalidHeaderName(reqwest::header::InvalidHeaderName),
    ReqwestInvalidHeaderValue(reqwest::header::InvalidHeaderValue),
    UrlParserError(url::ParseError),
    FileError(std::io::Error),
    ToStringError(ToStrError),
    HeaderParseError(String),
    MimeParseError(FromStrError),
    Utf8Error(std::str::Utf8Error),
}

type Result<T> = std::result::Result<T, Error>;

/**
 * Executes HTTP requests:
 *
 *  - SimpleParameters for query parameters and application/x-www-form-urlencoded
 *  - ComplexParameters for files (content-type of header/part-header is mime-type)
 *  - If multiple parameters are provided, then a multipart request is send
 *  - If the query string contains query parameters they are parsed into SimpleParameters (with ParameterType Query)
 *  - SimpleParameter name and value are URL encoded
 *  -   
 */
impl Client {
    pub fn new(url: &str, method: Method) -> Result<Client> {
        let client = reqwest::blocking::Client::new();
        let (base_url, parameters) = generate_base_url(url)?;
        let mut client = Client {
            method,
            reqwest_client: client,
            base_url,
            headers: HeaderMap::new(),
            // All simple parameters are URL encoded -> If added through add_parameters
            parameters: Vec::new(),
        };
        // Add clients via add to url encode them
        client.add_parameters(parameters);
        Ok(client)
    }

    /**
     * Will add the parameter to the client parameters for the request
     * If the parameter is a simple parameter, it will be URL encoded
     */
    pub fn add_parameter(&mut self, mut parameter: Parameter) {
        parameter = match parameter {
            Parameter::SimpleParameter {
                name,
                value,
                param_type,
            } => Parameter::SimpleParameter {
                name: encode(&name).to_string(),
                value: encode(&value).to_string(),
                param_type,
            },
            Parameter::ComplexParameter {
                name,
                mime_type,
                content_handle,
            } => Parameter::ComplexParameter {
                name,
                mime_type,
                content_handle,
            },
        };
        self.parameters.push(parameter);
    }

    pub fn add_parameters(&mut self, parameters: Vec<Parameter>) {
        parameters.into_iter().for_each(|parameter| {
            let _ = self.add_parameter(parameter);
        });
    }

    pub fn set_request_headers(&mut self, request_headers: HeaderMap) {
        self.headers = request_headers;
    }

    /**
     * Inserts the header and replace the previous value. Currently not supporting multi valued headers
     * If header parameters are desired, provide them as part of the value (delimited by the ;)
     *
     * Returns Some(value) of the previous header with the same name if one was present, otherwise None
     *
     * Only visible ASCII characters (32-127) are permitted. Use
     * `from_bytes` to create a `HeaderValue` that includes opaque octets
     * (128-255).
     */
    pub fn add_request_headers(&mut self, name: &str, value: &str) -> Result<Option<HeaderValue>> {
        Ok(self
            .headers
            .insert(HeaderName::from_str(name)?, HeaderValue::from_str(value)?))
    }

    /**
     * Generates the complete request URL including the query parameters.
     * Query parameters are constructed from the SimpleParameters with ParameterType Body.
     */
    fn generate_url(&self) -> Url {
        let mut query_params = Vec::new();
        self.parameters
            .iter()
            .for_each(|parameter| match parameter {
                Parameter::SimpleParameter {
                    name,
                    value,
                    param_type,
                } => {
                    let is_query_param = matches!(param_type, ParameterType::Query);
                    if is_query_param {
                        if value.len() == 0 {
                            query_params.push(name.clone());
                        } else {
                            query_params.push(format!("{}={}", name, value));
                        };
                    }
                }
                _ => (),
            });
        let query_string = query_params.join("&");
        let url = format!("{}?{}", self.base_url.as_str(), query_string);
        Url::parse(&url).expect("Cannot happen")
    }

    /**
     * Given the parameters/method the body and relevant headers are adjusted
     * After this method, the parameters field is left as an empty vector
     */
    fn generate_body(&mut self, request_builder: RequestBuilder) -> Result<RequestBuilder> {
        let parameters: Vec<Parameter> = std::mem::replace(&mut self.parameters, Vec::new());
        let mut body_parameters: Vec<Parameter> = parameters
            .into_iter()
            .filter(|parameter| match parameter {
                Parameter::SimpleParameter { param_type, .. } => {
                    matches!(param_type, ParameterType::Body)
                }
                Parameter::ComplexParameter { .. } => true,
            })
            .collect();
        if body_parameters.len() == 1 {
            Ok(construct_singular_body(
                body_parameters.pop().expect("Cannot fail"),
                request_builder,
            ))
        } else {
            Ok(construct_multipart(body_parameters, request_builder)?)
        }
    }

    /**
     * Will execute the request and return the RawResponse
     * Requires the target to send headers that only contain visible ascii
     */
    fn execute_raw(mut self) -> Result<RawResponse> {
        // For now: Explicitly passing simple parameters of desired type self.mark_query_parameters();
        let url = self.generate_url();
        let mut request_builder = self.reqwest_client.request(self.method.clone(), url);
        request_builder = self.set_headers(request_builder);
        request_builder = self.generate_body(request_builder)?;
        let request = request_builder.build()?;
        let response = self.reqwest_client.execute(request)?;

        Ok(RawResponse {
            headers: response.headers().clone(),
            status_code: response.status().as_u16(),
            body: response.bytes()?,
        })
    }

    /**
     * Executes the request and consumes the client as the headers and parameters are consumed by the request
     */
    pub fn execute(self) -> Result<ParsedResponse> {
        // TODO: What to do in case of a different status_code
        let raw = self.execute_raw()?;

        raw.parse_response()
    }

    /**
     *  Sets the headers of the request_builder.
     *  After this method the headers field is left as an empty map
     */
    fn set_headers(&mut self, request_builder: RequestBuilder) -> RequestBuilder {
        let headers: HeaderMap = std::mem::replace(&mut self.headers, HeaderMap::new());
        request_builder.headers(headers)
    }
}

fn generate_base_url(url_str: &str) -> Result<(Url, Vec<Parameter>)> {
    // Remove trailing whitespace
    let url_str = url_str.trim_end();
    let split = url_str.split_once("?");
    match split {
        Some((base_url_str, query_str)) => {
            let base_url = Url::parse(base_url_str)?;
            let parameters = parse_query_string(query_str);
            Ok((base_url, parameters))
        }
        // URL does not contain any query parameters
        None => Ok((Url::parse(url_str)?, Vec::new())),
    }
}

/**
 * Parses the query string (the section after <base-url>?)
 * Will not URL encode it
 */
fn parse_query_string(query: &str) -> Vec<Parameter> {
    query
        .split("&")
        .map(|parameter| {
            match parameter.split_once("=") {
                // Key value pair
                Some((name, value)) => Parameter::SimpleParameter {
                    name: name.to_owned(),
                    value: value.to_owned(),
                    param_type: ParameterType::Query,
                },
                // query parameter without a value
                None => Parameter::SimpleParameter {
                    name: parameter.to_owned(),
                    value: String::new(),
                    param_type: ParameterType::Query,
                },
            }
        })
        .collect()
}

/**
 * called when only a single simple body parameter or a single complex parameter is passed to the client (after transforming simple parameters into query parameters for GET calls)
 * If a simple parameter is provided, its name and value (if set) have to be url encoded
 *
 * This will also set the corresponding content headers
 */
fn construct_singular_body(
    parameter: Parameter,
    request_builder: RequestBuilder,
) -> RequestBuilder {
    match parameter {
        Parameter::SimpleParameter { name, value, .. } => {
            let text = if value.len() == 0 {
                name
            } else {
                format!("{}={}", name, value)
            };
            request_builder
                .body(text)
                // Need to provide content_type but not content-length
                .header(
                    CONTENT_TYPE,
                    // TODO: Is this the correct mimetype?
                    mime::APPLICATION_WWW_FORM_URLENCODED.to_string(),
                )
        }
        Parameter::ComplexParameter {
            mime_type,
            content_handle,
            ..
        } => request_builder
            .header(CONTENT_TYPE, mime_type)
            .body(content_handle),
    }
}

fn construct_multipart(
    parameters: Vec<Parameter>,
    mut request_builder: RequestBuilder,
) -> Result<RequestBuilder> {
    request_builder = request_builder.header(CONTENT_TYPE, mime::MULTIPART_FORM_DATA.to_string());
    let mut form = Form::new();
    for parameter in parameters {
        match parameter {
            Parameter::SimpleParameter { name, value, .. } => {
                // A simple parameter without a value will result in a part without a part body
                let part = Part::text(value).mime_str(&mime::TEXT_PLAIN_UTF_8.to_string())?;
                form = form.part(name, part);
            }
            Parameter::ComplexParameter {
                name,
                mime_type,
                content_handle,
            } => {
                let part = Part::reader(content_handle).mime_str(&mime_type)?;
                form = form.part(name, part);
            }
        }
    }
    Ok(request_builder.multipart(form))
}

impl RawResponse {
    fn parse_response(self) -> Result<ParsedResponse> {
        Ok(ParsedResponse {
            headers: self.headers.clone(),
            content: parse_part(Headers::HeaderMap(self.headers), &self.body)?,
            status_code: self.status_code,
        })
    }
}

/**
 * Parses the response with their corresponding headers and body
 * For non-multipart responses this will terminate after one method invocation
 * For multipart responses this is called recursive for each part.
 */
fn parse_part(headers: Headers, body: &[u8]) -> Result<Vec<Parameter>> {
    let (name, content_type) = get_name_and_content_type(&headers)?;
    // We use essence_str to remove any attached parameters for this comparison
    if content_type.essence_str() == mime::MULTIPART_FORM_DATA {
        let boundary = content_type
            .get_param(BOUNDARY)
            .ok_or(Error::HeaderParseError(
                "Content type multipart/form-data misses boundary parameter".to_owned(),
            ))?
            .to_string();
        parse_multipart(body, &boundary)
    } else if content_type.essence_str() == mime::APPLICATION_WWW_FORM_URLENCODED {
        parse_form_urlencoded(body)
    } else {
        parse_flat_data(&content_type.to_string(), body, &name)
    }
}

fn get_name_and_content_type(headers: &Headers) -> Result<(String, Mime)> {
    let name = match headers {
        Headers::HeaderMap(headers) => match headers.get(HeaderName::from_str("content-id")?) {
            Some(content_id) => content_id.to_str()?.trim().replace("\"", ""),
            None => "result".to_owned(),
        },
        Headers::PartHeaders(headers) => {
            // Get arc as ref
            let mut name = &*headers.name;
            if name.len() == 0 {
                name = "result";
            }
            name.to_owned()
        }
    };
    let content_type = match headers {
        Headers::HeaderMap(headers) => match headers.get(CONTENT_TYPE) {
            Some(content_type) => content_type.to_str()?.trim().parse::<mime::Mime>()?,
            None => APPLICATION_OCTET_STREAM,
        }
        .to_owned(),
        Headers::PartHeaders(headers) => headers
            .content_type
            .clone()
            .unwrap_or(APPLICATION_OCTET_STREAM),
    };
    Ok((name, content_type))
}

/**
 * Parses content into a single complex parameter
 */
fn parse_flat_data(content_type: &str, body: &[u8], name: &str) -> Result<Vec<Parameter>> {
    let mut content = tempfile::tempfile()?;
    content.write(&body)?;
    content.rewind()?;
    Ok(vec![Parameter::ComplexParameter {
        name: name.to_owned(),
        mime_type: content_type.to_owned(),
        content_handle: content,
    }])
}

/**
 * Parses content into list of simple parameters (& separated sequence)
 */
fn parse_form_urlencoded(body: &[u8]) -> Result<Vec<Parameter>> {
    let mut parameters = Vec::new();
    form_urlencoded::parse(&body).for_each(|pair| {
        parameters.push(Parameter::SimpleParameter {
            name: (*pair.0).to_owned(),
            value: (*pair.1).to_owned(),
            param_type: ParameterType::Body,
        });
    });
    Ok(parameters)
}

fn parse_multipart(body: &[u8], boundary: &str) -> Result<Vec<Parameter>> {
    let mut parameters: Vec<Parameter> = Vec::new();
    let mut multipart = multipart::server::Multipart::with_body(body.reader(), boundary);
    loop {
        let part = multipart.read_entry_mut();
        match part {
            multipart::server::ReadEntryResult::Entry(mut entry) => {
                let mut body: Vec<u8> = Vec::new();
                entry.data.read_to_end(&mut body)?;
                parameters.extend(parse_part(Headers::PartHeaders(entry.headers), &body)?)
            }
            multipart::server::ReadEntryResult::End(_) => return Ok(parameters),
            multipart::server::ReadEntryResult::Error(_, error) => return Err(Error::from(error)),
        };
    }
}

#[cfg(test)]
mod test {
    use std::io::{Read, Seek};
    use super::*;

    mod test_creation {
        use super::*;

        #[test]
        fn test_url_generation() -> Result<()> {
            let test_url = "https://www.testing.com?test=value&tes. == aba\"";
            let client = Client::new(test_url, Method::GET)?;
            println!("{:?}", client.parameters);
            assert_eq!(
                // checked with urlencoder.org
                "https://www.testing.com/?test=value&tes.%20=%3D%20aba%22",
                client.generate_url().to_string()
            );
            Ok(())
        }

        #[test]
        fn test_url_generation_with_params() -> Result<()> {
            let test_url = "https://www.testing.com/?test=value&onlyname";
            let mut client = Client::new(test_url, Method::GET)?;
            let parameters = vec![
                Parameter::SimpleParameter {
                    name: "a".to_owned(),
                    value: "a1".to_owned(),
                    param_type: ParameterType::Body,
                },
                Parameter::SimpleParameter {
                    name: "b".to_owned(),
                    value: "b1".to_owned(),
                    param_type: ParameterType::Query,
                },
                Parameter::SimpleParameter {
                    name: "c".to_owned(),
                    value: "".to_owned(),
                    param_type: ParameterType::Query,
                },
            ];
            client.add_parameters(parameters);

            assert_eq!(
                "https://www.testing.com/?test=value&onlyname&b=b1&c",
                client.generate_url().to_string()
            );
            Ok(())
        }

        // requires netcat to run on localhost 5678
        // tested by looking at the generated requests by netcat
        // Results can be found in the test_files/http/request/simple_singular_request.txt
        // For now is only used to generate bodies and manual inspection
        #[test]
        fn test_building_singular_simple_body() -> Result<()> {
            let test_url = "http://localhost:5678";
            let mut client = Client::new(test_url, Method::POST)?;
            client.add_parameter(Parameter::SimpleParameter {
                name: "simple_param_ test".to_owned(),
                value: "simple_value".to_owned(),
                param_type: ParameterType::Body,
            });
            let mut request_builder = client.reqwest_client.request(Method::POST, test_url);
            request_builder = client.generate_body(request_builder)?;
            let request = request_builder.build()?;
            let response = client.reqwest_client.execute(request)?;
            assert_eq!(response.status().as_u16(), 200);
            println!("{:?}", response.text().unwrap());
            Ok(())
        }

        // requires netcat to run on localhost 5678
        // tested by looking at the generated requests by netcat
        // Results can be found in the test_files/http/request/text_file_singular_request.txt
        // For now is only used to generate bodies and manual inspection
        #[test]
        fn test_building_singular_complex_text_body() -> Result<()> {
            let test_url = "http://localhost:5678";
            let mut client = Client::new(test_url, Method::POST)?;
            let file = "./test_files/text/file_example.xml";
            let file = fs::File::open(file)?;

            client.add_parameter(Parameter::ComplexParameter {
                name: "test_file".to_owned(),
                mime_type: mime::TEXT_XML.to_string(),
                content_handle: file,
            });
            let mut request_builder = client.reqwest_client.request(Method::POST, test_url);
            request_builder = client.generate_body(request_builder)?;
            let request = request_builder.build()?;
            let response = client.reqwest_client.execute(request)?;
            assert_eq!(response.status().as_u16(), 200);

            Ok(())
        }

        // requires netcat to run on localhost 5678
        // tested by looking at the generated requests by netcat
        // Results can be found in the test_files/http/request/jpg_file_singular_request.txt
        // For now is only used to generate bodies and manual inspection
        #[test]
        fn test_building_singular_complex_binary_body() -> Result<()> {
            let test_url = "http://localhost:5678";
            let mut client = Client::new(test_url, Method::POST)?;
            let test_file = "./test_files/binary/16x16.jpg";
            let file = fs::File::open(test_file)?;

            client.add_parameter(Parameter::ComplexParameter {
                name: "test_file".to_owned(),
                mime_type: mime::APPLICATION_OCTET_STREAM.to_string(),
                content_handle: file,
            });
            let mut request_builder = client.reqwest_client.request(Method::POST, test_url);
            request_builder = client.generate_body(request_builder)?;
            let request = request_builder.build()?;

            let response = client.reqwest_client.execute(request)?;
            println!("{:?}", response);
            assert_eq!(response.status().as_u16(), 200);

            let body = response.bytes()?;
            let _ = fs::write("./output/output.jpg", body);
            Ok(())
        }

        // requires netcat to run on localhost 5678
        // tested by looking at the generated requests by netcat
        // Results can be found in the test_files/http/request/text_multipart_request.txt
        // For now is only used to generate bodies and manual inspection
        #[test]
        fn test_building_text_multipart() -> Result<()> {
            let test_url = "http://localhost:5678";
            let mut client = Client::new(test_url, Method::POST)?;
            client.add_parameter(Parameter::SimpleParameter {
                name: "simple_param_0test".to_owned(),
                value: "simple_value0".to_owned(),
                param_type: ParameterType::Body,
            });
            client.add_parameter(Parameter::SimpleParameter {
                name: "simple_param_1test".to_owned(),
                value: "simple_value1".to_owned(),
                param_type: ParameterType::Body,
            });
            client.add_parameter(Parameter::SimpleParameter {
                name: "simple_param_2test".to_owned(),
                value: "simple_value2".to_owned(),
                param_type: ParameterType::Body,
            });
            let mut request_builder = client.reqwest_client.request(Method::POST, test_url);
            request_builder = client.generate_body(request_builder)?;
            let request = request_builder.build()?;
            let response = client.reqwest_client.execute(request)?;
            assert_eq!(response.status().as_u16(), 200);
            println!("{:?}", response.text().unwrap());
            Ok(())
        }

        // requires netcat to run on localhost 5678
        // tested by looking at the generated requests by netcat
        // Results can be found in the test_files/http/request/mixed_multipart_request.txt
        // For now is only used to generate bodies and manual inspection
        #[test]
        fn test_building_mixed_multipart() -> Result<()> {
            let test_url = "http://localhost:5678";
            let mut client = Client::new(test_url, Method::POST)?;

            let test_file = "./test_files/binary/16x16.jpg";
            let file = fs::File::open(test_file)?;
            client.add_parameter(Parameter::ComplexParameter {
                name: "test_jpg".to_owned(),
                mime_type: mime::IMAGE_JPEG.to_string(),
                content_handle: file,
            });

            let test_file = "./test_files/text/file_example.xml";
            let file = fs::File::open(test_file)?;
            client.add_parameter(Parameter::ComplexParameter {
                name: "test_xml".to_owned(),
                mime_type: mime::TEXT_XML.to_string(),
                content_handle: file,
            });

            client.add_parameter(Parameter::SimpleParameter {
                name: "test_simple".to_owned(),
                value: "test_value".to_owned(),
                param_type: ParameterType::Body,
            });

            let mut request_builder = client.reqwest_client.request(Method::POST, test_url);
            request_builder = client.generate_body(request_builder)?;
            let request = request_builder.build()?;
            let response = client.reqwest_client.execute(request)?;
            assert_eq!(response.status().as_u16(), 200);
            println!("{:?}", response.text().unwrap());
            Ok(())
        }
    }

    mod test_parsing {
        use mime::TEXT_PLAIN_UTF_8;

        use super::*;

        #[test]
        fn test_simple_parameter_parsing() -> Result<()> {
            let headers =
                parse_headers_from_file("./test_files/http/headers/simple_singular_headers.txt")?;
            println!("Headers: {:?}", headers);
            let body = fs::read("./test_files/http/bodies/simple_singular_body.txt")?;
            let mut result = parse_part(Headers::HeaderMap(headers), &body)?;
            assert_eq!(result.len(), 1);
            let result = result.pop().unwrap();
            match result {
                Parameter::SimpleParameter {
                    name,
                    value,
                    param_type,
                } => {
                    assert_eq!(name, "simple_param_ test");
                    assert_eq!(value, "simple_value");
                    assert!(matches!(param_type, ParameterType::Body));
                }
                Parameter::ComplexParameter { .. } => panic!("Should be simple_parameter"),
            }
            Ok(())
        }

        #[test]
        fn test_complex_parameter_parsing_text_file() -> Result<()> {
            let headers = parse_headers_from_file(
                "./test_files/http/headers/text_file_singular_headers.txt",
            )?;
            println!("Headers: {:?}", headers);
            let body = fs::read("./test_files/http/bodies/text_file_singular_body.txt")?;
            let mut result = parse_part(Headers::HeaderMap(headers), &body)?;
            println!("{:?}", result);
            match result.pop().unwrap() {
                Parameter::SimpleParameter { .. } => panic!("Should not happen"),
                Parameter::ComplexParameter {
                    mut content_handle, ..
                } => {
                    println!("{:?}", content_handle);
                    let mut buffer = Vec::new();
                    content_handle.read_to_end(&mut buffer)?;
                    assert_eq!(body, buffer)
                }
            };
            Ok(())
        }

        #[test]
        fn test_complex_parameter_parsing_binary_file() -> Result<()> {
            let headers =
                parse_headers_from_file("./test_files/http/headers/jpg_file_singular_headers.txt")?;
            let body = fs::read("./test_files/http/bodies/jpg_file_singular_body.txt")?;
            let mut result = parse_part(Headers::HeaderMap(headers), &body)?;
            match result.pop().unwrap() {
                Parameter::SimpleParameter { .. } => panic!("Should not happen"),
                Parameter::ComplexParameter {
                    name,
                    mime_type,
                    mut content_handle,
                } => {
                    // Test custom name via content-id header:
                    assert_eq!(name, "moon.jpg");
                    assert_eq!(mime_type, APPLICATION_OCTET_STREAM.to_string());
                    let mut buffer = Vec::new();
                    content_handle.read_to_end(&mut buffer)?;
                    assert_eq!(body, buffer);
                    // Using custom name:
                }
            };
            Ok(())
        }

        #[test]
        fn test_text_multipart_parsing() -> Result<()> {
            let headers =
                parse_headers_from_file("./test_files/http/headers/text_multipart_headers.txt")?;
            println!("Headers: {:?}", headers);
            let body = fs::read("./test_files/http/bodies/text_multipart_body.txt")?;
            let result = parse_part(Headers::HeaderMap(headers), &body)?;
            assert_eq!(result.len(), 3);
            for (index, parameter) in result.into_iter().enumerate() {
                println!("Checking parameter {}", index);
                match parameter {
                    Parameter::SimpleParameter { .. } => panic!("Should not happen"),
                    Parameter::ComplexParameter {
                        name,
                        mime_type,
                        mut content_handle,
                    } => {
                        assert_eq!(mime_type, TEXT_PLAIN_UTF_8.to_string());
                        assert_eq!(name, format!("simple_param_{}test", index));

                        let mut content = String::new();
                        content_handle.read_to_string(&mut content)?;
                        assert_eq!(content, format!("simple_value{}", index));
                    }
                }
            }

            Ok(())
        }

        #[test]
        fn test_mixed_multipart_parsing() -> Result<()> {
            let headers =
                parse_headers_from_file("./test_files/http/headers/mixed_multipart_headers.txt")?;
            println!("Headers: {:?}", headers);
            let body = fs::read("./test_files/http/bodies/mixed_multipart_body.txt")?;
            let mut result = parse_part(Headers::HeaderMap(headers), &body)?;
            assert_eq!(result.len(), 3);
            result.iter().for_each(|parameter| {
                assert!(matches!(parameter, Parameter::ComplexParameter { .. }))
            });
            let expected_names = ["test_jpg", "test_xml", "test_simple"];
            let expected_mime_types = ["image/jpeg", "text/xml", "text/plain; charset=utf-8"];

            let text_value: Vec<u8> = "test_value".bytes().collect();
            let xml_content: Vec<u8> =
                fs::read("./test_files/text/file_example.xml").expect("Failed reading xml");
            let image: Vec<u8> =
                fs::read("./test_files/binary/16x16.jpg").expect("Failed reading jpg");
            let expected_content = [image, xml_content, text_value];
 
            result
                .iter_mut()
                .enumerate()
                .for_each(|(index, parameter)| match parameter {
                    Parameter::SimpleParameter { .. } => panic!("Cannot happen."),
                    Parameter::ComplexParameter {
                        name,
                        mime_type,
                        content_handle,
                    } => {
                        assert_eq!(name, expected_names[index]);
                        assert_eq!(mime_type, expected_mime_types[index]);
                        
                        let mut content = Vec::new();
                        content_handle.read_to_end(&mut content).expect("Error reading parameter content");
                        assert_eq!(content, expected_content[index]);
                    }
                });

            println!("{:?}", result);
            Ok(())
        }

        fn parse_headers_from_file(path: &str) -> Result<HeaderMap> {
            let header_string = fs::read_to_string(path)?.replace("\r", "");

            let mut headers = HeaderMap::new();
            header_string
                .split("\n")
                .map(|header| -> Result<(HeaderName, HeaderValue)> {
                    let (name, value) = header
                        .split_once(":")
                        .ok_or(Error::HeaderParseError("Does not contain :".to_owned()))?;
                    Ok((
                        HeaderName::from_str(name.trim())?,
                        HeaderValue::from_str(value.trim())?,
                    ))
                })
                .filter_map(|result| match result {
                    Ok((header_name, header_value)) => Some((header_name, header_value)),
                    Err(_) => None,
                })
                .for_each(|entry| {
                    headers.insert(entry.0, entry.1);
                });
            Ok(headers)
        }
    }

    /**
     * Copies bytes from the 16x16.jpg from the multipart directly into a new file to check for correctness
     */
    fn _copy_result() -> Result<()> {
        let test_file = "./scripts/output-multipart-mixed.txt";
        let mut file = fs::File::open(test_file)?;
        file.seek(std::io::SeekFrom::Start(0x190))?;
        let mut buffer: &mut [u8] = &mut [0; 0x1C19];
        file.read(&mut buffer)?;
        fs::write("./scripts/output.jpg", buffer)?;
        Ok(())
    }
}
