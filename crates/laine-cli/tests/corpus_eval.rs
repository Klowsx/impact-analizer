//! Score the analysis engine against a deterministic, multi-framework corpus.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::time::{SystemTime, UNIX_EPOCH};

use laine_cli::engine::{
    AnalyzeRequest, ConsumerRevision, ConsumersRequest, index_workspace, query_endpoint_consumers,
    run_analysis_with_options,
};
use laine_cli::report::render_json;
use laine_core::model::{
    AnalysisReport, Confidence, ContractChange, DiscrepancyKind, ExtractionFailure,
    ExtractionStage, HttpMethod,
};
use serde::Serialize;
use serde_json::json;

use Confidence::{Confirmed, Likely, Possible};
use HttpMethod::{Get, Post};

const OPENAPI: &str = "openapi = 'openapi.yaml'\n";
const PROFILE: &str = "openapi: 3.0.3\npaths:\n  /profile:\n    get:\n      responses:\n        '200':\n          description: Profile\n          content:\n            application/json:\n              schema: {$ref: './schemas/profile.yaml'}\n";
const OLD_FIELD: &str =
    "type: object\nproperties:\n  id: {type: integer}\n  name: {type: string}\n";
const NEW_FIELD: &str = "type: object\nproperties:\n  id: {type: integer}\n";
const PROFILE_SERVICE: &str = "export async function load() { const r = await fetch('http://api/profile'); return r.json(); }\n";

struct Corpus(PathBuf);

impl Corpus {
    // Reserve a fresh directory so parallel test processes use isolated Git and SQLite state.
    fn new() -> Self {
        static NEXT_ID: AtomicUsize = AtomicUsize::new(0);
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let seq = NEXT_ID.fetch_add(1, Relaxed);
        let root =
            std::env::temp_dir().join(format!("laine-corpus-{}-{stamp}-{seq}", std::process::id()));
        fs::create_dir(&root).unwrap();
        Self(root)
    }

    // Run Git with deterministic identity, revisions, and line endings.
    fn git(&self, repo: &str, args: &[&str]) {
        let output = Command::new("git")
            .current_dir(self.0.join(repo))
            .args([
                "-c",
                "core.autocrlf=false",
                "-c",
                "core.hooksPath=.git/no-hooks",
            ])
            .args(args)
            .env("GIT_AUTHOR_NAME", "Laine Corpus")
            .env("GIT_AUTHOR_EMAIL", "corpus@example.invalid")
            .env("GIT_COMMITTER_NAME", "Laine Corpus")
            .env("GIT_COMMITTER_EMAIL", "corpus@example.invalid")
            .env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00Z")
            .env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00Z")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{repo}: git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // Materialize the initial commit of a fixture repository.
    fn repo(&self, repo: &str, files: &[(&str, &str)]) {
        fs::create_dir_all(self.0.join(repo)).unwrap();
        self.git(
            repo,
            &[
                "init",
                "--quiet",
                "--initial-branch=main",
                "--object-format=sha1",
            ],
        );
        self.snapshot(repo, files, "Add corpus fixture");
        self.git(repo, &["tag", "base"]);
    }

    // Write a set of files and record the next fixed-date Git snapshot.
    fn snapshot(&self, repo: &str, files: &[(&str, &str)], message: &str) {
        for (file, source) in files {
            let path = self.0.join(repo).join(file);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, source).unwrap();
        }
        self.git(repo, &["add", "--all"]);
        self.git(repo, &["commit", "--quiet", "--no-gpg-sign", "-m", message]);
    }

    // Create a workspace request with a cache private to the current corpus scenario.
    fn request(&self, producer: &str, consumers: &[&str], extra: &str) -> AnalyzeRequest {
        let config_path = self.0.join("laine.toml");
        let mut config = String::from(
            "version = 1\ncache_path = 'index.sqlite3'\n[[services]]\nname = 'api'\npath = './api'\nbase_urls = ['http://api']\n",
        );
        config.push_str(producer);
        for name in consumers {
            config.push_str(&format!("\n[[services]]\nname = '{name}'\npath = './{name}'\nbase_urls = ['http://{name}']\n"));
        }
        config.push_str(extra);
        fs::write(&config_path, config).unwrap();
        AnalyzeRequest {
            config_path,
            target_repo: "api".into(),
            base_rev: "base".into(),
            head_rev: "HEAD".into(),
        }
    }
}

impl Drop for Corpus {
    // Clean up even when an assertion fails.
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.0) {
            eprintln!("Could not remove {}: {error}", self.0.display());
        }
    }
}

type Change<'a> = (HttpMethod, &'a str, &'a str, &'a str, &'a str);
type Hit<'a> = (&'a str, &'a str, usize, Confidence);
type Indirect<'a> = (&'a str, &'a str, Confidence, &'a [&'a str]);
type Diagnostic<'a> = (
    &'a str,
    &'a str,
    ExtractionStage,
    ExtractionFailure,
    Option<usize>,
    Option<&'a str>,
);
type Discrepancy<'a> = (HttpMethod, &'a str, DiscrepancyKind);

struct Finding<'a> {
    change: Change<'static>,
    hits: &'a [Hit<'static>],
    indirect: &'static [Indirect<'static>],
}

