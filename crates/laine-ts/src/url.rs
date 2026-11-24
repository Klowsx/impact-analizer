use laine_core::config::ServiceConfig;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceEndpointMatch {
    pub service: String,
    pub normalized_path: String,
}

/// Matches a resolved URL (e.g. `http://customer-service/customers/:id?expand=1`)
/// against the `base_urls` configured in `laine.toml`.
///
/// Chooses the longest matching `base_url` prefix so specific mount prefixes win over broad origins.
pub fn resolve_url_against_services(
    raw_url: &str,
    services: &[ServiceConfig],
) -> Option<ServiceEndpointMatch> {
    let without_fragment = raw_url.split('#').next().unwrap_or(raw_url);
    let without_query = without_fragment
        .split('?')
        .next()
        .unwrap_or(without_fragment)
        .trim();

    let mut best_match: Option<(usize, ServiceEndpointMatch)> = None;

    for service in services {
        for base_url in &service.base_urls {
            let trimmed_base = base_url.trim().trim_end_matches('/');
            if trimmed_base.is_empty() {
                continue;
            }

            if let Some(rest) = strip_base_prefix(without_query, trimmed_base) {
                let normalized_path = normalize_consumer_path(rest);
                let score = trimmed_base.len();
                let should_replace = best_match
                    .as_ref()
                    .is_none_or(|(best_len, _)| score > *best_len);

                if should_replace {
                    best_match = Some((
                        score,
                        ServiceEndpointMatch {
                            service: service.name.clone(),
                            normalized_path,
                        },
                    ));
                }
            }
        }
    }

    best_match.map(|(_, matched)| matched)
}

fn strip_base_prefix<'a>(url: &'a str, base: &str) -> Option<&'a str> {
    let rest = url.strip_prefix(base)?;
    if rest.is_empty() || rest.starts_with('/') {
        Some(rest)
    } else {
        None
    }
}

fn normalize_consumer_path(raw_path: &str) -> String {
    let segments: Vec<&str> = raw_path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();

    if segments.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", segments.join("/"))
    }
}

/// Normalizes a relative endpoint path (stripping query/hash and optionally prepending `path_prefix`).
pub fn normalize_relative_path(raw_path: &str, path_prefix: Option<&str>) -> String {
    let without_fragment = raw_path.split('#').next().unwrap_or(raw_path);
    let without_query = without_fragment
        .split('?')
        .next()
        .unwrap_or(without_fragment)
        .trim();

    let combined = match path_prefix {
        Some(prefix) if !prefix.trim().is_empty() => {
            let p = prefix.trim().trim_end_matches('/');
            let r = without_query.trim_start_matches('/');
            if r.is_empty() {
                p.to_string()
            } else {
                format!("{p}/{r}")
            }
        }
        _ => without_query.to_string(),
    };

    normalize_consumer_path(&combined)
}

pub fn is_absolute_http_url(url: &str) -> bool {
    let trimmed = url.trim();
    trimmed.starts_with("http://") || trimmed.starts_with("https://")
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn sample_services() -> Vec<ServiceConfig> {
        vec![ServiceConfig {
            name: "customer-service".into(),
            path: PathBuf::from("./customer-service"),
            openapi: Some(PathBuf::from("openapi.yaml")),
            base_urls: vec![
                "http://customer-service".into(),
                "https://api.company.internal".into(),
            ],
        }]
    }

    #[test]
    fn matches_registered_base_url_and_strips_query_string() {
        let matched = resolve_url_against_services(
            "http://customer-service/customers/:id?include=orders#top",
            &sample_services(),
        )
        .expect("registered host should match");

        assert_eq!(matched.service, "customer-service");
        assert_eq!(matched.normalized_path, "/customers/:id");
    }

    #[test]
    fn does_not_match_partial_hostname_prefix() {
        let matched = resolve_url_against_services(
            "http://customer-service-evil.com/customers/1",
            &sample_services(),
        );
        assert!(matched.is_none());
    }
}
