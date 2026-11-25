pub mod cross_file;
pub mod express;
pub mod extractor;
pub mod module_resolver;
pub mod types;
pub mod url;
pub mod vue;
pub mod zod;

pub use cross_file::{
    CrossFileAnalysis, CrossFileFieldRead, analyze_cross_file_dataflow,
    analyze_cross_file_dataflow_with_limits,
};
pub use express::extract_express_routes;
pub use extractor::{
    ExtractContext, ExtractedFieldRead, ExtractedHttpCall, ExtractedUnresolvedCall, FileExtraction,
    HttpClientKind, extract_file, extract_file_with_context,
};
pub use module_resolver::ModuleResolver;
pub use types::{TypeContractDef, TypeFieldDef, extract_type_definitions, resolve_type_fields};
pub use url::{
    ServiceEndpointMatch, is_absolute_http_url, normalize_relative_path,
    resolve_url_against_services,
};
pub use vue::{
    VueScriptBlock, VueScriptLang, VueTemplateExpr, extract_vue_scripts,
    extract_vue_template_expressions,
};
pub use zod::{extract_zod_parse_schema_name, extract_zod_schemas};