// Describe a false attribution to rule out and the source site that should be recognized.
struct Negative {
    method: HttpMethod,
    path: &'static str,
    field: Option<&'static str>,
    repo: &'static str,
    file: &'static str,
    line: usize,
}

#[derive(Default)]
struct Expected<'a> {
    findings: &'a [Finding<'a>],
    negatives: &'a [Negative],
    unknown: &'static [(&'static str, &'static str, usize, &'static str)],
    diagnostics: &'static [Diagnostic<'static>],
    discrepancies: &'static [Discrepancy<'static>],
}

#[derive(Clone, Copy, Default, Serialize)]
struct Counts {
    expected_tp: usize,
    matched_tp: usize,
    false_positives: usize,
    false_negatives: usize,
    true_negatives_verified: usize,
    expected_possible: usize,
    matched_possible: usize,
    expected_unknown: usize,
    matched_unknown: usize,
    expected_diagnostics: usize,
    matched_diagnostics: usize,
    expected_discrepancies: usize,
    matched_discrepancies: usize,
}

impl Counts {
    // Derive precision, recall, and F1 from the observed direct attributions.
    fn metrics(self) -> serde_json::Value {
        let ratio = |num, denom| {
            if denom == 0 {
                0.0
            } else {
                num as f64 / denom as f64
            }
        };
        let precision = ratio(self.matched_tp, self.matched_tp + self.false_positives);
        let recall = ratio(self.matched_tp, self.expected_tp);
        let f1 = ratio(
            2 * self.matched_tp,
            2 * self.matched_tp + self.false_positives + self.false_negatives,
        );
        let mut metrics = serde_json::to_value(self).unwrap();
        metrics["precision"] = json!(precision);
        metrics["recall"] = json!(recall);
        metrics["f1"] = json!(f1);
        metrics
    }
}

#[derive(Default)]
struct Scores {
    total: Counts,
    by_framework: BTreeMap<&'static str, Counts>,
}

impl Scores {
    // Keep overall totals and both producer-side and consumer-side framework scores.
    fn record(&mut self, framework: &'static str, file: Option<&str>, count: impl Fn(&mut Counts)) {
        count(&mut self.total);
        count(self.by_framework.entry(framework).or_default());
        if let Some(file) = file {
            let consumer = if Path::new(file).extension().is_some_and(|ext| ext == "py") {
                "python"
            } else {
                "ts_js_vue"
            };
            count(self.by_framework.entry(consumer).or_default());
        }
    }
}

// Identify the change class, argument or field, and response status or parameter location.
fn change_key(change: &ContractChange) -> Change<'_> {
    let endpoint = change.endpoint();
    let (kind, field, context) = match change {
        ContractChange::EndpointRemoved { .. } => ("removed", "", ""),
        ContractChange::ResponseFieldRemoved {
            field_path,
            status_code,
            ..
        } => (
            "response_removed",
            field_path.as_str(),
            status_code.as_str(),
        ),
        ContractChange::ResponseFieldTypeChanged {
            field_path,
            status_code,
            ..
        } => ("response_type", field_path.as_str(), status_code.as_str()),
        ContractChange::ResponseFieldMadeOptional {
            field_path,
            status_code,
            ..
        } => (
            "response_optional",
            field_path.as_str(),
            status_code.as_str(),
        ),
        ContractChange::BodyTypeChanged { status_code, .. } => {
            ("body_type", "", status_code.as_deref().unwrap_or_default())
        }
        ContractChange::RequestFieldAddedRequired { field_path, .. } => {
            ("body_required", field_path.as_str(), "")
        }
        ContractChange::RequestFieldTypeChanged { field_path, .. } => {
            ("body_field_type", field_path.as_str(), "")
        }
        ContractChange::RequestParameterTypeChanged { location, name, .. } => {
            ("parameter_type", name.as_str(), location.as_str())
        }
        ContractChange::RequiredParameterAdded { location, name, .. } => {
            ("parameter_required", name.as_str(), location.as_str())
        }
    };
    (
        endpoint.method,
        &endpoint.normalized_path,
        kind,
        field,
        context,
    )
}

// Compare multisets of ground-truth and observed attributions, accounting for duplicates.
fn compare_hits<T: Ord>(
    expected: impl IntoIterator<Item = T>,
    actual: impl IntoIterator<Item = T>,
    mut count: impl FnMut(&T, bool, bool),
) -> (Vec<T>, Vec<(T, usize)>) {
    let mut observed = BTreeMap::<T, usize>::new();
    for item in actual {
        *observed.entry(item).or_default() += 1;
    }
    let mut missing = Vec::new();
    for item in expected {
        let matched = observed.get_mut(&item).is_some_and(|remaining| {
            if *remaining == 0 {
                return false;
            }
            *remaining -= 1;
            true
        });
        count(&item, true, matched);
        if !matched {
            missing.push(item);
        }
    }
    let mut extra = Vec::new();
    for (item, remaining) in observed {
        if remaining > 0 {
            for _ in 0..remaining {
                count(&item, false, false);
            }
            // Keep duplicate counts in the error so the investigator can see the excess.
            extra.push((item, remaining));
        }
    }
    (missing, extra)
}

