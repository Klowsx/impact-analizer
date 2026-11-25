use std::fs;
use std::io;
use std::path::{Path, PathBuf, is_separator};
use std::process::Command;

use crate::error::{LaineError, Result};

const FIXTURE_DATE: &str = "2026-01-15T12:00:00Z";
const FIXTURE_DATE_V2: &str = "2026-01-16T12:00:00Z";

#[derive(Debug, Clone)]
pub struct DemoWorkspacePaths {
    pub root: PathBuf,
    pub config_path: PathBuf,
    pub customer_service: PathBuf,
    pub billing_service: PathBuf,
    pub notification_service: PathBuf,
    pub shipping_service: PathBuf,
}

/// Crea los cuatro repositorios Git de la demo en un destino nuevo.
pub fn create_demo_workspace(root: &Path) -> Result<DemoWorkspacePaths> {
    create_demo_destination(root)?;

    let customer = root.join("customer-service");
    let billing = root.join("billing-service");
    let notification = root.join("notification-service");
    let shipping = root.join("shipping-service");

    build_customer_service(&customer)?;
    build_billing_service(&billing)?;
    build_notification_service(&notification)?;
    build_shipping_service(&shipping)?;

    let config_path = root.join("laine.toml");
    let config_toml = r#"version = 1

[[services]]
name = "customer-service"
path = "./customer-service"
openapi = "openapi.yaml"
base_urls = [
  "http://customer-service",
  "https://api.company.internal/customers"
]

[[services]]
name = "billing-service"
path = "./billing-service"

[[services]]
name = "notification-service"
path = "./notification-service"

[[services]]
name = "shipping-service"
path = "./shipping-service"
"#;
    write_file(&config_path, config_toml)?;

    Ok(DemoWorkspacePaths {
        root: root.to_path_buf(),
        config_path,
        customer_service: customer,
        billing_service: billing,
        notification_service: notification,
        shipping_service: shipping,
    })
}

// Crea los padres necesarios y reserva el destino sin reutilizar rutas existentes.
fn create_demo_destination(root: &Path) -> Result<()> {
    // Path normaliza los puntos finales al consultar file_name, así que se revisa la ruta original.
    let last_component = root
        .as_os_str()
        .as_encoded_bytes()
        .rsplit(|byte| is_separator(char::from(*byte)))
        .find(|component| !component.is_empty());
    if root.file_name().is_none() || matches!(last_component, Some(b"." | b"..")) {
        return Err(LaineError::Io {
            path: root.to_path_buf(),
            source: io::Error::new(
                io::ErrorKind::InvalidInput,
                "choose a new directory by name as the demo destination",
            ),
        });
    }

    if let Some(parent) = root.parent().filter(|path| !path.as_os_str().is_empty()) {
        fs::create_dir_all(parent).map_err(|source| LaineError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }

    // create_dir falla si el destino ya existe, incluidas las ejecuciones simultáneas.
    fs::create_dir(root).map_err(|source| LaineError::Io {
        path: root.to_path_buf(),
        source: if source.kind() == io::ErrorKind::AlreadyExists {
            io::Error::new(
                io::ErrorKind::AlreadyExists,
                "demo destination already exists; choose a new directory",
            )
        } else {
            source
        },
    })
}

fn build_customer_service(dir: &Path) -> Result<()> {
    init_repo(dir)?;

    let openapi_v1 = r#"openapi: 3.0.3
info:
  title: Customer Service API
  version: 1.0.0
paths:
  /customers/{id}:
    get:
      summary: Get customer by ID
      parameters:
        - name: id
          in: path
          required: true
          schema:
            type: string
      responses:
        '200':
          description: Customer details
          content:
            application/json:
              schema:
                type: object
                required:
                  - id
                  - fullName
                  - email
                  - address
                properties:
                  id:
                    type: integer
                  fullName:
                    type: string
                  email:
                    type: string
                  address:
                    type: string
"#;

    let controller_v1 = r#"export interface CustomerResponse {
  id: number;
  fullName: string;
  email: string;
  address: string;
}

export function getCustomer(id: string): CustomerResponse {
  return {
    id: Number(id),
    fullName: "John Doe",
    email: "john@example.com",
    address: "123 Main St",
  };
}
"#;

    write_file(&dir.join("openapi.yaml"), openapi_v1)?;
    write_file(&dir.join("src/routes/customers.ts"), controller_v1)?;
    commit_all(
        dir,
        "feat: initial customer API with fullName",
        FIXTURE_DATE,
    )?;

    // Create breaking-change branch without modifying `main`.
    run_git(dir, &["checkout", "-b", "feature/split-customer-name"])?;

    let openapi_v2 = r#"openapi: 3.0.3
info:
  title: Customer Service API
  version: 2.0.0
paths:
  /customers/{id}:
    get:
      summary: Get customer by ID
      parameters:
        - name: id
          in: path
          required: true
          schema:
            type: string
      responses:
        '200':
          description: Customer details
          content:
            application/json:
              schema:
                type: object
                required:
                  - id
                  - firstName
                  - lastName
                  - email
                  - address
                properties:
                  id:
                    type: integer
                  firstName:
                    type: string
                  lastName:
                    type: string
                  email:
                    type: string
                  address:
                    type: string
"#;

    let controller_v2 = r#"export interface CustomerResponse {
  id: number;
  firstName: string;
  lastName: string;
  email: string;
  address: string;
}

