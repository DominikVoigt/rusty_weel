
pub enum ParameterType {
    Header,
    Body
}

pub enum HTTPParameters {
    SimpleParameter {name: String, value: String, param_type: ParameterType,},
    ComplexParamter {a: String, mime_type: String, file_path: String }
}