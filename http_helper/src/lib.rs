use bytes::Bytes;
use derive_more::From;
use mime::{FromStrError, Mime, MULTIPART_FORM_DATA};
use multipart::server::Multipart;
use reqwest::{
    blocking::{multipart::{Form, Part}, Body, Request, RequestBuilder},
    header::{HeaderMap, HeaderName, HeaderValue, ToStrError, CONTENT_TYPE},
    Method, Url,
};
use std::{
    fs::{self, File},
    str::FromStr,
};
use tempfile::tempfile;
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
        value: Option<String>,
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
    UPDATE,
    PATCH,
}

pub struct RawResponse {
    pub headers: HeaderMap,
    pub body: bytes::Bytes,
    pub status_code: u16,
    content_type: Option<Mime>,
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
}

type Result<T> = std::result::Result<T, Error>;

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
                value: value.map(|value| encode(&value).to_string()),
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

    pub fn add_parameters(&mut self, mut parameters: Vec<Parameter>) {
        parameters
            .into_iter()
            .for_each(|parameter| self.add_parameter(parameter));
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
     * If the method is a GET, all simple parameters are turned into query parameters
     */
    fn mark_query_parameters(&mut self) {
        for parameter in &mut self.parameters {
            match parameter {
                Parameter::SimpleParameter { param_type, .. } => {
                    if self.method == Method::GET {
                        *param_type = ParameterType::Query;
                    }
                }
                _ => continue,
            }
        }
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
                        match value {
                            Some(value) => {
                                query_params.push(format!("{}={}", name, value));
                            }
                            None => {
                                query_params.push(name.clone());
                            }
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
            Ok(construct_singular_body(body_parameters.pop().expect("Cannot fail"), request_builder))
        } else {
            Ok(construct_multipart(body_parameters, request_builder)?)
        }
    }

    /**
     * Will execute the request and return the RawResponse
     * Requires the target to send headers that only contain visible ascii
     */
    fn execute_raw(mut self) -> Result<RawResponse> {
        self.mark_query_parameters();
        let url = self.generate_url();
        let mut request_builder = self.reqwest_client.request(self.method.clone(), url);
        request_builder = self.set_headers(request_builder);
        request_builder = self.generate_body(request_builder)?;
        let request = request_builder.build()?;
        let response = self.reqwest_client.execute(request)?;
        let content_type: Option<Mime> = match response.headers().get(CONTENT_TYPE) {
            Some(header) => Some(header.to_str()?.parse::<mime::Mime>()?),
            None => None,
        };

        RawResponse {
            headers: response.headers().clone(),
            status_code: response.status().as_u16(),
            content_type,
            body: response.bytes()?,
        };
        todo!()
    }

    /**
     * Executes the request and consumes the client as the headers and parameters are consumed by the request
     */
    pub fn execute(self) -> Result<()> {
        let raw = self.execute_raw()?;

        raw.parse_response();
        Ok(())
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
                    value: Some(value.to_owned()),
                    param_type: ParameterType::Query,
                },
                // query parameter without a value
                None => Parameter::SimpleParameter {
                    name: parameter.to_owned(),
                    value: None,
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
            let text = match value {
                Some(value) => format!("{}={}", name, value),
                None => name,
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
            name,
            mime_type,
            content_handle,
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
            Parameter::SimpleParameter { name, value, param_type } => {
                // A simple parameter without a value will result in a part without a part body
                let part = Part::text(value.unwrap_or("".to_owned())).mime_str(&mime::TEXT_PLAIN_UTF_8.to_string())?;
                form = form.part(name, part);
            },
            Parameter::ComplexParameter { name, mime_type, content_handle } => {
                let part = Part::reader(content_handle).mime_str(&mime_type)?;
                form = form.part(name, part);
            },
        }
    }
    Ok(request_builder.multipart(form))
}

impl RawResponse {
    fn parse_response(self) {
        let received_parameters =
            Self::parse_response_part(self.headers, self.body, self.content_type);
    }

    fn parse_response_part(
        headers: HeaderMap,
        body: Bytes,
        content_type: Option<Mime>,
    ) -> Vec<Parameter> {
        match content_type {
            Some(content_type) => {
                // Our supported text formats: text/* and application/json
                let is_text_data = content_type.essence_str().starts_with("text")
                    || content_type.essence_str() == mime::APPLICATION_JSON;

                // We use essence_str to remove any attached parameters for this comparison
                if content_type.essence_str() == mime::MULTIPART_FORM_DATA {
                } else if content_type.essence_str() == mime::APPLICATION_WWW_FORM_URLENCODED {
                } else if is_text_data {
                } else { // We treat it as binary
                }
                todo!()
            }
            None => {
                todo!()
            }
        }
    }
}