export function getCustomer(id: string): CustomerResponse {
  return {
    id: Number(id),
    firstName: "John",
    lastName: "Doe",
    email: "john@example.com",
    address: "123 Main St",
  };
}
"#;

    write_file(&dir.join("openapi.yaml"), openapi_v2)?;
    write_file(&dir.join("src/routes/customers.ts"), controller_v2)?;
    commit_all(
        dir,
        "refactor!: split fullName into firstName and lastName",
        FIXTURE_DATE_V2,
    )?;

    // Leave repo checked out on main so working tree stays clean while both refs exist.
    run_git(dir, &["checkout", "main"])?;
    Ok(())
}

fn build_billing_service(dir: &Path) -> Result<()> {
    init_repo(dir)?;

    let invoice_ts = r#"export async function getInvoiceCustomer(id: string): Promise<string> {
  const res = await fetch(`http://customer-service/customers/${id}`);
  const customer = await res.json();
  return `Invoice for ${customer.fullName} (${customer.email})`;
}
"#;

    let legacy_sync_ts = r#"declare function resolveLegacyUrl(key: string): string;

export async function syncLegacyCustomer(key: string): Promise<unknown> {
  const url = resolveLegacyUrl(key);
  const res = await fetch(url);
  return res.json();
}
"#;

    write_file(&dir.join("src/invoices/customer.ts"), invoice_ts)?;
    write_file(&dir.join("src/legacy/sync.ts"), legacy_sync_ts)?;
    commit_all(dir, "feat: add billing customer lookup", FIXTURE_DATE)?;
    Ok(())
}

fn build_notification_service(dir: &Path) -> Result<()> {
    init_repo(dir)?;

    let welcome_ts = r#"import axios from "axios";

export async function sendWelcomeEmail(customerId: string): Promise<string> {
  const response = await axios.get(`http://customer-service/customers/${customerId}`);
  const recipient = response.data.fullName;
  return `Welcome, ${recipient}!`;
}
"#;

    write_file(&dir.join("src/email/welcome.ts"), welcome_ts)?;
    commit_all(dir, "feat: add welcome email consumer", FIXTURE_DATE)?;
    Ok(())
}

fn build_shipping_service(dir: &Path) -> Result<()> {
    init_repo(dir)?;

    let address_ts = r#"export async function buildShippingLabel(customerId: string): Promise<string> {
  const res = await fetch(`http://customer-service/customers/${customerId}`);
  const customer = await res.json();
  return `Ship to: ${customer.address}`;
}
"#;

    write_file(&dir.join("src/labels/address.ts"), address_ts)?;
    commit_all(dir, "feat: add shipping label address lookup", FIXTURE_DATE)?;
    Ok(())
}

fn init_repo(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir).map_err(|source| LaineError::Io {
        path: dir.to_path_buf(),
        source,
    })?;
    run_git(dir, &["init", "-b", "main"])?;
    run_git(dir, &["config", "user.name", "Laine Fixture"])?;
    run_git(dir, &["config", "user.email", "fixture@laine.local"])?;
    Ok(())
}

