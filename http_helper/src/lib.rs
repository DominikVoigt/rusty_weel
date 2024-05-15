use std::fs;


pub enum ParameterType {
    Header,
    Body
}

pub enum HTTPParameters {
    // Simple kv parameters, can be in body or header
    SimpleParameter {name: String, value: String, param_type: ParameterType,},
    // Body parameters, multi-part
    ComplexParamter {name: String, mime_type: String, content_handle: fs::File }
}