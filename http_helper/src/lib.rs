use std::{collections::HashMap, fs::{self, File}, io::{Read, Write}};

use curl::easy::{Easy, List};
use multipart::server::Multipart;
use tempfile::tempfile;
use derive_more::From;

pub enum ParameterType {
    Query,
    Body
}

pub enum Parameters {
    // Simple kv parameters, can be in body or header
    SimpleParameter {name: String, value: String, param_type: ParameterType,},
    // Body parameters, multi-part
    ComplexParamter {name: String, mime_type: String, content_handle: fs::File }
}


#[derive(Debug, Clone)]
pub enum HttpVerb {
    GET,
    PUT,
    POST,
    DELETE,
    UPDATE,
    PATCH
}

pub struct RawResponse {
    pub headers: HashMap<String, String>,
    pub body: File,
    pub status_code: u32,
    content_type: String,
}

/**
 * Abstraction on top of libcurl
 */
pub struct Client {
    method: HttpVerb,
    client: Easy,
}

#[derive(From)]
pub enum Error {
    CurlError(curl::Error),
    FileError(std::io::Error)
}

impl HeaderValue {

    /**
     * Parses the text after the 
     */
    fn parse(text: &str) {

    }
}

impl Client {
    
    pub fn new(url: &str) -> Result<Client, curl::Error> {
        let mut client = Easy::new();
        client.url(url)?;
        client.get(true)?;
        Ok(Client {
            method: HttpVerb::GET,
            client
        })
    }

    pub fn set_method(&mut self, method: HttpVerb) {
        self.method = method;
    }
    
    pub fn set_request_body(&mut self, request_body: Vec<u8>) -> Result<(), curl::Error> {
        self.client.post_fields_copy(&request_body)
    }
    
    pub fn set_request_headers(&mut self, request_headers: List) -> Result<(), curl::Error>{
        self.client.http_headers(request_headers)
    }

    fn execute_raw(&mut self) -> Result<RawResponse, Error> {
        let mut response_headers = HashMap::new();
        let mut response_file = tempfile()?;
        {
            let mut client = self.client.transfer();
            client.write_function(|data| {
                response_file.write(data);
                Ok(data.len())
            })?;

            // Expect Encoding of data to be UTF-8
            client.header_function(|data| {
                let header = match std::str::from_utf8(data) {
                    Ok(x) => x,
                    Err(_) => return false,
                };
                match header.split_once(":") {
                    Some((name, value)) => response_headers.insert(name.to_owned(), value.to_owned()),
                    // If header is not of structure name: value we treat it like it only has a name and no value
                    None => response_headers.insert(header.to_owned(), "".to_owned()),
                };
                true
            })?;
            client.perform()?;
        }
        let content_type = match self.client.content_type()? {
            Some(content_type) => content_type.to_owned(),
            None => {
                log::error!("No content type provided by response. Using default");
                "application/octet-stream".to_owned()
            },
        };
        Ok(RawResponse {
            body: response_file,
            headers: response_headers,
            content_type: content_type,
            status_code: self.client.response_code()?
        })
    }

    pub fn execute(&mut self) -> Result<(), Error>{
        let raw = self.execute_raw()?;

        parse_response(raw);
        Ok(())
    }
}

fn parse_response(raw: RawResponse) -> () {
    let mut result: Vec<Parameters> = Vec::new();

    parse_response_part(&raw.body, &raw.content_type, raw.headers, &mut result);
}

fn parse_response_part(raw: &File, content_type: &str, headers: HashMap<String, String>, result: &mut Vec<Parameters>) -> Result<(), Error> {
    if content_type == "form/multipart" {
        let boundary = headers.get("boundary").expect("No boundary header in multipart");
        let mut multipart = Multipart::with_body(raw , boundary);
        loop {
            match multipart.read_entry()? {
                Some(entry) => {
                    let data = entry.data;
                    let headers = entry.headers;
                    let content_type = headers.content_type;
                    let name = headers.name;
                    let file_name = headers.filename;
                },
                None => break,
            }
        }
        
        Ok(())
    } else if content_type == "application/x-www-form-urlencoded" {
        todo!()
    } else {
        todo!()
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn it_works() {
        let a = testing();
        match a {
            Ok(_) => todo!(),
            Err(err) => match err {
                Error::CurlError(_) => println!("Curl error"),
                Error::FileError(_) => println!("File error"),
            },
        };
    }

    fn testing() -> Result<(), Error>{
        Err(curl::Error::new(4))?
    }

    // TODO: Test handling with unquoted and quoted boundaries
}