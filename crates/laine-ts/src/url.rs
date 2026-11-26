pub use laine_core::config::is_absolute_http_url;
use laine_core::config::{ServiceConfig, matching_http_url};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceEndpointMatch {
    pub service: String,
    pub normalized_path: String,
}

/// Matches a resolved URL (e.g. `http://customer-service/customers/:id?expand=1`)
/// against the `base_urls` configured in `laine.toml`.
///
/// Chooses the longest matching `base_url` prefix so specific mount prefixes win over broad origins.
// Compara URL y bases saneadas y conserva la coincidencia más específica.
pub fn resolve_url_against_services(
    raw_url: &str,
    services: &[ServiceConfig],
) -> Option<ServiceEndpointMatch> {
    let url = matching_http_url(raw_url)?;

    let mut best_match: Option<(usize, ServiceEndpointMatch)> = None;

    for service in services {
        for base_url in &service.base_urls {
            let Some(base) = matching_http_url(base_url) else {
                continue;
            };
            let trimmed_base = base.trim_end_matches('/');

            if let Some(rest) = strip_base_prefix(&url, trimmed_base) {
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn sample_services() -> Vec<ServiceConfig> {
        vec![ServiceConfig {
            name: "customer-service".into(),
            path: PathBuf::from("./customer-service"),
            openapi: Some(PathBuf::from("openapi.yaml")),
            routes: None,
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
    // Verifica los límites de host, puerto y ruta con credenciales de URL ficticias.
    fn matches_credentialed_urls_using_origin_and_mount_path() {
        let mut services = sample_services();
        services[0].base_urls =
            vec!["https://user:FICTITIOUS_PASSWORD@api.example:8443/v1/?q=FICTITIOUS_QUERY".into()];
        let matched = resolve_url_against_services(
            " https://user:FICTITIOUS_USERINFO@api.example:8443/v1/a@b?q=1#top ",
            &services,
        )
        .unwrap();
        assert_eq!(matched.normalized_path, "/a@b");
        for url in [
            "https://api.example:8444/v1/x",
            "https://api.example:8443/v10/x",
            "https://api.example:8443@other.example/v1/x",
            r"https://other.example\@api.example:8443/v1/x",
        ] {
            assert!(resolve_url_against_services(url, &services).is_none());
        }
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