#[cfg(test)]
mod test {
    use std::io::Read;

    use mime::BOUNDARY;

    use super::*;
    mod testing_libs {
        use super::*;
        /**
         * Checks for parsed equality, but note: Only equal if all is equal including parameters
         */
        #[test]
        fn check_parsed_equalities() -> Result<()> {
            let mime_parsed = "multipart/form-data".parse::<mime::Mime>()?;
            let mime = mime::MULTIPART_FORM_DATA;
            assert_eq!(mime_parsed, mime);

            let mime_parsed = "application/json".parse::<mime::Mime>()?;
            let mime = mime::APPLICATION_JSON;
            assert_eq!(mime_parsed, mime);

            let mime_parsed = "ApplICation/JSoN".parse::<mime::Mime>()?;
            let mime = mime::APPLICATION_JSON;
            assert_eq!(mime_parsed, mime);

            let mime_parsed = "text/csv".parse::<mime::Mime>()?;
            let mime = mime::TEXT_CSV;
            assert_eq!(mime_parsed, mime);

            let mime_parsed = "text/csv; charset=utf-8".parse::<mime::Mime>()?;
            let mime = mime::TEXT_CSV_UTF_8;
            assert_eq!(mime_parsed, mime);

            let mime_parsed = "text/csv; charset=utf-8".parse::<mime::Mime>()?;
            let mime = mime::TEXT_CSV;
            assert_ne!(mime_parsed, mime);

            let mime_parsed = "text/plain; charset=utf-8".parse::<mime::Mime>()?;
            let mime = mime::TEXT_PLAIN;
            assert_eq!(mime_parsed.essence_str(), mime);
            Ok(())
        }

        /**
         * Tests parsing boundaries with and without the quotation marks
         */
        #[test]
        fn test_boundary_parsing() -> Result<()> {
            // Basic case without quotation marks
            let mime_parsed =
                "multipart/form-data; boundary=askdfjaskfnasd".parse::<mime::Mime>()?;

            assert_eq!(
                "askdfjaskfnasd",
                mime_parsed.get_param(BOUNDARY).expect("Oups")
            );
            assert_ne!(
                "\"askdfjaskfnasd\"",
                mime_parsed.get_param(BOUNDARY).expect("Oups")
            );

            let mime = mime::MULTIPART_FORM_DATA;
            // essence_str does not include parameters:
            assert_eq!(mime_parsed.essence_str(), mime.essence_str());

            let mime_parsed =
                "multipart/form-data; boundary=\"askdfjaskfnasd\"".parse::<mime::Mime>()?;
            let mime = mime::MULTIPART_FORM_DATA;

            assert_eq!(
                "askdfjaskfnasd",
                mime_parsed.get_param(BOUNDARY).expect("Oups")
            );
            assert_ne!(
                "\"askdfjaskfnasd\"",
                mime_parsed.get_param(BOUNDARY).expect("Oups")
            );

            assert_ne!(mime, mime_parsed);
            // essence_str does not include parameters:
            assert_eq!(mime_parsed.essence_str(), mime.essence_str());
            Ok(())
        }

        #[test]
        fn test_essences() -> Result<()> {
            let mime = mime::APPLICATION_JSON;
            println!("{}", mime.essence_str());

            let mime = mime::TEXT_CSV_UTF_8;
            println!("{}", mime.essence_str());

            Ok(())
        }

        #[test]
        fn mime_string() -> Result<()> {
            let mime = mime::TEXT_PLAIN_UTF_8;
            println!("{}", mime.essence_str());
            println!("{}", mime.to_string());

            let mime = mime::APPLICATION_WWW_FORM_URLENCODED;
            println!("{}", mime.to_string());

            Ok(())
        }

