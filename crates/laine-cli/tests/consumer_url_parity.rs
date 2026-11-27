use std::collections::BTreeMap;

use laine_core::config::{CustomClientConfig, ServiceConfig};
use laine_core::model::HttpMethod;
use laine_py::{PyExtractContext, extract_python_file};
use laine_ts::{ExtractContext, extract_file_with_context};

// Check that TS and Python choose the same targets for env bases and custom wrappers.
#[test]
fn consumers_resolve_urls_and_report_ambiguity_identically() {
    let mut services = [
        ("zeta", "https://example.test/v2/"),
        ("root", "https://example.test"),
        ("mounted", "https://user:example@example.test/v1/?base=1"),
        ("alpha", "https://example.test/v2"),
    ]
    .map(|(name, base)| ServiceConfig {
        name: name.into(),
        path: name.into(),
        openapi: None,
        routes: None,
        base_urls: vec![base.into()],
    });
    let clients = [CustomClientConfig {
        consumer_service: None,
        callee: "apiClient".into(),
        target_service: "root".into(),
        path_prefix: Some("/proxy//".into()),
    }];
    let env = BTreeMap::from([(
        "API_BASE".into(),
        " https://user:example@example.test/v1//?base=1#frag ".into(),
    )]);
    let ts = r#"
const client = axios.create({ baseURL: process.env.API_BASE });
client.get(' /orders//items/?limit=1#frag ');
fetch('https://user:example@example.test/v1/hello@world?limit=1#frag');
apiClient.get(' orders//items?limit=1#frag ');
apiClient.get('https://example.test/v1/override');
client.get('https://example.test/v2/tied');
"#;
    let py = r#"
import httpx
import os
client = httpx.Client(base_url=os.getenv("API_BASE"))
client.get(" /orders//items/?limit=1#frag ")
httpx.get("https://user:example@example.test/v1/hello@world?limit=1#frag")
apiClient.get(" orders//items?limit=1#frag ")
apiClient.get("https://example.test/v1/override")
client.get("https://example.test/v2/tied")
"#;
    let expected = [
        ("mounted", HttpMethod::Get, "/orders/items"),
        ("mounted", HttpMethod::Get, "/hello@world"),
        ("root", HttpMethod::Get, "/proxy/orders/items"),
        ("mounted", HttpMethod::Get, "/override"),
    ];
    let expected_reason = ["ambiguous service base URL (candidates: alpha, zeta)"];

    for _ in 0..2 {
        let ts = extract_file_with_context(
            "consumer.ts",
            ts,
            ExtractContext {
                services: &services,
                clients: &clients,
                env: &env,
            },
        )
        .unwrap();
        let py = extract_python_file(
            "consumer.py",
            py,
            PyExtractContext {
                services: &services,
                clients: &clients,
                env: &env,
            },
        );
        let ts_calls: Vec<_> = ts
            .calls
            .iter()
            .map(|call| {
                (
                    call.service.as_str(),
                    call.method,
                    call.normalized_path.as_str(),
                )
            })
            .collect();
        let py_calls: Vec<_> = py
            .calls
            .iter()
            .map(|call| {
                (
                    call.service.as_str(),
                    call.method,
                    call.normalized_path.as_str(),
                )
            })
            .collect();
        assert_eq!(ts_calls, expected);
        assert_eq!(py_calls, expected);
        let ts_reasons: Vec<_> = ts
            .unresolved_calls
            .iter()
            .map(|call| &call.reason)
            .collect();
        let py_reasons: Vec<_> = py
            .unresolved_calls
            .iter()
            .map(|call| &call.reason)
            .collect();
        assert_eq!(ts_reasons, expected_reason);
        assert_eq!(py_reasons, expected_reason);
        services.reverse();
    }
}
