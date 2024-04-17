enum HTTPVerb {
    GET,
    PUT,
    POST,
    DELETE,
    PATCH,
}
struct Argument {
    key: String,
    value: Option<String>,
}
type UndefinedTypeTODO = ();

struct Timing {
    timing_weight: Option<UndefinedTypeTODO>,
    timingAvg: Option<UndefinedTypeTODO>,
    explanations: Option<UndefinedTypeTODO>,
}
struct Shift {
    shiftingType: String,
}

struct ContextDataAnalysis;
struct Report;
struct Notes;

struct Annotations {
    generic: Option<String>,
    timing: Timing,
    shifting: Shift,
    context_data_analysis: ContextDataAnalysis,
    report: Report,
    notes: Notes,
}

struct Parameters {
    label: String,
    method: HTTPVerb,
    arguments: Option<Vec<Argument>>,
    annotations: Annotations,
}

trait DSL {
    fn call(label: String, endpoint: String, parameters: Parameters);
}
