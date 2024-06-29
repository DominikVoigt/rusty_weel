use bytes::Bytes;
use derive_more::From;
use form_urlencoded::Serializer;
use mime::{FromStrError, Mime, MULTIPART_FORM_DATA};
use multipart::server::Multipart;
use reqwest::{
    blocking::{Body, Request},
    header::{HeaderMap, HeaderName, HeaderValue, ToStrError, CONTENT_TYPE},
    Method, Url,
};
use std::{
    fs::{self, File},
    str::FromStr,
};
use tempfile::tempfile;

#[derive(Clone)]
pub enum ParameterType {
    Query,
    Body,
}

// Simple kv parameters, can be in body or url
// If a request is a get request -> All parameters into the query
// The name and value do not have to be escaped yet -> Part of generate_url

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
    client: reqwest::blocking::Client,
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
        Ok(Client {
            method,
            client,
            base_url,
            headers: HeaderMap::new(),
            parameters,
        })
    }

    pub fn add_parameter(&mut self, param: Parameter) {
        self.parameters.push(param);
    }

    pub fn add_parameters(&mut self, mut parameters: Vec<Parameter>) {
        self.parameters.append(&mut parameters);
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
     * Given the parameters/method the URL parameters are adapted
     * If the method is a GET, all simple parameters are directly turned into query parameters
     */
    fn mark_query_parameters(&mut self) {
        for parameter in &mut self.parameters {
            match parameter {
                Parameter::SimpleParameter {
                    name,
                    value,
                    param_type,
                } => {
                    if self.method == Method::GET {
                        *param_type = ParameterType::Query;
                    }
                }
                _ => continue,
            }
        }
    }

    fn generate_url(&self) -> Url {
        let mut builder: Serializer<String> = form_urlencoded::Serializer::new("".to_owned());
        self.parameters
            .iter()
            .filter(|parameter| match parameter {
                Parameter::SimpleParameter {
                    name,
                    value,
                    param_type,
                } => matches!(param_type, ParameterType::Query),
                _ => false,
            })
            .for_each(|parameter| {
                if let Parameter::SimpleParameter {
                    name,
                    value,
                    param_type,
                } = parameter
                {
                    match value {
                        Some(value) => {
                            builder.append_pair(name, value);
                        }
                        None => {
                            builder.append_key_only(name);
                        }
                    };
                }
            });
        let url = format!("{}?{}", self.base_url.as_str(), builder.finish());
        Url::parse(&url).expect("Cannot happen")
    }

    /**
     * Given the parameters/method the body or URL parameters are adapted
     */
    fn generate_body(&mut self) -> Vec<u8> {
        let mut parameters: Vec<Parameter> = Vec::new();
        std::mem::swap(&mut self.parameters, &mut parameters);
        let mut body_parameters: Vec<Parameter> = parameters
            .into_iter()
            .filter(|parameter| match parameter {
                Parameter::SimpleParameter { param_type, .. } => {
                    matches!(param_type, ParameterType::Body)
                }
                Parameter::ComplexParameter { .. } => true,
            })
            .collect();
        let body: Vec<u8> = if body_parameters.len() == 1 {
            construct_singular_body(body_parameters.pop().expect("Cannot fail"))
        } else {
            construct_multipart(body_parameters)
        };
        
        body
    }

    /**
     * Will execute the request and return the RawResponse
     * Requires the target to send headers that only contain visible ascii
     */
    fn execute_raw(mut self) -> Result<RawResponse> {
        self.mark_query_parameters();
        let url = self.generate_url();
        self.generate_body();
        let request = Request::new(self.method, url);

        let response = self.client.execute(request)?;
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
     * Executes the request and consumes the client as the request may contain uncloneable body
     */
    pub fn execute(self) -> Result<()> {
        let raw = self.execute_raw()?;

        raw.parse_response();
        Ok(())
    }
}

fn generate_base_url(url_str: &str) -> Result<(Url, Vec<Parameter>)> {
    let url_str = url_str.trim_end();
    let split = url_str.split_once("?");
    match split {
        Some((base_url_str, query_str)) => {
            let base_url_str = cleanup_trailing_slashes(base_url_str);
            let url = Url::parse(base_url_str)?;
            let parameters = parse_query_parameters(query_str);
            Ok((url, parameters))
        }
        // URL does not contain any query parameters
        None => Ok((Url::parse(url_str)?, Vec::new())),
    }
}

/**
 * Used to cleanup the trailing / of the base url
 *
 * Will only work for ASCII, but fine for now
 */
fn cleanup_trailing_slashes(url_str: &str) -> &str {
    url_str.trim_end_matches("/")
}

/**
 * Parses the query string (the section after <base-url>?)
 */
fn parse_query_parameters(query_str: &str) -> Vec<Parameter> {
    query_str
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
 */
fn construct_singular_body(parameter: Parameter) -> Vec<u8> {
    todo!()
}

fn construct_multipart(parameters: Vec<Parameter>) -> Vec<u8> {
    todo!()
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
    use mime::BOUNDARY;

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

    #[test]
    fn test_boundary_parsing() -> Result<()> {
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
        println!("{}", mime.essence_str());
        // essence_str does not include parameters:
        println!("{}", mime_parsed.essence_str());
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
    fn test_url_generation() -> Result<()> {
        let test_url = "https://www.testing.com/?test=value&tes., = aba\"";
        let client = Client::new(test_url, Method::GET)?;
        assert_eq!(
            "https://www.testing.com/?test=value&tes.%2C+=+aba%22",
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

    #[test]
    fn test_trimming_trailing_slashes() {
        let test_url = "https://www.testing.com///";
        assert_eq!(
            "https://www.testing.com",
            cleanup_trailing_slashes(test_url)
        );
    }

    // TODO: Test handling with unquoted and quoted boundaries
}
