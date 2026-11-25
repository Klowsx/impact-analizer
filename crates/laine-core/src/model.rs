use serde::{Deserialize, Serialize};

use crate::policy::PolicyEvaluation;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum HttpMethod {
    Get,
    Post,
    Put,
    Patch,
    Delete,
}

impl HttpMethod {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.to_ascii_lowercase().as_str() {
            "get" => Some(Self::Get),
            "post" => Some(Self::Post),
            "put" => Some(Self::Put),
            "patch" => Some(Self::Patch),
            "delete" => Some(Self::Delete),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Patch => "PATCH",
            Self::Delete => "DELETE",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Confidence {
    Unknown,
    Possible,
    Likely,
    Confirmed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Severity {
    Low,
    Medium,
    High,
    Critical,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct EndpointId {
    pub service: String,
    pub method: HttpMethod,
    pub normalized_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Evidence {
    pub repo: String,
    pub revision: String,
    pub file: String,
    pub line: usize,
    pub snippet: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceSpan {
    pub line: usize,
    pub column: usize,
    pub end_line: usize,
    pub end_column: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ProducerEvidence {
    pub repo: String,
    pub revision: String,
    pub file: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span: Option<SourceSpan>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub json_pointer: Option<String>,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub snippet: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ContractChange {
    EndpointRemoved {
        endpoint: EndpointId,
    },
    ResponseFieldRemoved {
        endpoint: EndpointId,
        status_code: String,
        field_path: String,
        previous_type: String,
    },
    ResponseFieldTypeChanged {
        endpoint: EndpointId,
        status_code: String,
        field_path: String,
        previous_type: String,
        new_type: String,
    },
    RequestFieldAddedRequired {
        endpoint: EndpointId,
        field_path: String,
        field_type: String,
    },
    RequestFieldTypeChanged {
        endpoint: EndpointId,
        field_path: String,
        previous_type: String,
        new_type: String,
    },
    RequiredParameterAdded {
        endpoint: EndpointId,
        location: String,
        name: String,
        param_type: String,
    },
}

impl ContractChange {
    // Devuelve la operación a la que pertenece el cambio.
    pub fn endpoint(&self) -> &EndpointId {
        match self {
            Self::EndpointRemoved { endpoint }
            | Self::ResponseFieldRemoved { endpoint, .. }
            | Self::ResponseFieldTypeChanged { endpoint, .. }
            | Self::RequestFieldAddedRequired { endpoint, .. }
            | Self::RequestFieldTypeChanged { endpoint, .. }
            | Self::RequiredParameterAdded { endpoint, .. } => endpoint,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AffectedConsumer {
    pub repo: String,
    pub evidence: Evidence,
    pub confidence: Confidence,
    pub dependency_path: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndirectConsumer {
    pub repo: String,
    pub confidence: Confidence,
    pub via_repo: String,
    pub dependency_chain: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImpactFinding {
    pub id: String,
    pub change: ContractChange,
    pub severity: Severity,
    pub confidence: Confidence,
    pub producer_evidence: ProducerEvidence,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub producer_evidence_comparison: Option<ProducerEvidence>,
    pub consumers: Vec<AffectedConsumer>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub indirect_consumers: Vec<IndirectConsumer>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnresolvedCall {
    pub repo: String,
    pub evidence: Evidence,
    pub reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiscrepancyKind {
    UndocumentedInOpenApi,
    MissingInCode,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContractDiscrepancy {
    pub endpoint: EndpointId,
    pub kind: DiscrepancyKind,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AnalysisStats {
    pub repositories: usize,
    pub files_analyzed: usize,
    pub graph_nodes: usize,
    pub graph_edges: usize,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileCoverage {
    pub discovered: usize,
    pub read: usize,
    pub analyzed: usize,
    pub failed: usize,
}

impl FileCoverage {
    // Agrega los conteos de otro repositorio o fuente del mismo snapshot.
    pub fn accumulate(&mut self, other: Self) {
        self.discovered += other.discovered;
        self.read += other.read;
        self.analyzed += other.analyzed;
        self.failed += other.failed;
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnalysisCoverage {
    pub complete: bool,
    pub producer_base: FileCoverage,
    pub producer_head: FileCoverage,
    pub consumers: FileCoverage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtractionFailure {
    FileReadFailed,
    FileParseFailed,
}

impl ExtractionFailure {
    // Resume el fallo sin copiar texto del archivo o del parser.
    pub fn message(self) -> &'static str {
        match self {
            Self::FileReadFailed => "File could not be read",
            Self::FileParseFailed => "File could not be parsed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtractionStage {
    ProducerBase,
    ProducerHead,
    Consumer,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ExtractionDiagnostic {
    pub repo: String,
    pub revision: String,
    pub file: String,
    pub stage: ExtractionStage,
    pub code: ExtractionFailure,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnalysisReport {
    pub schema_version: u32,
    pub target_repo: String,
    pub base_rev: String,
    pub head_rev: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<PolicyEvaluation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coverage: Option<AnalysisCoverage>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<ExtractionDiagnostic>,
    pub stats: AnalysisStats,
    pub findings: Vec<ImpactFinding>,
    pub unresolved_calls: Vec<UnresolvedCall>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub discrepancies: Vec<ContractDiscrepancy>,
}
