#[derive(Debug)]
pub struct Parameters {
    pub label: &'static str,
    pub method: HTTP,
    pub arguments: Option<Vec<KeyValuePair>>,
    pub annotations: &'static str,
}

#[derive(Debug)]
pub enum HTTP {
    GET,
    PUT,
    POST,
    DELETE,
    PATCH,
}

/*
* Represents KVs with optional values
*/
#[derive(Debug)]
pub struct KeyValuePair {
    pub key: &'static str,
    pub value: Option<&'static str>,
}

type UndefinedTypeTODO = ();
// Define a float type to easily apply changes here if needed
#[allow(non_camel_case_types)]
type float = f32;