// Validate the full reports in each cache state before scoring the warm result.
fn cached_report(request: &AnalyzeRequest, label: &str) -> AnalysisReport {
    let cold = run_analysis_with_options(request, true).unwrap();
    let populated = run_analysis_with_options(request, false).unwrap();
    let warm = run_analysis_with_options(request, false).unwrap();
    let json = render_json(&cold).unwrap();
    assert_eq!(
        render_json(&populated).unwrap(),
        json,
        "{label}: cache population differs"
    );
    assert_eq!(
        render_json(&warm).unwrap(),
        json,
        "{label}: warm cache differs"
    );
    let index = index_workspace(&request.config_path).unwrap();
    assert_eq!(
        (index.cache_hits, index.cache_misses),
        (cold.stats.repositories, 0),
        "{label}: cache was not populated"
    );
    warm
}

// Verify negatives identify real HTTP call sites or response reads in the base snapshot.
fn verify_negatives(
    request: &AnalyzeRequest,
    report: &AnalysisReport,
    negatives: &[Negative],
    label: &str,
) {
    for case in negatives {
        let result = query_endpoint_consumers(
            &ConsumersRequest {
                config_path: request.config_path.clone(),
                service: request.target_repo.clone(),
                method: case.method,
                path: case.path.into(),
                field: case.field.map(str::to_owned),
                producer_rev: request.base_rev.clone(),
            },
            false,
            false,
            ConsumerRevision::Head,
        )
        .unwrap();
        assert!(
            result.in_catalog,
            "{label}: negative endpoint {} is missing from the base catalog",
            case.path
        );
        assert!(
            result.consumers.iter().any(|hit| hit.repo == case.repo
                && hit.evidence.file == case.file
                && hit.evidence.line == case.line),
            "{label}: negative site {}:{} was not recognized: {:?}",
            case.file,
            case.line,
            result.consumers
        );
        assert!(
            report
                .findings
                .iter()
                .filter(|f| {
                    let endpoint = f.change.endpoint();
                    endpoint.method == case.method && endpoint.normalized_path == case.path
                })
                .flat_map(|f| &f.consumers)
                .all(|hit| hit.repo != case.repo
                    || hit.evidence.file != case.file
                    || hit.evidence.line != case.line),
            "{label}: negative site {}:{} was attributed",
            case.file,
            case.line
        );
    }
}

// Score exact confidence, location, and dependency identities for every scenario.
fn evaluate(
    scores: &mut Scores,
    label: &str,
    framework: &'static str,
    request: &AnalyzeRequest,
    expected: &Expected<'_>,
) {
    let report = cached_report(request, label);
    let changes = compare_hits(
        expected.findings.iter().map(|f| f.change),
        report.findings.iter().map(|f| change_key(&f.change)),
        |_, _, _| {},
    );
    let direct = compare_hits(
        expected
            .findings
            .iter()
            .flat_map(|f| f.hits.iter().map(|hit| (f.change, *hit))),
        report.findings.iter().flat_map(|f| {
            f.consumers.iter().map(|hit| {
                (
                    change_key(&f.change),
                    (
                        hit.repo.as_str(),
                        hit.evidence.file.as_str(),
                        hit.evidence.line,
                        hit.confidence,
                    ),
                )
            })
        }),
        |(_, (_, file, _, confidence)), expected, matched| {
            scores.record(framework, Some(file), |count| {
                if *confidence == Possible {
                    count.expected_possible += usize::from(expected);
                    count.matched_possible += usize::from(matched);
                } else if matches!(confidence, Confirmed | Likely) {
                    count.expected_tp += usize::from(expected);
                    count.matched_tp += usize::from(matched);
                    count.false_positives += usize::from(!expected);
                    count.false_negatives += usize::from(expected && !matched);
                }
            })
        },
    );
    let expected_indirect = expected.findings.iter().flat_map(|f| {
        f.indirect.iter().map(|(repo, via, confidence, chain)| {
            (f.change, *repo, *via, *confidence, chain.to_vec())
        })
    });
    let actual_indirect = report.findings.iter().flat_map(|f| {
        f.indirect_consumers.iter().map(|hit| {
            (
                change_key(&f.change),
                hit.repo.as_str(),
                hit.via_repo.as_str(),
                hit.confidence,
                hit.dependency_chain
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>(),
            )
        })
    });
    let indirect = compare_hits(
        expected_indirect,
        actual_indirect,
        |_, expected, matched| {
            scores.record(framework, None, |c| {
                c.expected_possible += usize::from(expected);
                c.matched_possible += usize::from(matched);
            })
        },
    );
    let unknown = compare_hits(
        expected.unknown.iter().copied(),
        report.unresolved_calls.iter().map(|call| {
            (
                call.repo.as_str(),
                call.evidence.file.as_str(),
                call.evidence.line,
                call.reason.as_str(),
            )
        }),
        |(_, file, _, _), expected, matched| {
            scores.record(framework, Some(file), |c| {
                c.expected_unknown += usize::from(expected);
                c.matched_unknown += usize::from(matched);
            })
        },
    );
    let diagnostics = compare_hits(
        expected.diagnostics.iter().copied(),
        report.diagnostics.iter().map(|d| {
            (
                d.repo.as_str(),
                d.file.as_str(),
                d.stage,
                d.code,
                d.line,
                d.json_pointer.as_deref(),
            )
        }),
        |(_, file, stage, _, _, _), expected, matched| {
            scores.record(
                framework,
                (*stage == ExtractionStage::Consumer).then_some(file),
                |c| {
                    c.expected_diagnostics += usize::from(expected);
                    c.matched_diagnostics += usize::from(matched);
                },
            )
        },
    );
    let discrepancies = compare_hits(
        expected
            .discrepancies
            .iter()
            .map(|(method, path, kind)| (*method, *path, *kind as u8)),
        report.discrepancies.iter().map(|d| {
            (
                d.endpoint.method,
                d.endpoint.normalized_path.as_str(),
                d.kind as u8,
            )
        }),
        |_, expected, matched| {
            scores.record(framework, None, |c| {
                c.expected_discrepancies += usize::from(expected);
                c.matched_discrepancies += usize::from(matched);
            })
        },
    );
    assert!(
        [
            changes.0.is_empty() && changes.1.is_empty(),
            direct.0.is_empty() && direct.1.is_empty(),
            indirect.0.is_empty() && indirect.1.is_empty(),
            unknown.0.is_empty() && unknown.1.is_empty(),
            diagnostics.0.is_empty() && diagnostics.1.is_empty(),
            discrepancies.0.is_empty() && discrepancies.1.is_empty(),
        ]
        .into_iter()
        .all(|matches| matches),
        "{label}: (missing, extra)\nchanges: {changes:?}\ndirect: {direct:?}\nindirect: {indirect:?}\nunknown: {unknown:?}\ndiagnostics: {diagnostics:?}\ndiscrepancies: {discrepancies:?}"
    );
    assert_eq!(
        report.coverage.as_ref().unwrap().complete,
        expected.diagnostics.is_empty(),
        "{label}: coverage"
    );
    verify_negatives(request, &report, expected.negatives, label);
    for case in expected.negatives {
        scores.record(framework, Some(case.file), |c| {
            c.true_negatives_verified += 1
        });
    }
}