        #[test]
        fn test_reqwest() -> Result<()> {
            let test_url = "http://localhost:5678?a==$=&=+=,=/=;=?=@ b&c=d";
            let mut client = Client::new(test_url, Method::POST)?;
            client.add_parameter(Parameter::SimpleParameter {
                name: "simple_param_test".to_owned(),
                value: Some("simple_value".to_owned()),
                param_type: ParameterType::Body,
            });
            let mut request_builder = client.reqwest_client.request(Method::POST, test_url);
            request_builder = client.generate_body(request_builder)?;
            let request = request_builder.build()?;
            let response = client.reqwest_client.execute(request);
            println!("{:?}", response);
            Ok(())
        }
    }

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
                value: Some("a1".to_owned()),
                param_type: ParameterType::Body,
            },
            Parameter::SimpleParameter {
                name: "b".to_owned(),
                value: Some("b1".to_owned()),
                param_type: ParameterType::Query,
            },
            Parameter::SimpleParameter {
                name: "c".to_owned(),
                value: None,
                param_type: ParameterType::Query,
            },
        ];
        client.add_parameters(parameters);
        client.mark_query_parameters();
        assert_eq!(
            "https://www.testing.com/?test=value&onlyname&a=a1&b=b1&c",
            client.generate_url().to_string()
        );
        Ok(())
    }

    // requires http_echo_server (npm) to run on localhost 5678
    #[test]
    fn test_building_singular_simple_body() -> Result<()> {
        let test_url = "http://localhost:5678?a=2&b=3&c= d";
        let mut client = Client::new(test_url, Method::POST)?;
        client.add_parameter(Parameter::SimpleParameter {
            name: "simple_param_ test".to_owned(),
            value: Some("simple_value".to_owned()),
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

    // requires http_echo_server (npm) to run on localhost 5678
    #[test]
    fn test_building_singular_complex_text_body() -> Result<()> {
        let test_url = "http://localhost:5678?a=2&b=3&c= d";
        let mut client = Client::new(test_url, Method::POST)?;
        let test_file_dir = "./test_files/text";
        for file in fs::read_dir(test_file_dir)? {
            let file = file?.path();
            let file = File::open(file)?;

            client.add_parameter(Parameter::ComplexParameter {
                name: "test_file".to_owned(),
                mime_type: mime::APPLICATION_JSON.to_string(),
                content_handle: file,
            });
            let mut request_builder = client.reqwest_client.request(Method::POST, test_url);
            request_builder = client.generate_body(request_builder)?;
            let request = request_builder.build()?;
            let response = client.reqwest_client.execute(request)?;
            assert_eq!(response.status().as_u16(), 200);
        }

        Ok(())
    }

    // requires http_echo_server (npm) to run on localhost 5678
    #[test]
    fn test_building_singular_complex_binary_body() -> Result<()> {
        let test_url = "http://localhost:5678?a=2&b=3&c= d";
        let mut client = Client::new(test_url, Method::POST)?;
        let test_file = "./test_files/binary/16x16.jpg";
        let file = File::open(test_file)?;

        client.add_parameter(Parameter::ComplexParameter {
            name: "test_file".to_owned(),
            mime_type: mime::APPLICATION_OCTET_STREAM.to_string(),
            content_handle: file,
        });
        let mut request_builder = client.reqwest_client.request(Method::POST, test_url);
        request_builder = client.generate_body(request_builder)?;
        let mut request = request_builder.build()?;

        let response = client.reqwest_client.execute(request)?;
        println!("{:?}", response);
        assert_eq!(response.status().as_u16(), 200);

        let body = response.bytes()?;
        fs::write("./output/output.jpg", body);
        Ok(())
    }

    // requires http_echo_server (npm) to run on localhost 5678
    #[test]
    fn test_building_multipart_simple_body() -> Result<()> {
        let test_url = "http://localhost:5678?a=2&b=3&c= d";
        let mut client = Client::new(test_url, Method::POST)?;
        client.add_parameter(Parameter::SimpleParameter {
            name: "simple_param_0test".to_owned(),
            value: Some("simple_value0".to_owned()),
            param_type: ParameterType::Body,
        });
        client.add_parameter(Parameter::SimpleParameter {
            name: "simple_param_1test".to_owned(),
            value: Some("simple_value1".to_owned()),
            param_type: ParameterType::Body,
        });
        client.add_parameter(Parameter::SimpleParameter {
            name: "simple_param_2test".to_owned(),
            value: Some("simple_value2".to_owned()),
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


    // requires http_echo_server (npm) to run on localhost 5678
    #[test]
    fn test_building_multipart_complex_body() -> Result<()> {
        let test_url = "http://localhost:5678?a=2&b=3&c= d";
        let mut client = Client::new(test_url, Method::POST)?;
        
        let test_file = "./test_files/binary/16x16.jpg";
        let file = File::open(test_file)?;
        client.add_parameter(Parameter::ComplexParameter {
            name: "test_file".to_owned(),
            mime_type: mime::APPLICATION_OCTET_STREAM.to_string(),
            content_handle: file,
        });

        let test_file = "./test_files/text/file_example.json";
        let file = File::open(test_file)?;
        client.add_parameter(Parameter::ComplexParameter {
            name: "test_file".to_owned(),
            mime_type: mime::APPLICATION_JSON.to_string(),
            content_handle: file,
        });

        let test_file = "./test_files/text/file_example.xml";
        let file = File::open(test_file)?;
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
        println!("{:?}", response.text().unwrap());
        Ok(())
    }

    // TODO: Test handling with unquoted and quoted boundaries
}
