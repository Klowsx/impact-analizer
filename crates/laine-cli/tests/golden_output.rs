use std::fs;
use std::path::Path;
use std::process::Command;

use laine_core::fixtures::create_demo_workspace;

#[test]
fn cli_outputs_match_committed_golden_files() {
    // Generate the committed demo fixture and compare each CLI format with its checked-in output.
    let root = std::env::temp_dir().join(format!("laine-golden-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let paths = create_demo_workspace(&root).expect("demo workspace should generate");
    let binary = env!("CARGO_BIN_EXE_laine");
    let golden_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/golden");
    fs::create_dir_all(&golden_dir).expect("golden fixture directory should exist");

    for format in ["terminal", "markdown", "json"] {
        let output = Command::new(binary)
            .args(["analyze", "--config"])
            .arg(&paths.config_path)
            .args([
                "--repo",
                "customer-service",
                "--base",
                "main",
                "--head",
                "feature/split-customer-name",
                "--format",
                format,
            ])
            .output()
            .expect("laine analyze should run");

        assert_eq!(output.status.code(), Some(2), "{format} exit code");
        assert!(output.stderr.is_empty(), "{format} stderr should be empty");
        let actual = normalize_output(format, &String::from_utf8(output.stdout).unwrap());
        let snapshot = golden_dir.join(format!("{format}.snap"));

        if std::env::var_os("UPDATE_GOLDEN").is_some() {
            fs::write(snapshot, &actual).expect("golden output should be written");
        } else {
            let expected = fs::read_to_string(&snapshot)
                .unwrap_or_else(|error| panic!("could not read {}: {error}", snapshot.display()));
            assert_eq!(actual, normalize_line_endings(&expected), "{format} output");
        }
    }

    fs::remove_dir_all(root).expect("temporary demo workspace should be removed");
}

// Normalize platform-specific newlines and volatile report fields before snapshot comparison.
fn normalize_output(format: &str, output: &str) -> String {
    if format == "json" {
        let mut report: serde_json::Value = serde_json::from_str(output).unwrap();
        normalize_json(&mut report);
        return format!("{}\n", serde_json::to_string_pretty(&report).unwrap());
    }

    let mut normalized = normalize_short_sha_values(output);
    for line in output.lines() {
        if line.starts_with("Analyzed: ")
            && let Some(elapsed_start) = line.rfind(" | ")
        {
            normalized = normalized.replace(&line[elapsed_start..], " | <elapsed> ms");
        }
    }
    normalize_line_endings(&normalized)
}

// Replace local workspace identity and commit hashes throughout the JSON value.
fn normalize_json(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(object) => {
            if let Some(serde_json::Value::Object(workspace)) = object.get_mut("workspace")
                && let Some(id) = workspace.get_mut("id")
            {
                *id = serde_json::Value::String("<workspace-id>".into());
            }
            for child in object.values_mut() {
                normalize_json(child);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                normalize_json(item);
            }
        }
        serde_json::Value::String(text) => *text = normalize_sha_values(text),
        _ => {}
    }
}

// Replace any full hexadecimal commit ID while preserving surrounding report text.
fn normalize_sha_values(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut result = String::with_capacity(text.len());
    let mut cursor = 0;
    for (index, _) in text.char_indices() {
        if index >= cursor
            && index + 40 <= bytes.len()
            && bytes[index..index + 40].iter().all(u8::is_ascii_hexdigit)
        {
            result.push_str(&text[cursor..index]);
            result.push_str("<SHA>");
            cursor = index + 40;
        }
    }
    result.push_str(&text[cursor..]);
    result
}

// Replace seven-character commit abbreviations printed in terminal and Markdown output.
fn normalize_short_sha_values(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut cursor = 0;
    let mut index = 0;
    while index < text.len() {
        let Some(character) = text[index..].chars().next() else {
            break;
        };
        if character.is_ascii_hexdigit()
            && (index == 0
                || !text[..index]
                    .chars()
                    .next_back()
                    .is_some_and(|previous| previous.is_ascii_hexdigit()))
        {
            let start = index;
            while index < text.len()
                && text[index..]
                    .chars()
                    .next()
                    .is_some_and(|next| next.is_ascii_hexdigit())
            {
                index += 1;
            }
            if index - start == 7 {
                result.push_str(&text[cursor..start]);
                result.push_str("<SHA>");
                cursor = index;
            }
        } else {
            index += character.len_utf8();
        }
    }
    result.push_str(&text[cursor..]);
    result
}

// Compare snapshots consistently when the checkout uses a different newline convention.
fn normalize_line_endings(text: &str) -> String {
    text.replace("\r\n", "\n")
}