// Evaluate response changes from a versioned external schema with two variants.
fn response_evolution(scores: &mut Scores) {
    let corpus = Corpus::new();
    let before = r#"oneOf:
  - type: object
    required: [kind, badge]
    properties:
      kind: {type: string, enum: [personal]}
      name: {type: string}
      rank: {type: integer}
      badge: {type: string}
      address: {type: string}
      personal: {type: boolean}
  - type: object
    required: [kind, badge]
    properties:
      kind: {type: string, enum: [business]}
      name: {type: string}
      rank: {type: integer}
      badge: {type: string}
      address: {type: string}
      business: {type: boolean}
"#;
    let after = r#"oneOf:
  - type: object
    required: [kind, badge]
    properties:
      kind: {type: string, enum: [personal]}
      rank: {type: string}
      badge: {type: string}
      address: {type: string}
      personal: {type: boolean}
  - type: object
    required: [kind]
    properties:
      kind: {type: string, enum: [business]}
      rank: {type: string}
      address: {type: string}
      business: {type: boolean}
      extra: {type: string}
"#;
    corpus.repo(
        "api",
        &[("openapi.yaml", PROFILE), ("schemas/profile.yaml", before)],
    );
    corpus.snapshot(
        "api",
        &[("schemas/profile.yaml", after)],
        "Evolve external response variants",
    );
    corpus.repo("web", &[
        ("fetch.ts", "const r = await fetch('http://api/profile');\nconst data = await r.json();\nconsole.log(data.name);\n"),
        ("client.ts", "import axios from 'axios';\nexport const api = axios.create({baseURL: 'http://api'});\n"),
        ("index.ts", "export { api } from './client';\n"),
        ("axios.ts", "import { api } from './index';\nconst { data } = await api.get('/profile');\nconsole.log(data.rank);\n"),
        ("Profile.vue", r#"<template>
  <span>{{ profile.badge }}</span>
</template>
<script setup lang="ts">
const res = await fetch('http://api/profile');
const profile = await res.json();
</script>
"#),
        ("untouched.ts", "const r = await fetch('http://api/profile');\nconst data = await r.json();\nconsole.log(data.address, data.extra, data.personal);\n"),
        ("unknown.ts", "export async function load(url: string) {\n  await fetch(url);\n  await fetch(import.meta.env.PROFILE_URL);\n}\n"),
    ]);
    corpus.repo("worker", &[
        ("requests_profile.py", "import requests\nr = requests.get('http://api/profile')\ndata = r.json()\nprint(data['name'])\n"),
        ("httpx_profile.py", "import httpx\nclient = httpx.Client(base_url='http://api')\nr = client.get('/profile')\ndata = r.json()\nprint(data['rank'])\n"),
    ]);
    evaluate(
        scores,
        "response evolution",
        "openapi",
        &corpus.request(OPENAPI, &["web", "worker"], ""),
        &Expected {
            findings: &[
                Finding {
                    change: (Get, "/profile", "response_removed", "name", "200"),
                    hits: &[
                        ("web", "fetch.ts", 3, Confirmed),
                        ("worker", "requests_profile.py", 4, Confirmed),
                    ],
                    indirect: &[],
                },
                Finding {
                    change: (Get, "/profile", "response_type", "rank", "200"),
                    hits: &[
                        ("web", "axios.ts", 3, Confirmed),
                        ("worker", "httpx_profile.py", 5, Confirmed),
                    ],
                    indirect: &[],
                },
                Finding {
                    change: (Get, "/profile", "response_optional", "badge", "200"),
                    hits: &[("web", "Profile.vue", 2, Confirmed)],
                    indirect: &[],
                },
            ],
            negatives: &[
                Negative {
                    method: Get,
                    path: "/profile",
                    field: Some("address"),
                    repo: "web",
                    file: "untouched.ts",
                    line: 3,
                },
                Negative {
                    method: Get,
                    path: "/profile",
                    field: Some("extra"),
                    repo: "web",
                    file: "untouched.ts",
                    line: 3,
                },
                Negative {
                    method: Get,
                    path: "/profile",
                    field: Some("personal"),
                    repo: "web",
                    file: "untouched.ts",
                    line: 3,
                },
            ],
            unknown: &[
                (
                    "web",
                    "unknown.ts",
                    2,
                    "dynamic URL variable `url` cannot be resolved statically",
                ),
                (
                    "web",
                    "unknown.ts",
                    3,
                    "unmapped environment variable (configure under `[env]` in laine.toml)",
                ),
            ],
            ..Expected::default()
        },
    );
}

// Evaluate required arguments, OpenAPI 3.1 nullable types, and unverifiable payloads.
fn request_arguments(scores: &mut Scores) {
    let corpus = Corpus::new();
    let spec = |head| {
        let parameters = [
            json!({"in": "query", "name": "page", "required": head, "schema": {"type": "integer"}}),
            json!({"in": "query", "name": "count", "schema": {"type": if head {"integer"} else {"string"}}}),
            json!({"in": "header", "name": "X-Mode", "required": head, "schema": {"type": "string"}}),
        ];
        json!({
            "openapi": "3.1.0",
            "paths": {"/orders": {"post": {
                "parameters": parameters,
                "requestBody": {"required": true, "content": {"application/json": {"schema": {
                    "type": "object",
                    "required": if head {vec!["code", "tenant"]} else {vec!["code"]},
                    "properties": {
                        "code": {"type": if head {json!("string")} else {json!(["string", "null"])}},
                        "tenant": {"type": "string"}
                    }
                }}}},
                "responses": {"204": {"description": "Accepted"}}
            }}}
        }).to_string()
    };
    corpus.repo("api", &[("openapi.json", &spec(false))]);
    corpus.snapshot(
        "api",
        &[("openapi.json", &spec(true))],
        "Tighten request arguments",
    );
    corpus.repo("web", &[
        ("bad.ts", "fetch('http://api/orders?count=all', {method:'POST', headers:{}, body:JSON.stringify({code:null})});\n"),
        ("good.ts", "import axios from 'axios';\naxios.post('http://api/orders', {code:'new', tenant:'a'}, {params:{page:2, count:3}, headers:{'x-mode':'fast'}});\n"),
        ("dynamic.ts", "export function send(tenant: string) {\n  fetch('http://api/orders?page=2&count=4', {method:'POST', headers:{'X-Mode':'fast'}, body:JSON.stringify({code:'new', tenant})});\n}\n"),
    ]);
    corpus.repo("worker", &[
        ("bad.py", "import requests\nrequests.post('http://api/orders', json={'code': None}, params={'count': 'all'}, headers={'X-Mode': 'fast'})\n"),
        ("good.py", "import httpx\nhttpx.post('http://api/orders', json={'code': 'new', 'tenant': 'a'}, params={'page': 2, 'count': 4}, headers={'x-mode': 'fast'})\n"),
    ]);
    const BAD: &[Hit<'static>] = &[
        ("web", "bad.ts", 1, Confirmed),
        ("worker", "bad.py", 2, Confirmed),
    ];
    evaluate(
        scores,
        "request arguments",
        "openapi",
        &corpus.request("openapi = 'openapi.json'\n", &["web", "worker"], ""),
        &Expected {
            findings: &[
                Finding {
                    change: (Post, "/orders", "body_required", "tenant", ""),
                    hits: &[BAD[0], BAD[1], ("web", "dynamic.ts", 2, Possible)],
                    indirect: &[],
                },
                Finding {
                    change: (Post, "/orders", "body_field_type", "code", ""),
                    hits: BAD,
                    indirect: &[],
                },
                Finding {
                    change: (Post, "/orders", "parameter_required", "page", "query"),
                    hits: BAD,
                    indirect: &[],
                },
                Finding {
                    change: (Post, "/orders", "parameter_required", "X-Mode", "header"),
                    hits: &[BAD[0]],
                    indirect: &[],
                },
                Finding {
                    change: (Post, "/orders", "parameter_type", "count", "query"),
                    hits: BAD,
                    indirect: &[],
                },
            ],
            negatives: &[
                Negative {
                    method: Post,
                    path: "/orders",
                    field: None,
                    repo: "web",
                    file: "good.ts",
                    line: 2,
                },
                Negative {
                    method: Post,
                    path: "/orders",
                    field: None,
                    repo: "worker",
                    file: "good.py",
                    line: 2,
                },
            ],
            ..Expected::default()
        },
    );
}

// Cover JSX prop propagation, static contracts, and the cross-file hop limit.
fn cross_file_contracts(scores: &mut Scores) {
    let corpus = Corpus::new();
    corpus.repo(
        "api",
        &[
            ("openapi.yaml", PROFILE),
            ("schemas/profile.yaml", OLD_FIELD),
        ],
    );
    corpus.snapshot(
        "api",
        &[("schemas/profile.yaml", NEW_FIELD)],
        "Remove profile name",
    );
    let mut files = vec![
        ("tsconfig.json".to_owned(), r#"{"compilerOptions":{"baseUrl":".","paths":{"@/*":["src/*"]}}}"#.to_owned()),
        ("src/service.ts".into(), PROFILE_SERVICE.into()),
        ("src/index.ts".into(), "export { load } from './service';\n".into()),
        ("src/hook.ts".into(), "import { load } from '@/index';\nexport async function useProfile() { return load(); }\n".into()),
        ("src/Page.tsx".into(), "import { useProfile } from './hook';\nimport { Badge } from './Badge';\nexport async function Page() { const profile = await useProfile(); return <Badge profile={profile} />; }\n".into()),
        ("src/Badge.tsx".into(), "export function Badge({profile}: {profile: any}) { return <b>{profile.name}</b>; }\n".into()),
        ("src/schema.ts".into(), "import { z } from 'zod';\nexport const ProfileSchema = z.object({id:z.number(), name:z.string()});\nexport type Profile = z.infer<typeof ProfileSchema>;\n".into()),
        ("src/validated.ts".into(), "import { ProfileSchema } from './schema';\nexport async function load() { const r = await fetch('http://api/profile'); return ProfileSchema.parse(await r.json()); }\n".into()),
        ("src/dto.ts".into(), "export interface ProfileDto { id: number; name: string }\n".into()),
        ("src/generic.ts".into(), "import axios from 'axios';\nimport type { ProfileDto } from './dto';\nexport const load = () => axios.get<ProfileDto>('http://api/profile');\n".into()),
    ];
    let mut prev = "service".to_owned();
    for hop in 1..=7 {
        files.push((
            format!("src/step{hop}.ts"),
            format!("export {{ load }} from './{prev}';\n"),
        ));
        prev = format!("step{hop}");
    }
    files.push((
        "src/too-far.ts".into(),
        "import {load} from './step7';\nconsole.log((await load()).name);\n".into(),
    ));
    let sources: Vec<_> = files
        .iter()
        .map(|(file, source)| (file.as_str(), source.as_str()))
        .collect();
    corpus.repo("web", &sources);
    evaluate(
        scores,
        "cross-file contracts",
        "openapi",
        &corpus.request(OPENAPI, &["web"], ""),
        &Expected {
            findings: &[Finding {
                change: (Get, "/profile", "response_removed", "name", "200"),
                hits: &[
                    ("web", "src/Badge.tsx", 1, Confirmed),
                    ("web", "src/schema.ts", 2, Likely),
                    ("web", "src/dto.ts", 1, Likely),
                ],
                indirect: &[],
            }],
            diagnostics: &[(
                "web",
                "src/step7.ts",
                ExtractionStage::Consumer,
                ExtractionFailure::DataflowLimitReached,
                None,
                None,
            )],
            ..Expected::default()
        },
    );
}

// Compare a mounted Express router with its independently maintained OpenAPI catalog.
fn express_drift(scores: &mut Scores) {
    let corpus = Corpus::new();
    let router = "import express from 'express';\nexport const router = express.Router();\nrouter.get('/stable', handler);\n";
    const SPEC: &str = "openapi: 3.0.3\npaths:\n  /v1/paper-only:\n    get:\n      responses:\n        '204': {description: Accepted}\n";
    corpus.repo("api", &[
        ("openapi.yaml", SPEC),
        ("app.ts", "import express from 'express';\nimport { router } from './router';\nconst app = express();\napp.use('/v1', router);\n"),
        ("router.ts", &format!("{router}router.route('/retired').get(handler);\n")),
    ]);
    corpus.snapshot(
        "api",
        &[("router.ts", router)],
        "Retire mounted Express route",
    );
    corpus.repo(
        "web",
        &[(
            "client.ts",
            "apiClient.get('/v1/retired');\napiClient.get('/v1/stable');\n",
        )],
    );
    corpus.repo(
        "worker",
        &[(
            "client.py",
            "import httpx\nhttpx.get('http://api/v1/retired')\nhttpx.get('http://api/v1/stable')\n",
        )],
    );
    evaluate(scores, "Express drift", "express", &corpus.request(
        "openapi = 'openapi.yaml'\n[services.routes]\nframework = 'express'\nfiles = ['router.ts']\n",
        &["web", "worker"],
        "\n[[clients]]\ncallee = 'apiClient'\ntarget_service = 'api'\nconsumer_service = 'web'\n",
    ), &Expected {
        findings: &[Finding { change: (Get, "/v1/retired", "removed", "", ""),
            hits: &[("web", "client.ts", 1, Confirmed), ("worker", "client.py", 2, Confirmed)], indirect: &[] }],
        negatives: &[
            Negative { method: Get, path: "/v1/stable", field: None, repo: "web", file: "client.ts", line: 2 },
            Negative { method: Get, path: "/v1/stable", field: None, repo: "worker", file: "client.py", line: 3 },
        ],
        discrepancies: &[
            (Get, "/v1/stable", DiscrepancyKind::UndocumentedInOpenApi),
            (Get, "/v1/paper-only", DiscrepancyKind::MissingInCode),
        ],
        ..Expected::default()
    });
}

// Exercise mounted Python routes and language-specific clients across two scenarios.
fn python_routes(scores: &mut Scores) {
    for (framework, producer, removed, stable, ts, py, line) in [
        (
            "fastapi",
            "from fastapi import FastAPI, APIRouter\napp = FastAPI()\nrouter = APIRouter(prefix='/users')\napp.include_router(router, prefix='/v1')\n",
            "/v1/users/retired",
            "/v1/users/stable",
            "// FastAPI client\nfetch('http://api/v1/users/retired');\nfetch('http://api/v1/users/stable');\n",
            "import requests\nclient = requests.Session()\nclient.get('http://api/v1/users/retired')\nclient.get('http://api/v1/users/stable')\n",
            3,
        ),
        (
            "flask",
            "from flask import Flask, Blueprint\napp = Flask(__name__)\nrouter = Blueprint('users', __name__, url_prefix='/original')\napp.register_blueprint(router, url_prefix='/v1')\n",
            "/v1/retired",
            "/v1/stable",
            "import axios from 'axios';\naxios.get('http://api/v1/retired');\naxios.get('http://api/v1/stable');\n",
            "import httpx\nasync def read():\n    async with httpx.AsyncClient(base_url='http://api') as client:\n        await client.get('/v1/retired')\n        await client.get('/v1/stable')\n",
            4,
        ),
    ] {
        let corpus = Corpus::new();
        let head = format!("{producer}@router.get('/stable')\ndef stable():\n    return []\n");
        let base = format!("{head}@router.get('/retired')\ndef retired():\n    return []\n");
        corpus.repo("api", &[("app.py", &base)]);
        corpus.snapshot("api", &[("app.py", &head)], "Retire mounted Python route");
        corpus.repo("web", &[("client.ts", ts)]);
        corpus.repo("worker", &[("client.py", py)]);
        let request = corpus.request(
            &format!("[services.routes]\nframework = '{framework}'\nfiles = ['app.py']\n"),
            &["web", "worker"],
            "",
        );
        evaluate(
            scores,
            framework,
            framework,
            &request,
            &Expected {
                findings: &[Finding {
                    change: (Get, removed, "removed", "", ""),
                    hits: &[
                        ("web", "client.ts", 2, Confirmed),
                        ("worker", "client.py", line, Confirmed),
                    ],
                    indirect: &[],
                }],
                negatives: &[
                    Negative {
                        method: Get,
                        path: stable,
                        field: None,
                        repo: "web",
                        file: "client.ts",
                        line: 3,
                    },
                    Negative {
                        method: Get,
                        path: stable,
                        field: None,
                        repo: "worker",
                        file: "client.py",
                        line: line + 1,
                    },
                ],
                ..Expected::default()
            },
        );
    }
}

// Check clean code-only removals and incomplete Laravel snapshots separately.
fn laravel_routes(scores: &mut Scores) {
    let corpus = Corpus::new();
    const ROUTES: &str = "routes/api.php";
    const BASE: &str = "<?php\nRoute::prefix('v1')->group(function () {\n    Route::get('/stable', Handler::class);\n    Route::get('/retired', Handler::class);\n});\n";
    corpus.repo("api", &[(ROUTES, BASE)]);
    corpus.snapshot(
        "api",
        &[(
            ROUTES,
            &BASE.replace("    Route::get('/retired', Handler::class);\n", ""),
        )],
        "Retire Laravel route",
    );
    corpus.repo(
        "web",
        &[
            (
                "Route.vue",
                "<script setup lang=\"ts\">\nawait $fetch('/v1/retired');\n</script>\n",
            ),
            (
                "routes.ts",
                "fetch('http://api/v1/retired');\nfetch('http://api/v1/stable');\n",
            ),
        ],
    );
    let request = corpus.request(
        "[services.routes]\nframework = 'laravel'\nfiles = ['routes/api.php']\n",
        &["web"],
        "\n[[clients]]\ncallee = '$fetch'\ntarget_service = 'api'\nconsumer_service = 'web'\n",
    );
    evaluate(
        scores,
        "Laravel clean",
        "laravel",
        &request,
        &Expected {
            findings: &[Finding {
                change: (Get, "/v1/retired", "removed", "", ""),
                hits: &[
                    ("web", "Route.vue", 2, Confirmed),
                    ("web", "routes.ts", 1, Confirmed),
                ],
                indirect: &[],
            }],
            negatives: &[Negative {
                method: Get,
                path: "/v1/stable",
                field: None,
                repo: "web",
                file: "routes.ts",
                line: 2,
            }],
            ..Expected::default()
        },
    );
    corpus.snapshot(
        "api",
        &[(ROUTES, "<?php\nRoute::get($path, Handler::class);\n")],
        "Replace declarations with dynamic route",
    );
    evaluate(
        scores,
        "Laravel incomplete",
        "laravel",
        &request,
        &Expected {
            diagnostics: &[(
                "api",
                ROUTES,
                ExtractionStage::ProducerHead,
                ExtractionFailure::RouteDeclarationUnresolved,
                Some(2),
                None,
            )],
            ..Expected::default()
        },
    );
}

// Check transitive attribution, policy suppression, and unresolved external references.
fn indirect_policy_and_references(scores: &mut Scores) {
    let corpus = Corpus::new();
    const DEBUG: &str =
        "  /internal/debug:\n    get:\n      responses:\n        '204': {description: Accepted}\n";
    corpus.repo(
        "api",
        &[
            ("openapi.yaml", &format!("{PROFILE}{DEBUG}")),
            ("schemas/profile.yaml", OLD_FIELD),
        ],
    );
    corpus.snapshot(
        "api",
        &[
            ("schemas/profile.yaml", NEW_FIELD),
            ("openapi.yaml", PROFILE),
        ],
        "Remove profile field and debug endpoint",
    );
    corpus.repo("service-b", &[
        ("profile.ts", "const r = await fetch('http://api/profile');\nconst data = await r.json();\nconsole.log(data.name);\n"),
        ("debug.ts", "fetch('http://api/internal/debug');\n"),
    ]);
    corpus.repo(
        "service-c",
        &[("ledger.ts", "fetch('http://service-b/ledger');\n")],
    );
    let services = &["service-b", "service-c"];
    let unfiltered = corpus.request(OPENAPI, services, "");
    let control = run_analysis_with_options(&unfiltered, true).unwrap();
    assert!(
        control.findings.iter().any(|finding| {
            change_key(&finding.change) == (Get, "/internal/debug", "removed", "", "")
                && finding.consumers.iter().any(|hit| {
                    hit.repo == "service-b"
                        && hit.evidence.file == "debug.ts"
                        && hit.evidence.line == 1
                        && hit.confidence == Confirmed
                })
        }),
        "the debug endpoint must be reported before applying policy"
    );
    let request = corpus.request(
        OPENAPI,
        services,
        "\n[policy]\nignore_endpoints = ['GET /internal/**']\n",
    );
    evaluate(
        scores,
        "transitive and policy",
        "openapi",
        &request,
        &Expected {
            findings: &[Finding {
                change: (Get, "/profile", "response_removed", "name", "200"),
                hits: &[("service-b", "profile.ts", 3, Confirmed)],
                indirect: &[(
                    "service-c",
                    "service-b",
                    Possible,
                    &["service-b", "service-c"],
                )],
            }],
            negatives: &[Negative {
                method: Get,
                path: "/internal/debug",
                field: None,
                repo: "service-b",
                file: "debug.ts",
                line: 1,
            }],
            ..Expected::default()
        },
    );
    corpus.snapshot(
        "api",
        &[(
            "openapi.yaml",
            &PROFILE.replace("profile.yaml", "missing.yaml"),
        )],
        "Reference a missing external schema",
    );
    evaluate(
        scores,
        "broken reference",
        "openapi",
        &request,
        &Expected {
            diagnostics: &[(
                "api",
                "openapi.yaml",
                ExtractionStage::ProducerHead,
                ExtractionFailure::OpenApiReferenceUnresolved,
                Some(10),
                Some("/paths/~1profile/get/responses/200/content/application~1json/schema/$ref"),
            )],
            ..Expected::default()
        },
    );
}

// Run all eight scenarios and emit the stable, versioned evaluation metrics.
#[test]
fn corpus_precision_and_recall() {
    let mut scores = Scores::default();
    response_evolution(&mut scores);
    request_arguments(&mut scores);
    cross_file_contracts(&mut scores);
    express_drift(&mut scores);
    python_routes(&mut scores);
    laravel_routes(&mut scores);
    indirect_policy_and_references(&mut scores);
    let totals = scores.total.metrics();
    assert_eq!(totals["precision"], 1.0);
    assert_eq!(totals["recall"], 1.0);
    assert_eq!(totals["f1"], 1.0);
    assert_eq!(totals["false_positives"], 0);
    assert_eq!(totals["false_negatives"], 0);
    assert_eq!(totals["matched_tp"], totals["expected_tp"]);
    assert_eq!(totals["matched_possible"], totals["expected_possible"]);
    assert_eq!(totals["matched_unknown"], totals["expected_unknown"]);
    assert_eq!(
        totals["matched_diagnostics"],
        totals["expected_diagnostics"]
    );
    assert_eq!(
        totals["matched_discrepancies"],
        totals["expected_discrepancies"]
    );
    let by_framework: BTreeMap<_, _> = scores
        .by_framework
        .into_iter()
        .map(|(framework, counts)| (framework, counts.metrics()))
        .collect();
    let report = json!({
        "schema_version": 1,
        "scenarios": 8,
        "totals": totals,
        "by_framework": by_framework,
    });
    println!("{}", serde_json::to_string_pretty(&report).unwrap());
}
