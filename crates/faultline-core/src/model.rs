use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum HttpMethod {
    Get,
    Post,
    Put,
    Patch,
    Delete,
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

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AffectedConsumer {
    pub repo: String,
    pub evidence: Evidence,
    pub confidence: Confidence,
    pub dependency_path: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImpactFinding {
    pub id: String,
    pub change: ContractChange,
    pub severity: Severity,
    pub confidence: Confidence,
    pub producer_evidence: Evidence,
    pub consumers: Vec<AffectedConsumer>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnresolvedCall {
    pub repo: String,
    pub evidence: Evidence,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnalysisReport {
    pub schema_version: u32,
    pub target_repo: String,
    pub base_rev: String,
    pub head_rev: String,
    pub findings: Vec<ImpactFinding>,
    pub unresolved_calls: Vec<UnresolvedCall>,
}