fn commit_all(dir: &Path, message: &str, date: &str) -> Result<()> {
    run_git(dir, &["add", "."])?;
    let output = Command::new("git")
        .current_dir(dir)
        .args(["commit", "-m", message])
        .env("GIT_AUTHOR_DATE", date)
        .env("GIT_COMMITTER_DATE", date)
        .output()
        .map_err(|source| LaineError::Io {
            path: dir.to_path_buf(),
            source,
        })?;

    if !output.status.success() {
        return Err(LaineError::Git {
            command: format!("git commit -m {message:?}"),
            message: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    Ok(())
}

fn run_git(dir: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .map_err(|source| LaineError::Io {
            path: dir.to_path_buf(),
            source,
        })?;

    if !output.status.success() {
        return Err(LaineError::Git {
            command: format!("git {}", args.join(" ")),
            message: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn write_file(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| LaineError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    fs::write(path, content).map_err(|source| LaineError::Io {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::WorkspaceConfig;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        // Reserva un directorio temporal exclusivo para cada prueba.
        fn new() -> Self {
            static NEXT_ID: AtomicUsize = AtomicUsize::new(0);

            loop {
                let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
                let path = std::env::temp_dir()
                    .join(format!("laine-fixture-test-{}-{id}", std::process::id()));
                match fs::create_dir(&path) {
                    Ok(()) => return Self(path),
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(error) => panic!("could not create temporary directory: {error}"),
                }
            }
        }
    }

    impl Drop for TestDirectory {
        // Limpia el directorio reservado para la prueba.
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    // Comprueba la configuración y los repositorios generados en un destino nuevo.
    #[test]
    fn generates_four_reproducible_git_repos_and_valid_config() {
        let tmp = TestDirectory::new();
        let root = tmp.0.join("fixtures/demo");

        let paths = create_demo_workspace(&root).expect("demo workspace should generate");
        let cfg = WorkspaceConfig::load_from_path(&paths.config_path)
            .expect("generated laine.toml should parse");

        assert_eq!(cfg.services.len(), 4);
        assert!(paths.customer_service.join(".git").exists());
        assert!(paths.billing_service.join(".git").exists());
        assert!(paths.notification_service.join(".git").exists());
        assert!(paths.shipping_service.join(".git").exists());

        // Verify both main and feature/split-customer-name exist in customer-service.
        let base_ref = run_git(&paths.customer_service, &["rev-parse", "main"]).unwrap();
        let head_ref = run_git(
            &paths.customer_service,
            &["rev-parse", "feature/split-customer-name"],
        )
        .unwrap();
        assert_ne!(base_ref, head_ref);
    }

    // Comprueba que el comando rechaza un directorio o archivo ajeno sin modificarlo.
    #[test]
    fn rejects_existing_destinations_without_changing_them() {
        let tmp = TestDirectory::new();
        let file = tmp.0.join("keep.txt");
        fs::write(&file, b"keep this content").unwrap();

        for root in [&tmp.0, &file] {
            let error = create_demo_workspace(root).unwrap_err();
            assert!(
                matches!(error, LaineError::Io { source, .. } if source.kind() == io::ErrorKind::AlreadyExists)
            );
        }

        assert_eq!(fs::read(&file).unwrap(), b"keep this content");
        assert!(!tmp.0.join("laine.toml").exists());
        assert!(!tmp.0.join("customer-service").exists());
    }

    // Comprueba que una segunda ejecución preserva los cambios dentro de la demo.
    #[test]
    fn rejects_reruns_without_changing_the_existing_demo() {
        let tmp = TestDirectory::new();
        let root = tmp.0.join("demo");
        let paths = create_demo_workspace(&root).unwrap();
        let file = paths.customer_service.join("local-notes.txt");
        fs::write(&file, b"local edits").unwrap();
        let head = run_git(&paths.customer_service, &["rev-parse", "HEAD"]).unwrap();

        let error = create_demo_workspace(&root).unwrap_err();
        assert!(
            matches!(error, LaineError::Io { source, .. } if source.kind() == io::ErrorKind::AlreadyExists)
        );
        assert_eq!(fs::read(&file).unwrap(), b"local edits");
        assert_eq!(
            run_git(&paths.customer_service, &["rev-parse", "HEAD"]).unwrap(),
            head
        );
    }

    // Comprueba que un destino terminado en punto no crea directorios intermedios.
    #[test]
    fn rejects_dot_destinations_before_creating_parents() {
        let tmp = TestDirectory::new();
        let missing = tmp.0.join("missing");

        for suffix in [".", ".."] {
            let error = create_demo_workspace(&missing.join(suffix)).unwrap_err();
            assert!(
                matches!(error, LaineError::Io { source, .. } if source.kind() == io::ErrorKind::InvalidInput)
            );
        }
        assert!(!missing.exists());
    }
}
