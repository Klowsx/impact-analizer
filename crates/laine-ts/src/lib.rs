use oxc_allocator::Allocator;
use oxc_parser::Parser;
use oxc_semantic::SemanticBuilder;
use oxc_span::SourceType;

/// Smoke-test parser + semantic entrypoint verifying `oxc` 0.160 integration on TypeScript sources.
/// Phase 1 will expand this into HTTP call extraction (`fetch`/Axios) and field-read data-flow.
pub fn parse_typescript_statement_count(source: &str) -> Result<usize, Vec<String>> {
    let allocator = Allocator::default();
    let source_type = SourceType::ts();
    let parsed = Parser::new(&allocator, source, source_type).parse();

    if !parsed.diagnostics.is_empty() {
        return Err(parsed
            .diagnostics
            .into_iter()
            .map(|e| e.to_string())
            .collect());
    }

    let semantic = SemanticBuilder::new().build(&parsed.program);
    if !semantic.diagnostics.is_empty() {
        return Err(semantic
            .diagnostics
            .into_iter()
            .map(|e| e.to_string())
            .collect());
    }

    Ok(parsed.program.body.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sample_typescript_consumer() {
        let code = r#"
export async function getInvoiceCustomer(id: string): Promise<string> {
  const res = await fetch(`http://customer-service/customers/${id}`);
  const customer = await res.json();
  return customer.fullName;
}
"#;
        let statements = parse_typescript_statement_count(code).expect("valid TS should parse");
        assert_eq!(statements, 1);
    }
}
