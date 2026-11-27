use laine_core::config::ServiceConfig;
use laine_core::url::resolve_http_service_url;
pub use laine_core::url::{ServiceEndpointMatch, is_absolute_http_url, normalize_relative_path};

// Return unique matches for passes that do not emit diagnostics.
pub fn resolve_url_against_services(
    raw_url: &str,
    services: &[ServiceConfig],
) -> Option<ServiceEndpointMatch> {
    resolve_http_service_url(raw_url, services).ok().flatten()
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
