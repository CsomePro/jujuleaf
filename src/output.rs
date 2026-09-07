use std::fmt::Write as _;
use std::io::IsTerminal;

use anstyle::{AnsiColor, Style};
use anyhow::Result;
use serde::Serialize;
use serde_json::{Map, Value, json};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OutputMode {
    Human { color: bool },
    RawJson,
    PrettyJson,
}

impl OutputMode {
    pub(crate) fn from_flags(raw: bool, pretty: bool, no_color: bool) -> Self {
        if raw {
            Self::RawJson
        } else if pretty {
            Self::PrettyJson
        } else {
            Self::Human {
                color: terminal_color_enabled(no_color, std::io::stdout().is_terminal()),
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tone {
    Plain,
    Bold,
    Dim,
    Red,
    RedBold,
    Green,
    GreenBold,
    Yellow,
    YellowBold,
    Cyan,
    CyanBold,
    Magenta,
}

fn terminal_color_enabled(no_color: bool, is_terminal: bool) -> bool {
    !no_color
        && is_terminal
        && std::env::var_os("NO_COLOR").is_none()
        && std::env::var("TERM").map_or(true, |term| term != "dumb")
}

fn tone_style(tone: Tone) -> Style {
    let color = |color| Style::new().fg_color(Some(color));
    match tone {
        Tone::Plain => Style::new(),
        Tone::Bold => Style::new().bold(),
        Tone::Dim => Style::new().dimmed(),
        Tone::Red => color(AnsiColor::Red.into()),
        Tone::RedBold => color(AnsiColor::Red.into()).bold(),
        Tone::Green => color(AnsiColor::Green.into()),
        Tone::GreenBold => color(AnsiColor::Green.into()).bold(),
        Tone::Yellow => color(AnsiColor::Yellow.into()),
        Tone::YellowBold => color(AnsiColor::Yellow.into()).bold(),
        Tone::Cyan => color(AnsiColor::Cyan.into()),
        Tone::CyanBold => color(AnsiColor::Cyan.into()).bold(),
        Tone::Magenta => color(AnsiColor::Magenta.into()),
    }
}

fn paint(text: &str, tone: Tone, color: bool) -> String {
    if !color || tone == Tone::Plain {
        return text.to_owned();
    }
    let style = tone_style(tone);
    format!("{style}{text}{style:#}")
}

fn human_label(key: &str) -> String {
    let key = key.trim_start_matches('_');
    let mut label = String::new();
    let mut previous_was_lowercase = false;
    for character in key.chars() {
        if character == '_' || character == '-' {
            if !label.ends_with(' ') {
                label.push(' ');
            }
            previous_was_lowercase = false;
        } else {
            if character.is_uppercase() && previous_was_lowercase {
                label.push(' ');
            }
            label.extend(character.to_uppercase());
            previous_was_lowercase = character.is_lowercase() || character.is_ascii_digit();
        }
    }
    label
}

fn human_scalar(value: &Value) -> String {
    match value {
        Value::Null => "-".to_owned(),
        Value::Bool(true) => "yes".to_owned(),
        Value::Bool(false) => "no".to_owned(),
        Value::Number(number) => number.to_string(),
        Value::String(string) => string.clone(),
        Value::Array(_) | Value::Object(_) => {
            serde_json::to_string(value).unwrap_or_else(|_| "-".to_owned())
        }
    }
}

fn normalized_key(key: &str) -> String {
    key.chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn human_table_scalar(key: &str, value: &Value) -> String {
    let text = human_scalar(value);
    let key = normalized_key(key);
    if matches!(key.as_str(), "operationid" | "commitid" | "changeid")
        && text.len() > 12
        && text.is_ascii()
    {
        format!("{}…", &text[..12])
    } else {
        text
    }
}

fn key_contains(key: &str, needles: &[&str]) -> bool {
    let key = normalized_key(key);
    needles.iter().any(|needle| key.contains(needle))
}

fn field_rank(key: &str) -> (u8, String) {
    let normalized = normalized_key(key);
    let rank = match normalized.as_str() {
        "success" => 0,
        "projectid" | "id" => 1,
        "name" | "description" => 2,
        "path" | "profile" | "baseurl" => 3,
        "phase" | "status" | "type" | "severity" => 4,
        _ if normalized.ends_with("count") => 8,
        _ if normalized.ends_with("id") => 6,
        _ => 5,
    };
    (rank, normalized)
}

fn scalar_tone(key: &str, value: &Value) -> Tone {
    if value.is_null() {
        return Tone::Dim;
    }
    if let Some(boolean) = value.as_bool() {
        return if key_contains(key, &["success", "confirmed", "ready", "active"]) {
            if boolean {
                Tone::GreenBold
            } else {
                Tone::RedBold
            }
        } else if boolean {
            Tone::Green
        } else {
            Tone::Dim
        };
    }
    if value.is_number() {
        return Tone::Yellow;
    }
    let text = value.as_str().unwrap_or_default().to_ascii_lowercase();
    if key_contains(key, &["error", "conflict", "missing", "failed", "deleted"])
        || matches!(text.as_str(), "error" | "failed" | "conflict")
    {
        Tone::Red
    } else if key_contains(key, &["warning", "pending", "unknown", "unsubmitted"])
        || matches!(
            text.as_str(),
            "warning" | "pending" | "unknown" | "submitting"
        )
    {
        Tone::Yellow
    } else if text == "skipped" {
        Tone::Dim
    } else if key_contains(key, &["operationid"]) || normalized_key(key).ends_with("commitid") {
        Tone::Magenta
    } else if normalized_key(key).ends_with("id") || key_contains(key, &["path", "file", "url"]) {
        Tone::Cyan
    } else if matches!(
        text.as_str(),
        "ok" | "success" | "confirmed" | "submitted" | "finishing" | "clean"
    ) {
        Tone::Green
    } else if text == "draft" {
        Tone::Yellow
    } else {
        Tone::Plain
    }
}

fn section_tone(key: &str) -> Tone {
    if key_contains(key, &["conflict", "missing", "deleted", "failed", "error"]) {
        Tone::RedBold
    } else if key_contains(
        key,
        &[
            "modified",
            "pending",
            "unknown",
            "warning",
            "unsubmitted",
            "localonly",
            "unresolved",
        ],
    ) {
        Tone::YellowBold
    } else if key_contains(
        key,
        &[
            "added",
            "updated",
            "pushed",
            "submitted",
            "confirmed",
            "clean",
        ],
    ) {
        Tone::GreenBold
    } else if key_contains(key, &["unchanged"]) {
        Tone::Dim
    } else {
        Tone::CyanBold
    }
}

fn section_marker(key: &str) -> &'static str {
    if key_contains(key, &["conflict", "error", "failed"]) {
        "!"
    } else if key_contains(key, &["missing", "deleted"]) {
        "D"
    } else if key_contains(key, &["modified"]) {
        "M"
    } else if key_contains(key, &["pending", "unknown", "warning", "unsubmitted"]) {
        "?"
    } else if key_contains(key, &["added"]) {
        "A"
    } else if key_contains(key, &["updated", "pushed", "submitted", "confirmed"]) {
        "+"
    } else if key_contains(key, &["clean", "unchanged"]) {
        "·"
    } else {
        "•"
    }
}

fn table_columns(rows: &[Value]) -> Option<Vec<String>> {
    let mut columns = Vec::new();
    for row in rows {
        let object = row.as_object()?;
        if object.values().any(|value| {
            value.is_array()
                || value.is_object()
                || value.as_str().is_some_and(|text| text.contains('\n'))
        }) {
            return None;
        }
        for key in object.keys() {
            if !columns.contains(key) {
                columns.push(key.clone());
            }
        }
    }
    columns.sort_by_key(|key| field_rank(key));
    (!columns.is_empty() && columns.len() <= 8).then_some(columns)
}

fn write_padded_cell(
    output: &mut String,
    text: &str,
    width: usize,
    right_aligned: bool,
    tone: Tone,
    color: bool,
) {
    let padding = width.saturating_sub(text.chars().count());
    if right_aligned {
        output.push_str(&" ".repeat(padding));
    }
    output.push_str(&paint(text, tone, color));
    if !right_aligned {
        output.push_str(&" ".repeat(padding));
    }
}

fn write_table(output: &mut String, rows: &[Value], indent: usize, color: bool) {
    let Some(columns) = table_columns(rows) else {
        for (index, row) in rows.iter().enumerate() {
            let marker = format!("{}.", index + 1);
            let _ = writeln!(
                output,
                "{}{}",
                " ".repeat(indent),
                paint(&marker, Tone::Cyan, color)
            );
            write_human_value(output, row, indent + 2, color, false);
        }
        return;
    };
    let headers: Vec<_> = columns.iter().map(|column| human_label(column)).collect();
    let cells: Vec<Vec<_>> = rows
        .iter()
        .map(|row| {
            let object = row.as_object().expect("table rows are objects");
            columns
                .iter()
                .map(|column| {
                    object
                        .get(column)
                        .map(|value| human_table_scalar(column, value))
                        .unwrap_or_default()
                })
                .collect()
        })
        .collect();
    let widths: Vec<_> = (0..columns.len())
        .map(|index| {
            cells
                .iter()
                .map(|row| row[index].chars().count())
                .chain(std::iter::once(headers[index].chars().count()))
                .max()
                .unwrap_or_default()
        })
        .collect();
    let numeric: Vec<_> = columns
        .iter()
        .map(|column| {
            rows.iter().all(|row| {
                row.as_object()
                    .and_then(|object| object.get(column))
                    .is_some_and(Value::is_number)
            })
        })
        .collect();

    output.push_str(&" ".repeat(indent));
    for (index, header) in headers.iter().enumerate() {
        write_padded_cell(
            output,
            header,
            widths[index],
            numeric[index],
            Tone::Bold,
            color,
        );
        if index + 1 < headers.len() {
            output.push_str("  ");
        }
    }
    output.push('\n');
    output.push_str(&" ".repeat(indent));
    for (index, width) in widths.iter().enumerate() {
        let separator = "─".repeat(*width);
        write_padded_cell(output, &separator, *width, false, Tone::Dim, color);
        if index + 1 < widths.len() {
            output.push_str("  ");
        }
    }
    output.push('\n');
    for (row_index, row) in cells.iter().enumerate() {
        let object = rows[row_index].as_object().expect("table rows are objects");
        output.push_str(&" ".repeat(indent));
        for (column_index, cell) in row.iter().enumerate() {
            let value = object.get(&columns[column_index]).unwrap_or(&Value::Null);
            write_padded_cell(
                output,
                cell,
                widths[column_index],
                numeric[column_index],
                scalar_tone(&columns[column_index], value),
                color,
            );
            if column_index + 1 < row.len() {
                output.push_str("  ");
            }
        }
        output.push('\n');
    }
}

fn write_multiline(output: &mut String, key: &str, text: &str, indent: usize, color: bool) {
    let _ = writeln!(
        output,
        "{}{}",
        " ".repeat(indent),
        paint(&human_label(key), Tone::Bold, color)
    );
    for line in text.lines() {
        let _ = writeln!(
            output,
            "{}{} {}",
            " ".repeat(indent + 2),
            paint("│", Tone::Dim, color),
            line
        );
    }
}

fn write_scalar_list(output: &mut String, key: &str, values: &[Value], indent: usize, color: bool) {
    let tone = section_tone(key);
    let marker = section_marker(key);
    for value in values {
        let text = human_scalar(value);
        let _ = writeln!(
            output,
            "{}{} {}",
            " ".repeat(indent),
            paint(marker, tone, color),
            paint(&text, tone, color)
        );
    }
}

fn structured_value_is_empty(value: &Value) -> bool {
    matches!(value, Value::Array(values) if values.is_empty())
        || matches!(value, Value::Object(object) if object.is_empty())
}

fn write_human_object(
    output: &mut String,
    object: &Map<String, Value>,
    indent: usize,
    color: bool,
    top_level: bool,
) {
    let initial_len = output.len();
    if top_level && let Some(success) = object.get("success").and_then(Value::as_bool) {
        let (marker, label, tone) = if success {
            ("✓", "SUCCESS", Tone::GreenBold)
        } else {
            ("×", "FAILED", Tone::RedBold)
        };
        let _ = writeln!(
            output,
            "{}{} {}",
            " ".repeat(indent),
            paint(marker, tone, color),
            paint(label, tone, color)
        );
    }

    let mut scalars: Vec<_> = object
        .iter()
        .filter(|(key, value)| {
            !(value.is_array() || value.is_object() || top_level && key.as_str() == "success")
        })
        .collect();
    scalars.sort_by_key(|(key, _)| field_rank(key));
    let label_width = scalars
        .iter()
        .filter(|(_, value)| !value.as_str().is_some_and(|text| text.contains('\n')))
        .map(|(key, _)| human_label(key).chars().count())
        .max()
        .unwrap_or_default();
    for (key, value) in scalars {
        if let Some(text) = value.as_str().filter(|text| text.contains('\n')) {
            write_multiline(output, key, text, indent, color);
            continue;
        }
        let label = human_label(key);
        let padding = label_width.saturating_sub(label.chars().count());
        let _ = writeln!(
            output,
            "{}{}{}  {}",
            " ".repeat(indent),
            paint(&label, Tone::Dim, color),
            " ".repeat(padding),
            paint(&human_scalar(value), scalar_tone(key, value), color)
        );
    }

    let mut structured: Vec<_> = object
        .iter()
        .filter(|(_, value)| {
            (value.is_array() || value.is_object()) && !structured_value_is_empty(value)
        })
        .collect();
    structured.sort_by_key(|(key, _)| field_rank(key));
    for (key, value) in structured {
        if output.len() > initial_len && !output.ends_with("\n\n") {
            output.push('\n');
        }
        let count = value.as_array().map(Vec::len);
        let heading = count.map_or_else(
            || human_label(key),
            |count| {
                format!(
                    "{}  {}",
                    human_label(key),
                    paint(&count.to_string(), Tone::Dim, color)
                )
            },
        );
        let _ = writeln!(
            output,
            "{}{}",
            " ".repeat(indent),
            paint(&heading, section_tone(key), color)
        );
        match value {
            Value::Array(values)
                if values
                    .iter()
                    .all(|value| !value.is_array() && !value.is_object()) =>
            {
                write_scalar_list(output, key, values, indent + 2, color);
            }
            Value::Array(values) => write_table(output, values, indent + 2, color),
            Value::Object(_) => write_human_value(output, value, indent + 2, color, false),
            _ => unreachable!(),
        }
    }

    if output.len() == initial_len {
        let _ = writeln!(
            output,
            "{}{}",
            " ".repeat(indent),
            paint("(no data)", Tone::Dim, color)
        );
    }
}

fn write_human_value(
    output: &mut String,
    value: &Value,
    indent: usize,
    color: bool,
    top_level: bool,
) {
    match value {
        Value::Object(object) => write_human_object(output, object, indent, color, top_level),
        Value::Array(values) if values.is_empty() => {
            let _ = writeln!(
                output,
                "{}{}",
                " ".repeat(indent),
                paint("(none)", Tone::Dim, color)
            );
        }
        Value::Array(values)
            if values
                .iter()
                .all(|value| !value.is_array() && !value.is_object()) =>
        {
            write_scalar_list(output, "items", values, indent, color);
        }
        Value::Array(values) => write_table(output, values, indent, color),
        _ => {
            let _ = writeln!(output, "{}{}", " ".repeat(indent), human_scalar(value));
        }
    }
}

pub(crate) fn render_human(value: &Value, color: bool) -> String {
    let mut rendered = String::new();
    write_human_value(&mut rendered, value, 0, color, true);
    rendered.trim_end().to_owned()
}

pub(crate) fn output(value: impl Serialize, mode: OutputMode) -> Result<()> {
    let value = serde_json::to_value(value)?;
    match mode {
        OutputMode::Human { color } => println!("{}", render_human(&value, color)),
        OutputMode::RawJson => println!("{}", serde_json::to_string(&value)?),
        OutputMode::PrettyJson => println!("{}", serde_json::to_string_pretty(&value)?),
    }
    Ok(())
}

pub(crate) fn notice(message: &str, mode: OutputMode) {
    let color = matches!(mode, OutputMode::Human { color: true });
    eprintln!("{} {}", paint("◆", Tone::CyanBold, color), message);
}

fn process_has_flag(flag: &str) -> bool {
    std::env::args_os().any(|argument| argument == flag)
}

pub fn print_error(error: &anyhow::Error) {
    let value = json!({"error": format!("{error:#}")});
    if process_has_flag("--raw") {
        eprintln!(
            "{}",
            serde_json::to_string(&value).expect("error JSON is serializable")
        );
    } else if process_has_flag("--pretty") {
        eprintln!(
            "{}",
            serde_json::to_string_pretty(&value).expect("error JSON is serializable")
        );
    } else {
        let color = terminal_color_enabled(
            process_has_flag("--no-color"),
            std::io::stderr().is_terminal(),
        );
        eprintln!(
            "{} {}",
            paint("×", Tone::RedBold, color),
            paint(&format!("Error: {error:#}"), Tone::RedBold, color)
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_renderer_uses_status_sections_and_tables() {
        let rendered = render_human(
            &json!({
                "success": true,
                "projectId": "project-1",
                "modified": ["/main.tex"],
                "projects": [
                    {"_id": "p1", "accessLevel": "owner", "name": "Paper One"},
                    {"_id": "p2", "accessLevel": "readWrite", "name": "Paper Two"}
                ],
                "unchanged": []
            }),
            false,
        );
        assert!(rendered.starts_with("✓ SUCCESS"));
        assert!(rendered.contains("PROJECT ID  project-1"));
        assert!(rendered.contains("MODIFIED  1"));
        assert!(rendered.contains("M /main.tex"));
        assert!(rendered.contains("PROJECTS  2"));
        assert!(rendered.contains("Paper One"));
        assert!(!rendered.contains("UNCHANGED"));
        assert!(!rendered.contains('{'));
    }

    #[test]
    fn history_tables_abbreviate_jj_ids_without_touching_project_ids() {
        let operation_id = "0123456789abcdef0123456789abcdef";
        let rendered = render_human(
            &json!({
                "projectId": "project-identifier-must-remain-complete",
                "entries": [{
                    "operationId": operation_id,
                    "commitId": "abcdef0123456789abcdef0123456789",
                    "description": "checkpoint"
                }]
            }),
            false,
        );
        assert!(rendered.contains("0123456789ab…"));
        assert!(rendered.contains("abcdef012345…"));
        assert!(!rendered.contains(operation_id));
        assert!(rendered.contains("project-identifier-must-remain-complete"));
    }

    #[test]
    fn color_is_opt_in_to_the_renderer_and_never_changes_layout() {
        let value = json!({"success": false, "path": "/main.tex", "warnings": ["check this"]});
        let plain = render_human(&value, false);
        let colored = render_human(&value, true);
        assert!(!plain.contains("\u{1b}["));
        assert!(colored.contains("\u{1b}["));
        assert_eq!(plain, strip_ansi(&colored));
    }

    fn strip_ansi(value: &str) -> String {
        let mut result = String::new();
        let mut characters = value.chars().peekable();
        while let Some(character) = characters.next() {
            if character == '\u{1b}' && characters.peek() == Some(&'[') {
                characters.next();
                for next in characters.by_ref() {
                    if next.is_ascii_alphabetic() {
                        break;
                    }
                }
            } else {
                result.push(character);
            }
        }
        result
    }

    #[test]
    fn no_color_always_disables_terminal_color() {
        assert!(!terminal_color_enabled(true, true));
        assert!(!terminal_color_enabled(false, false));
    }
}
