use std::collections::BTreeSet;

use crate::config::ServiceConfig;

// Split an absolute HTTP(S) URL into scheme and remainder.
fn http_url_parts(value: &str) -> Option<(&str, &str)> {
    let (scheme, rest) = value.trim().split_once("://")?;
    (scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https"))
        .then_some((scheme, rest))
}

/// Check whether the URL declares an HTTP(S) scheme.
pub fn is_absolute_http_url(value: &str) -> bool {
    http_url_parts(value).is_some()
}

/// Keep the HTTP origin and path for matching, dropping userinfo, query, and fragment.
pub fn matching_http_url(value: &str) -> Option<String> {
    let (scheme, rest) = http_url_parts(value)?;

    // Isolate the authority so an @ in the path stays intact.
    let rest = rest.split(['?', '#']).next()?.trim_end();
    let (authority, path) = rest.find('/').map_or((rest, ""), |i| rest.split_at(i));
    let host = authority.rsplit('@').next()?;
    if host.is_empty() || authority.contains('\\') || authority.contains(char::is_whitespace) {
        return None;
    }
    Some(format!("{}://{host}{path}", scheme.to_ascii_lowercase()))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceEndpointMatch {
    pub service: String,
    pub normalized_path: String,
}

/// Resolve the most specific service base and report ties between different services.
pub fn resolve_http_service_url(
    raw_url: &str,
    services: &[ServiceConfig],
) -> Result<Option<ServiceEndpointMatch>, String> {
    let Some(url) = matching_http_url(raw_url) else {
        return Ok(None);
    };
    match_http_service_url(&url, services)
}

/// Append a relative request to a static client base, or resolve an absolute override.
pub fn resolve_url_with_base(
    raw_url: &str,
    base: Result<&str, &str>,
    services: &[ServiceConfig],
) -> Result<Option<ServiceEndpointMatch>, String> {
    if is_absolute_http_url(raw_url) {
        return resolve_http_service_url(raw_url, services);
    }

    let Some(base) = matching_http_url(base?) else {
        return Ok(None);
    };
    // Strip the base's query and fragment before appending the request path.
    let base = base.trim_end_matches('/');
    let relative = raw_url.trim().trim_start_matches('/');
    resolve_http_service_url(&format!("{base}/{relative}"), services)
}

/// Use the configured target for relative paths and service bases for absolute overrides.
pub fn resolve_custom_client_url(
    raw_url: &str,
    target_service: &str,
    path_prefix: Option<&str>,
    services: &[ServiceConfig],
) -> Result<Option<ServiceEndpointMatch>, String> {
    if is_absolute_http_url(raw_url) {
        return resolve_http_service_url(raw_url, services);
    }
    Ok(Some(ServiceEndpointMatch {
        service: target_service.to_owned(),
        normalized_path: normalize_relative_path(raw_url, path_prefix),
    }))
}

// Match sanitized URLs at origin and path segment boundaries.
fn match_http_service_url(
    url: &str,
    services: &[ServiceConfig],
) -> Result<Option<ServiceEndpointMatch>, String> {
    let mut names = BTreeSet::new();
    let mut longest = 0;
    for service in services {
        for base in &service.base_urls {
            let Some(base) = matching_http_url(base) else {
                continue;
            };
            let base = base.trim_end_matches('/');
            if base.len() >= longest
                && url
                    .strip_prefix(base)
                    .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
            {
                if base.len() > longest {
                    longest = base.len();
                    names.clear();
                }
                names.insert(service.name.as_str());
            }
        }
    }

    match names.len() {
        0 => Ok(None),
        1 => Ok(names.first().map(|name| ServiceEndpointMatch {
            service: (*name).to_string(),
            normalized_path: normalize_relative_path(&url[longest..], None),
        })),
        _ => Err(format!(
            "ambiguous service base URL (candidates: {})",
            names.into_iter().collect::<Vec<_>>().join(", ")
        )),
    }
}

/// Normalize a relative path and prepend its configured prefix, when present.
pub fn normalize_relative_path(raw_path: &str, path_prefix: Option<&str>) -> String {
    let path = raw_path.split(['?', '#']).next().unwrap_or(raw_path).trim();
    let segments = path_prefix
        .into_iter()
        .chain([path])
        .flat_map(|part| part.trim().split('/'))
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    format!("/{}", segments.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Build service configs with only the URLs needed for matching.
    fn service(name: &str, bases: &[&str]) -> ServiceConfig {
        ServiceConfig {
            name: name.into(),
            path: name.into(),
            openapi: None,
            routes: None,
            base_urls: bases.iter().map(|base| (*base).into()).collect(),
        }
    }

    // Check origin and path boundaries, preserving @ in path segments.
    #[test]
    fn matches_the_most_specific_base_without_credentials() {
        let services = [
            service("root", &["https://example.test:8443"]),
            service(
                "mounted",
                &[
                    "HTTPS://user:example@external.test:8443/v1",
                    "HTTPS://user:example@example.test:8443/v1/?q=1#frag",
                    "https://example.test:8443/v1",
                ],
            ),
        ];
        let resolve = |url| resolve_http_service_url(url, &services).unwrap();
        let matched = resolve(" https://another@example.test:8443/v1//a@b/?q=2#frag ").unwrap();
        assert_eq!(matched.service, "mounted");
        assert_eq!(matched.normalized_path, "/a@b");
        assert_eq!(
            resolve("https://example.test:8443/v10").unwrap().service,
            "root"
        );
        for url in [
            "https://example.test:8444/v1/a",
            "https://example.test:8443.evil/v1/a",
            "https://example.test:8443@other.test/v1/a",
            r"https://other.test\@example.test:8443/v1/a",
            "/relative",
        ] {
            assert!(resolve(url).is_none(), "{url}");
        }
    }

    // Check that ties are identical for either service order and exclude shorter bases.
    #[test]
    fn reports_sorted_ambiguity_between_services() {
        let mut services = [
            service("zeta", &["https://example.test/v1"]),
            service("root", &["https://example.test"]),
            service("alpha", &["https://example.test/v1/"]),
        ];
        for _ in 0..2 {
            assert_eq!(
                resolve_http_service_url("https://example.test/v1", &services).unwrap_err(),
                "ambiguous service base URL (candidates: alpha, zeta)"
            );
            services.reverse();
        }
    }

    // Exercise static bases, configured wrapper prefixes, and absolute overrides.
    #[test]
    fn resolves_base_and_custom_client_paths() {
        let services = [service("api", &["https://example.test/v1"])];
        let matched = resolve_url_with_base(
            " /orders//:id/?expand=items#top ",
            Ok(" https://user:example@example.test/v1//?base=1#base "),
            &services,
        )
        .unwrap()
        .unwrap();
        assert_eq!(matched.normalized_path, "/orders/:id");
        assert_eq!(
            resolve_url_with_base("/", Ok("https://example.test/v1/"), &services)
                .unwrap()
                .unwrap()
                .normalized_path,
            "/"
        );
        assert_eq!(
            resolve_url_with_base("/orders", Err("missing base"), &services).unwrap_err(),
            "missing base"
        );
        assert_eq!(
            resolve_url_with_base(
                "https://example.test/v1/orders",
                Err("missing base"),
                &services
            )
            .unwrap()
            .unwrap()
            .service,
            "api"
        );

        let custom =
            |url| resolve_custom_client_url(url, "configured", Some("/proxy//"), &services);
        let matched = custom("orders//:id?expand=items#top").unwrap().unwrap();
        assert_eq!(matched.service, "configured");
        assert_eq!(matched.normalized_path, "/proxy/orders/:id");
        let matched = custom("https://example.test/v1/orders").unwrap().unwrap();
        assert_eq!(matched.service, "api");
        assert_eq!(matched.normalized_path, "/orders");
    }
}
