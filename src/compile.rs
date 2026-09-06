use regex::Regex;
use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompileDiagnostic {
    pub severity: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompileReport {
    pub success: bool,
    pub status: String,
    pub diagnostics: Vec<CompileDiagnostic>,
    pub output_files: Value,
    pub validation_problems: Value,
    pub timings: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub log: Option<String>,
}

pub fn has_output(result: &Value, path: &str) -> bool {
    result
        .get("outputFiles")
        .and_then(Value::as_array)
        .is_some_and(|files| {
            files
                .iter()
                .any(|file| file.get("path").and_then(Value::as_str) == Some(path))
        })
}

pub fn parse_compile_log(log: &str) -> Vec<CompileDiagnostic> {
    let file_line = Regex::new(r"^(.+?):(\d+):\s*(.+)$").expect("valid compile-log regex");
    let latex_warning =
        Regex::new(r"^(?:LaTeX|Package .+?) Warning:\s*(.+?)(?: on input line (\d+)\.)?$")
            .expect("valid warning regex");
    let latex_line = Regex::new(r"^l\.(\d+)\s*(.*)$").expect("valid TeX line regex");
    let lines: Vec<_> = log.lines().collect();
    let mut diagnostics = Vec::new();
    let mut index = 0;

    while index < lines.len() {
        let line = lines[index].trim();
        if let Some(captures) = file_line.captures(line) {
            diagnostics.push(CompileDiagnostic {
                severity: if captures[3].contains("Warning") {
                    "warning".into()
                } else {
                    "error".into()
                },
                file: Some(captures[1].trim_start_matches("./").to_owned()),
                line: captures[2].parse().ok(),
                message: captures[3].trim().to_owned(),
            });
        } else if let Some(message) = line.strip_prefix("! ") {
            let mut line_number = None;
            let mut detail = message.trim().to_owned();
            if let Some(next) = lines.get(index + 1).map(|line| line.trim())
                && let Some(captures) = latex_line.captures(next)
            {
                line_number = captures[1].parse().ok();
                if !captures[2].trim().is_empty() {
                    detail.push_str(": ");
                    detail.push_str(captures[2].trim());
                }
                index += 1;
            }
            diagnostics.push(CompileDiagnostic {
                severity: "error".into(),
                file: None,
                line: line_number,
                message: detail,
            });
        } else if let Some(captures) = latex_warning.captures(line) {
            diagnostics.push(CompileDiagnostic {
                severity: "warning".into(),
                file: None,
                line: captures
                    .get(2)
                    .and_then(|value| value.as_str().parse().ok()),
                message: captures[1].trim().to_owned(),
            });
        }
        index += 1;
    }

    diagnostics.dedup();
    diagnostics
}

pub fn build_compile_report(
    result: Value,
    log: Option<String>,
    include_log: bool,
) -> CompileReport {
    let status = result
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_owned();
    let diagnostics = log.as_deref().map(parse_compile_log).unwrap_or_default();
    CompileReport {
        success: status == "success",
        status,
        diagnostics,
        output_files: result.get("outputFiles").cloned().unwrap_or(Value::Null),
        validation_problems: result
            .get("validationProblems")
            .cloned()
            .unwrap_or(Value::Null),
        timings: result.get("timings").cloned().unwrap_or(Value::Null),
        log: include_log.then(|| log.unwrap_or_default()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_file_line_errors_bang_errors_and_warnings() {
        let log = r#"./chapters/one.tex:12: Undefined control sequence.
! Missing $ inserted.
l.27 bad_math
LaTeX Warning: Label `x' multiply defined on input line 42."#;
        let diagnostics = parse_compile_log(log);
        assert_eq!(diagnostics.len(), 3);
        assert_eq!(diagnostics[0].file.as_deref(), Some("chapters/one.tex"));
        assert_eq!(diagnostics[0].line, Some(12));
        assert_eq!(diagnostics[1].line, Some(27));
        assert_eq!(diagnostics[2].severity, "warning");
        assert_eq!(diagnostics[2].line, Some(42));
    }

    #[test]
    fn report_keeps_raw_log_optional() {
        let report = build_compile_report(
            json!({"status":"failure","outputFiles":[],"validationProblems":[]}),
            Some("! Broken".into()),
            false,
        );
        assert!(!report.success);
        assert_eq!(report.diagnostics.len(), 1);
        assert!(report.log.is_none());
    }
}
