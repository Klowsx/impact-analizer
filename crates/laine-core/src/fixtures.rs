use std::fs;
use std::path::{Path, PathBuf};
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

/// Materializes the 4 independent Git repositories described in PLAN.md under `root`.
/// Safe to re-run: removes an existing directory at `root` before regenerating.
pub fn create_demo_workspace(root: &Path) -> Result<DemoWorkspacePaths> {
    if root.exists() {
        fs::remove_dir_all(root).map_err(|source| LaineError::Io {
            path: root.to_path_buf(),
            source,
        })?;
    }
    fs::create_dir_all(root).map_err(|source| LaineError::Io {
        path: root.to_path_buf(),
        source,
    })?;

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

    #[test]
    fn generates_four_reproducible_git_repos_and_valid_config() {
        let tmp = std::env::temp_dir().join(format!("laine-fixture-test-{}", std::process::id()));

        let paths = create_demo_workspace(&tmp).expect("demo workspace should generate");
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

        let _ = fs::remove_dir_all(&tmp);
    }
}
