//! Conservative LaTeX block scanner for human-review grouping.
//!
//! Blocks always cover the input exactly once. Recognized structures receive
//! typed boundaries; unknown or malformed structures remain explicit so the
//! review planner can fall back instead of guessing.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LatexBlockKind {
    Prose,
    Command,
    ListItem,
    FigurePart,
    Table,
    Math,
    Code,
    Drawing,
    Opaque,
    Unknown,
    Whitespace,
}

impl LatexBlockKind {
    pub(crate) fn is_structured(self) -> bool {
        !matches!(self, Self::Prose | Self::Whitespace | Self::Unknown)
    }

    pub(crate) fn is_unknown(self) -> bool {
        self == Self::Unknown
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LatexBlock {
    pub(crate) byte_from: usize,
    pub(crate) byte_to: usize,
    pub(crate) kind: LatexBlockKind,
}

#[derive(Debug, Clone)]
pub(crate) struct LatexBlockScan {
    pub(crate) blocks: Vec<LatexBlock>,
    pub(crate) signature: Vec<String>,
    pub(crate) valid: bool,
}

#[derive(Debug, Clone)]
struct TexCommand {
    start: usize,
    end: usize,
    name: String,
    environment: Option<String>,
}

#[derive(Debug, Clone, Copy)]
struct EnvironmentEnd {
    start: usize,
    end: usize,
}

#[derive(Debug, Clone)]
enum TopLevelEvent {
    Command { start: usize, name: String },
    Environment { start: usize, name: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EnvironmentClass {
    List,
    Figure,
    Table,
    Math,
    Code,
    Drawing,
    Opaque,
    Text,
    Unknown,
}

pub(crate) fn scan_latex_blocks(text: &str) -> LatexBlockScan {
    LatexBlockScanner::new(text).scan()
}

struct LatexBlockScanner<'a> {
    text: &'a str,
    blocks: Vec<LatexBlock>,
    signature: Vec<String>,
    valid: bool,
}

impl<'a> LatexBlockScanner<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            text,
            blocks: Vec::new(),
            signature: Vec::new(),
            valid: true,
        }
    }

    fn scan(mut self) -> LatexBlockScan {
        self.scan_range(0, self.text.len());
        self.blocks = collapse_whitespace(self.blocks);
        debug_assert!(blocks_cover_text(&self.blocks, self.text.len()));
        LatexBlockScan {
            blocks: self.blocks,
            signature: self.signature,
            valid: self.valid,
        }
    }

    fn scan_range(&mut self, start: usize, end: usize) {
        let mut cursor = start;
        while cursor < end {
            let Some(begin) = self.next_top_level_begin(cursor, end) else {
                self.push_plain_range(cursor, end);
                break;
            };
            let environment = begin
                .environment
                .as_deref()
                .expect("begin commands carry an environment");
            let environment_from = line_wrapped_start(self.text, cursor, begin.start);
            if cursor < environment_from {
                self.push_plain_range(cursor, environment_from);
            }

            let Some(closing) = self.find_matching_environment(&begin, end) else {
                self.valid = false;
                self.blocks.push(LatexBlock {
                    byte_from: environment_from,
                    byte_to: end,
                    kind: LatexBlockKind::Unknown,
                });
                break;
            };
            let environment_to = line_wrapped_end(self.text, closing.end, end);
            match environment_class(environment) {
                EnvironmentClass::List => self.push_list_environment(
                    environment_from,
                    begin.end,
                    closing.start,
                    environment_to,
                    environment,
                ),
                EnvironmentClass::Figure => self.push_figure_environment(
                    environment_from,
                    begin.end,
                    closing.start,
                    environment_to,
                    environment,
                ),
                EnvironmentClass::Table => self.push_atomic_environment(
                    environment_from,
                    environment_to,
                    LatexBlockKind::Table,
                    "table",
                    environment,
                ),
                EnvironmentClass::Math => self.push_atomic_environment(
                    environment_from,
                    environment_to,
                    LatexBlockKind::Math,
                    "math",
                    environment,
                ),
                EnvironmentClass::Code => self.push_atomic_environment(
                    environment_from,
                    environment_to,
                    LatexBlockKind::Code,
                    "code",
                    environment,
                ),
                EnvironmentClass::Drawing => self.push_atomic_environment(
                    environment_from,
                    environment_to,
                    LatexBlockKind::Drawing,
                    "drawing",
                    environment,
                ),
                EnvironmentClass::Opaque => self.push_atomic_environment(
                    environment_from,
                    environment_to,
                    LatexBlockKind::Opaque,
                    "opaque",
                    environment,
                ),
                EnvironmentClass::Text => {
                    self.signature.push(format!("text:{environment}"));
                    let block_start = self.blocks.len();
                    self.scan_range(begin.end, closing.start);
                    self.attach_environment_wrappers(block_start, environment_from, environment_to);
                }
                EnvironmentClass::Unknown => {
                    self.signature.push(format!("unknown:{environment}"));
                    self.blocks.push(LatexBlock {
                        byte_from: environment_from,
                        byte_to: environment_to,
                        kind: LatexBlockKind::Unknown,
                    });
                }
            }
            cursor = environment_to;
        }
    }

    fn push_atomic_environment(
        &mut self,
        byte_from: usize,
        byte_to: usize,
        kind: LatexBlockKind,
        family: &str,
        environment: &str,
    ) {
        self.signature.push(format!("{family}:{environment}"));
        self.blocks.push(LatexBlock {
            byte_from,
            byte_to,
            kind,
        });
    }

    fn attach_environment_wrappers(
        &mut self,
        block_start: usize,
        byte_from: usize,
        byte_to: usize,
    ) {
        let segment = self.blocks.split_off(block_start);
        let first = segment
            .iter()
            .position(|block| block.kind != LatexBlockKind::Whitespace);
        let last = segment
            .iter()
            .rposition(|block| block.kind != LatexBlockKind::Whitespace);
        let (Some(first), Some(last)) = (first, last) else {
            self.blocks.push(LatexBlock {
                byte_from,
                byte_to,
                kind: LatexBlockKind::Command,
            });
            return;
        };
        let mut meaningful = segment[first..=last].to_vec();
        meaningful[0].byte_from = byte_from;
        meaningful.last_mut().expect("non-empty segment").byte_to = byte_to;
        self.blocks.extend(meaningful);
    }

    fn push_list_environment(
        &mut self,
        byte_from: usize,
        content_from: usize,
        content_to: usize,
        byte_to: usize,
        environment: &str,
    ) {
        self.signature.push(format!("list:{environment}"));
        let items: Vec<_> = self
            .top_level_events(content_from, content_to)
            .into_iter()
            .filter_map(|event| match event {
                TopLevelEvent::Command { start, name }
                    if normalized_command_name(&name) == "item" =>
                {
                    Some(line_wrapped_start(self.text, content_from, start))
                }
                _ => None,
            })
            .collect();
        if items.is_empty() {
            self.blocks.push(LatexBlock {
                byte_from,
                byte_to,
                kind: LatexBlockKind::ListItem,
            });
            return;
        }

        for (index, item) in items.iter().enumerate() {
            self.signature.push("list:item".to_owned());
            self.blocks.push(LatexBlock {
                byte_from: if index == 0 { byte_from } else { *item },
                byte_to: items.get(index + 1).copied().unwrap_or(byte_to),
                kind: LatexBlockKind::ListItem,
            });
        }
    }

    fn push_figure_environment(
        &mut self,
        byte_from: usize,
        content_from: usize,
        content_to: usize,
        byte_to: usize,
        environment: &str,
    ) {
        self.signature.push(format!("figure:{environment}"));
        let mut components = Vec::new();
        for event in self.top_level_events(content_from, content_to) {
            match event {
                TopLevelEvent::Command { start, name } => {
                    let normalized = normalized_command_name(&name);
                    if matches!(
                        normalized,
                        "includegraphics" | "caption" | "captionof" | "subcaptionbox" | "subfloat"
                    ) {
                        self.signature.push(format!("figure:{normalized}"));
                        components.push(line_wrapped_start(self.text, content_from, start));
                    } else if normalized == "label" {
                        self.signature.push("figure:label".to_owned());
                    }
                }
                TopLevelEvent::Environment { start, name }
                    if environment_class(&name) == EnvironmentClass::Figure =>
                {
                    self.signature.push(format!("figure:{name}"));
                    components.push(line_wrapped_start(self.text, content_from, start));
                }
                TopLevelEvent::Environment { .. } => {}
            }
        }
        components.sort_unstable();
        components.dedup();

        if components.is_empty() {
            self.blocks.push(LatexBlock {
                byte_from,
                byte_to,
                kind: LatexBlockKind::FigurePart,
            });
            return;
        }
        for (index, component) in components.iter().enumerate() {
            self.blocks.push(LatexBlock {
                byte_from: if index == 0 { byte_from } else { *component },
                byte_to: components.get(index + 1).copied().unwrap_or(byte_to),
                kind: LatexBlockKind::FigurePart,
            });
        }
    }

    fn push_plain_range(&mut self, start: usize, end: usize) {
        let mut cursor = start;
        while cursor < end {
            let line_end = self.text[cursor..end]
                .find('\n')
                .map_or(end, |relative| cursor + relative + 1);
            let line = &self.text[cursor..line_end];
            let trimmed = line.trim_start();
            if trimmed.trim().is_empty() {
                self.blocks.push(LatexBlock {
                    byte_from: cursor,
                    byte_to: line_end,
                    kind: LatexBlockKind::Whitespace,
                });
                cursor = line_end;
                continue;
            }

            if let Some((delimiter, closing)) = display_math_delimiters(trimmed) {
                let math_start = cursor + (line.len() - trimmed.len());
                let search_from = math_start + delimiter.len();
                let Some(relative_end) = self.text[search_from..end].find(closing) else {
                    self.valid = false;
                    self.blocks.push(LatexBlock {
                        byte_from: cursor,
                        byte_to: end,
                        kind: LatexBlockKind::Unknown,
                    });
                    return;
                };
                let closing_end = search_from + relative_end + closing.len();
                let math_end = line_wrapped_end(self.text, closing_end, end);
                self.signature.push("math:display".to_owned());
                self.blocks.push(LatexBlock {
                    byte_from: cursor,
                    byte_to: math_end,
                    kind: LatexBlockKind::Math,
                });
                cursor = math_end;
                continue;
            }

            let first_non_whitespace = cursor + (line.len() - trimmed.len());
            if trimmed.starts_with('%') {
                self.blocks.push(LatexBlock {
                    byte_from: cursor,
                    byte_to: line_end,
                    kind: LatexBlockKind::Unknown,
                });
            } else if let Some(command) = command_at(self.text, first_non_whitespace) {
                let safe = is_safe_standalone_command(&command.name);
                if safe && is_structural_command(&command.name) {
                    self.signature.push(format!(
                        "command:{}",
                        normalized_command_name(&command.name)
                    ));
                }
                self.blocks.push(LatexBlock {
                    byte_from: cursor,
                    byte_to: line_end,
                    kind: if safe {
                        LatexBlockKind::Command
                    } else {
                        LatexBlockKind::Unknown
                    },
                });
            } else {
                self.push_sentence_units(cursor, line_end);
            }
            cursor = line_end;
        }
    }

    fn push_sentence_units(&mut self, start: usize, end: usize) {
        let line = &self.text[start..end];
        let mut unit_start = 0usize;
        let mut cursor = 0usize;
        let mut brace_depth = 0usize;
        let mut inline_math = false;

        while cursor < line.len() {
            let character = line[cursor..].chars().next().expect("valid character");
            let width = character.len_utf8();
            if character == '\\' {
                if let Some(command) = command_at(self.text, start + cursor) {
                    match command.name.as_str() {
                        "[" | "(" => inline_math = true,
                        "]" | ")" => inline_math = false,
                        _ => {}
                    }
                    cursor = (command.end - start).min(line.len());
                    continue;
                }
            } else if character == '%' {
                break;
            } else if character == '$' {
                let delimiter_width = if line[cursor + width..].starts_with('$') {
                    width * 2
                } else {
                    width
                };
                inline_math = !inline_math;
                cursor += delimiter_width;
                continue;
            } else if !inline_math {
                if character == '{' {
                    brace_depth += 1;
                } else if character == '}' {
                    brace_depth = brace_depth.saturating_sub(1);
                } else if brace_depth == 0 && sentence_boundary_at(line, cursor, character) {
                    let mut unit_end = cursor + width;
                    while unit_end < line.len() {
                        let next = line[unit_end..].chars().next().expect("valid character");
                        if !next.is_whitespace() {
                            break;
                        }
                        unit_end += next.len_utf8();
                    }
                    self.blocks.push(LatexBlock {
                        byte_from: start + unit_start,
                        byte_to: start + unit_end,
                        kind: LatexBlockKind::Prose,
                    });
                    unit_start = unit_end;
                    cursor = unit_end;
                    continue;
                }
            }
            cursor += width;
        }

        if unit_start < line.len() {
            self.blocks.push(LatexBlock {
                byte_from: start + unit_start,
                byte_to: end,
                kind: LatexBlockKind::Prose,
            });
        }
    }

    fn next_top_level_begin(&self, start: usize, end: usize) -> Option<TexCommand> {
        let mut cursor = start;
        let mut brace_depth = 0usize;
        let mut math = false;
        while cursor < end {
            match self.text.as_bytes()[cursor] {
                b'\\' => {
                    if let Some(command) = command_at(self.text, cursor) {
                        if brace_depth == 0
                            && !math
                            && command.name == "begin"
                            && command.environment.is_some()
                        {
                            return Some(command);
                        }
                        match command.name.as_str() {
                            "[" | "(" => math = true,
                            "]" | ")" => math = false,
                            _ => {}
                        }
                        cursor = command.end;
                    } else {
                        cursor += 1;
                    }
                }
                b'%' => cursor = next_line(self.text, cursor, end),
                b'{' if !math => {
                    brace_depth += 1;
                    cursor += 1;
                }
                b'}' if !math => {
                    brace_depth = brace_depth.saturating_sub(1);
                    cursor += 1;
                }
                b'$' => {
                    math = !math;
                    cursor += if self.text[cursor + 1..end].starts_with('$') {
                        2
                    } else {
                        1
                    };
                }
                _ => cursor += 1,
            }
        }
        None
    }

    fn find_matching_environment(
        &self,
        begin: &TexCommand,
        limit: usize,
    ) -> Option<EnvironmentEnd> {
        let environment = begin.environment.as_deref()?;
        if environment_class(environment) == EnvironmentClass::Code {
            return literal_environment_end(self.text, begin.end, limit, environment);
        }

        let mut stack = vec![environment.to_owned()];
        let mut cursor = begin.end;
        let mut brace_depth = 0usize;
        let mut math = false;
        while cursor < limit {
            match self.text.as_bytes()[cursor] {
                b'\\' => {
                    let Some(command) = command_at(self.text, cursor) else {
                        cursor += 1;
                        continue;
                    };
                    if brace_depth == 0 && !math {
                        if command.name == "begin" {
                            let nested = command.environment.clone()?;
                            if environment_class(&nested) == EnvironmentClass::Code {
                                let closing = literal_environment_end(
                                    self.text,
                                    command.end,
                                    limit,
                                    &nested,
                                )?;
                                cursor = closing.end;
                                continue;
                            }
                            stack.push(nested);
                        } else if command.name == "end" {
                            let closing = command.environment.as_deref()?;
                            if stack.last().is_none_or(|open| open != closing) {
                                return None;
                            }
                            stack.pop();
                            if stack.is_empty() {
                                return Some(EnvironmentEnd {
                                    start: command.start,
                                    end: command.end,
                                });
                            }
                        }
                    }
                    match command.name.as_str() {
                        "[" | "(" => math = true,
                        "]" | ")" => math = false,
                        _ => {}
                    }
                    cursor = command.end;
                }
                b'%' => cursor = next_line(self.text, cursor, limit),
                b'{' if !math => {
                    brace_depth += 1;
                    cursor += 1;
                }
                b'}' if !math => {
                    brace_depth = brace_depth.saturating_sub(1);
                    cursor += 1;
                }
                b'$' => {
                    math = !math;
                    cursor += if self.text[cursor + 1..limit].starts_with('$') {
                        2
                    } else {
                        1
                    };
                }
                _ => cursor += 1,
            }
        }
        None
    }

    fn top_level_events(&self, start: usize, end: usize) -> Vec<TopLevelEvent> {
        let mut events = Vec::new();
        let mut environments: Vec<String> = Vec::new();
        let mut cursor = start;
        let mut brace_depth = 0usize;
        let mut math = false;

        while cursor < end {
            match self.text.as_bytes()[cursor] {
                b'\\' => {
                    let Some(command) = command_at(self.text, cursor) else {
                        cursor += 1;
                        continue;
                    };
                    if brace_depth == 0 && !math {
                        if command.name == "begin" {
                            if let Some(environment) = command.environment.clone() {
                                if environments.is_empty() {
                                    events.push(TopLevelEvent::Environment {
                                        start: command.start,
                                        name: environment.clone(),
                                    });
                                }
                                if environment_class(&environment) == EnvironmentClass::Code
                                    && let Some(closing) = literal_environment_end(
                                        self.text,
                                        command.end,
                                        end,
                                        &environment,
                                    )
                                {
                                    cursor = closing.end;
                                    continue;
                                }
                                environments.push(environment);
                            }
                        } else if command.name == "end" {
                            if command
                                .environment
                                .as_ref()
                                .is_some_and(|closing| environments.last() == Some(closing))
                            {
                                environments.pop();
                            }
                        } else if environments.is_empty() {
                            events.push(TopLevelEvent::Command {
                                start: command.start,
                                name: command.name.clone(),
                            });
                        }
                    }
                    match command.name.as_str() {
                        "[" | "(" => math = true,
                        "]" | ")" => math = false,
                        _ => {}
                    }
                    cursor = command.end;
                }
                b'%' => cursor = next_line(self.text, cursor, end),
                b'{' if !math => {
                    brace_depth += 1;
                    cursor += 1;
                }
                b'}' if !math => {
                    brace_depth = brace_depth.saturating_sub(1);
                    cursor += 1;
                }
                b'$' => {
                    math = !math;
                    cursor += if self.text[cursor + 1..end].starts_with('$') {
                        2
                    } else {
                        1
                    };
                }
                _ => cursor += 1,
            }
        }
        events
    }
}

fn literal_environment_end(
    text: &str,
    start: usize,
    limit: usize,
    environment: &str,
) -> Option<EnvironmentEnd> {
    let needle = format!("\\end{{{environment}}}");
    let relative = text[start..limit].find(&needle)?;
    let byte_from = start + relative;
    Some(EnvironmentEnd {
        start: byte_from,
        end: byte_from + needle.len(),
    })
}

fn command_at(text: &str, start: usize) -> Option<TexCommand> {
    let bytes = text.as_bytes();
    if bytes.get(start) != Some(&b'\\') || start + 1 >= bytes.len() {
        return None;
    }
    let name_start = start + 1;
    let mut end = name_start;
    if is_command_letter(bytes[end]) {
        while end < bytes.len() && is_command_letter(bytes[end]) {
            end += 1;
        }
        if bytes.get(end) == Some(&b'*') {
            end += 1;
        }
    } else {
        end += 1;
    }
    let name = text[name_start..end].to_owned();
    let mut environment = None;
    let mut command_end = end;
    if matches!(name.as_str(), "begin" | "end") {
        let mut cursor = end;
        while bytes.get(cursor).is_some_and(u8::is_ascii_whitespace) {
            cursor += 1;
        }
        if bytes.get(cursor) == Some(&b'{')
            && let Some(relative) = text[cursor + 1..].find('}')
        {
            let closing = cursor + 1 + relative;
            let value = &text[cursor + 1..closing];
            if !value.is_empty()
                && value.chars().all(|character| {
                    !character.is_whitespace() && !matches!(character, '{' | '}' | '\\' | '%')
                })
            {
                environment = Some(value.to_owned());
                command_end = closing + 1;
            }
        }
    }
    Some(TexCommand {
        start,
        end: command_end,
        name,
        environment,
    })
}

fn is_command_letter(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'@'
}

fn normalized_command_name(name: &str) -> &str {
    name.strip_suffix('*').unwrap_or(name)
}

fn is_safe_standalone_command(name: &str) -> bool {
    matches!(
        normalized_command_name(name),
        "part"
            | "chapter"
            | "section"
            | "subsection"
            | "subsubsection"
            | "paragraph"
            | "subparagraph"
            | "documentclass"
            | "appendix"
            | "appendices"
            | "usepackage"
            | "RequirePackage"
            | "title"
            | "author"
            | "date"
            | "thanks"
            | "maketitle"
            | "centering"
            | "item"
            | "include"
            | "input"
            | "includegraphics"
            | "caption"
            | "label"
            | "bibliographystyle"
            | "bibliography"
            | "addbibresource"
            | "newcommand"
            | "renewcommand"
            | "providecommand"
            | "DeclareMathOperator"
            | "def"
            | "let"
            | "setlength"
            | "addtolength"
            | "newcounter"
            | "setcounter"
            | "newtheorem"
            | "pagestyle"
            | "thispagestyle"
            | "clearpage"
            | "newpage"
            | "linebreak"
            | "pagebreak"
            | "noindent"
            | "smallskip"
            | "medskip"
            | "bigskip"
            | "vspace"
            | "hspace"
    )
}

fn is_structural_command(name: &str) -> bool {
    matches!(
        normalized_command_name(name),
        "part"
            | "chapter"
            | "appendix"
            | "appendices"
            | "section"
            | "subsection"
            | "subsubsection"
            | "paragraph"
            | "subparagraph"
            | "include"
            | "input"
            | "includegraphics"
            | "caption"
            | "label"
            | "bibliography"
            | "addbibresource"
    )
}

fn environment_class(environment: &str) -> EnvironmentClass {
    match environment {
        "itemize" | "enumerate" | "description" | "compactitem" | "compactenum" | "inparaenum" => {
            EnvironmentClass::List
        }
        "figure" | "figure*" | "wrapfigure" | "subfigure" => EnvironmentClass::Figure,
        "table" | "table*" | "tabular" | "tabular*" | "tabularx" | "longtable" | "array" => {
            EnvironmentClass::Table
        }
        "equation" | "equation*" | "align" | "align*" | "alignat" | "alignat*" | "gather"
        | "gather*" | "multline" | "multline*" | "displaymath" | "math" | "eqnarray"
        | "eqnarray*" | "cases" | "matrix" | "pmatrix" | "bmatrix" | "Bmatrix" | "vmatrix"
        | "Vmatrix" | "smallmatrix" => EnvironmentClass::Math,
        "verbatim" | "verbatim*" | "Verbatim" | "Verbatim*" | "lstlisting" | "minted" | "alltt" => {
            EnvironmentClass::Code
        }
        "tikzpicture" | "pgfpicture" | "picture" | "pspicture" => EnvironmentClass::Drawing,
        "algorithm" | "algorithm*" | "algorithmic" | "algorithm2e" | "forest" => {
            EnvironmentClass::Opaque
        }
        "document" | "abstract" | "quote" | "quotation" | "verse" | "center" | "flushleft"
        | "flushright" | "minipage" | "theorem" | "lemma" | "proposition" | "corollary"
        | "definition" | "remark" | "proof" | "frame" | "multicols" => EnvironmentClass::Text,
        _ => EnvironmentClass::Unknown,
    }
}

fn display_math_delimiters(text: &str) -> Option<(&'static str, &'static str)> {
    if text.starts_with("\\[") {
        Some(("\\[", "\\]"))
    } else if text.starts_with("$$") {
        Some(("$$", "$$"))
    } else {
        None
    }
}

fn sentence_boundary_at(text: &str, index: usize, character: char) -> bool {
    match character {
        '.' => text[index + character.len_utf8()..]
            .chars()
            .next()
            .is_none_or(char::is_whitespace),
        '?' | '!' | ';' | '\u{3002}' | '\u{ff1f}' | '\u{ff01}' | '\u{ff1b}' => true,
        _ => false,
    }
}

fn next_line(text: &str, start: usize, limit: usize) -> usize {
    text[start..limit]
        .find('\n')
        .map_or(limit, |relative| start + relative + 1)
}

fn line_wrapped_start(text: &str, floor: usize, start: usize) -> usize {
    let line_start = text[floor..start]
        .rfind('\n')
        .map_or(floor, |relative| floor + relative + 1);
    if text[line_start..start].trim().is_empty() {
        line_start
    } else {
        start
    }
}

fn line_wrapped_end(text: &str, end: usize, ceiling: usize) -> usize {
    let line_end = text[end..ceiling]
        .find('\n')
        .map_or(ceiling, |relative| end + relative + 1);
    let content_end = if line_end > end && text.as_bytes()[line_end - 1] == b'\n' {
        line_end - 1
    } else {
        line_end
    };
    if text[end..content_end].trim().is_empty() {
        line_end
    } else {
        end
    }
}

fn blocks_cover_text(blocks: &[LatexBlock], text_length: usize) -> bool {
    if blocks.is_empty() {
        return text_length == 0;
    }
    let mut cursor = 0;
    for block in blocks {
        if block.byte_from != cursor || block.byte_to <= block.byte_from {
            return false;
        }
        cursor = block.byte_to;
    }
    cursor == text_length
}

fn collapse_whitespace(blocks: Vec<LatexBlock>) -> Vec<LatexBlock> {
    let mut collapsed: Vec<LatexBlock> = Vec::new();
    let mut pending_from = None;
    let mut pending_to = None;
    for mut block in blocks {
        if block.kind == LatexBlockKind::Whitespace {
            pending_from.get_or_insert(block.byte_from);
            pending_to = Some(block.byte_to);
            continue;
        }
        if let Some(byte_from) = pending_from.take() {
            block.byte_from = byte_from;
            pending_to = None;
        }
        collapsed.push(block);
    }
    if let (Some(byte_from), Some(byte_to)) = (pending_from, pending_to) {
        if let Some(last) = collapsed.last_mut() {
            last.byte_to = byte_to;
        } else {
            collapsed.push(LatexBlock {
                byte_from,
                byte_to,
                kind: LatexBlockKind::Prose,
            });
        }
    }
    collapsed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts<'a>(source: &'a str, scan: &LatexBlockScan) -> Vec<&'a str> {
        scan.blocks
            .iter()
            .map(|block| &source[block.byte_from..block.byte_to])
            .collect()
    }

    #[test]
    fn list_items_include_multiline_and_nested_content() {
        let source = r#"\begin{itemize}
\item First sentence.
Continuation of the first item.
\begin{enumerate}
\item Nested item.
\end{enumerate}
\item Second item.
\end{itemize}
"#;
        let scan = scan_latex_blocks(source);
        assert!(scan.valid);
        assert_eq!(scan.blocks.len(), 2);
        assert!(texts(source, &scan)[0].starts_with(r"\begin{itemize}"));
        assert!(texts(source, &scan)[0].contains("Nested item."));
        assert!(texts(source, &scan)[1].starts_with(r"\item Second"));
        assert!(texts(source, &scan)[1].ends_with("\\end{itemize}\n"));
        assert_eq!(
            scan.signature,
            vec!["list:itemize", "list:item", "list:item"]
        );
    }

    #[test]
    fn figure_components_keep_commands_and_arguments_intact() {
        let source = r#"\begin{figure}
\centering
\includegraphics[width=.8\textwidth]{plot.pdf}
\caption{A figure. Its caption has two sentences.}
\label{fig:plot}
\end{figure}
"#;
        let scan = scan_latex_blocks(source);
        assert!(scan.valid);
        assert_eq!(scan.blocks.len(), 2);
        let blocks = texts(source, &scan);
        assert!(blocks[0].contains(r"\includegraphics"));
        assert!(!blocks[0].contains(r"\caption"));
        assert!(blocks[1].contains(r"\caption{A figure. Its caption has two sentences.}"));
        assert!(blocks[1].contains(r"\label{fig:plot}"));
        assert!(blocks[1].ends_with("\\end{figure}\n"));
    }

    #[test]
    fn tables_math_code_and_drawings_are_atomic() {
        let source = r#"\begin{table}
\begin{tabular}{cc}
A & B \\
\end{tabular}
\end{table}

\begin{align}
a &= b + c
\end{align}

\begin{lstlisting}
\begin{not-latex}
\end{lstlisting}

\begin{tikzpicture}
\draw (0,0) -- (1,1);
\end{tikzpicture}
"#;
        let scan = scan_latex_blocks(source);
        assert!(scan.valid);
        assert_eq!(scan.blocks.len(), 4);
        assert_eq!(scan.blocks[0].kind, LatexBlockKind::Table);
        assert_eq!(scan.blocks[1].kind, LatexBlockKind::Math);
        assert_eq!(scan.blocks[2].kind, LatexBlockKind::Code);
        assert_eq!(scan.blocks[3].kind, LatexBlockKind::Drawing);
    }

    #[test]
    fn sentence_scanning_respects_macro_arguments_and_math() {
        let source =
            r"See \textbf{version 2.0. It works}. Next sentence. The value $x. y$ is stable.";
        let scan = scan_latex_blocks(source);
        assert!(scan.valid);
        assert_eq!(scan.blocks.len(), 3);
        let blocks = texts(source, &scan);
        assert_eq!(blocks[0], r"See \textbf{version 2.0. It works}. ");
        assert_eq!(blocks[1], "Next sentence. ");
        assert_eq!(blocks[2], r"The value $x. y$ is stable.");
    }

    #[test]
    fn display_math_is_atomic() {
        let source = "Before.\n\\[\na = b + c.\n\\]\nAfter.\n";
        let scan = scan_latex_blocks(source);
        assert!(scan.valid);
        assert_eq!(scan.blocks.len(), 3);
        assert_eq!(scan.blocks[1].kind, LatexBlockKind::Math);
        assert_eq!(texts(source, &scan)[1], "\\[\na = b + c.\n\\]\n");
    }

    #[test]
    fn text_containers_ignore_fake_environments_inside_code() {
        let source = r#"\begin{document}
Before.
\begin{lstlisting}
\begin{not-latex}
\item also not LaTeX
\end{lstlisting}
After.
\end{document}
"#;
        let scan = scan_latex_blocks(source);
        assert!(scan.valid);
        assert!(
            scan.blocks
                .iter()
                .any(|block| block.kind == LatexBlockKind::Code)
        );
        assert!(
            !scan
                .blocks
                .iter()
                .any(|block| block.kind == LatexBlockKind::Unknown)
        );
    }

    #[test]
    fn appendix_commands_are_structural() {
        let source = "\\appendix\n\\section{Supplement}\n\\appendices\n";
        let scan = scan_latex_blocks(source);
        assert!(scan.valid);
        assert!(
            scan.blocks
                .iter()
                .all(|block| block.kind != LatexBlockKind::Unknown)
        );
        assert_eq!(
            scan.signature,
            vec!["command:appendix", "command:section", "command:appendices"]
        );
    }

    #[test]
    fn unknown_or_unclosed_environments_are_not_trusted() {
        let unknown = scan_latex_blocks("\\begin{custom}\ntext\n\\end{custom}\n");
        assert!(unknown.valid);
        assert_eq!(unknown.blocks[0].kind, LatexBlockKind::Unknown);

        let unclosed = scan_latex_blocks("\\begin{itemize}\n\\item text\n");
        assert!(!unclosed.valid);
        assert_eq!(unclosed.blocks[0].kind, LatexBlockKind::Unknown);
    }

    #[test]
    fn unknown_commands_and_extended_environment_names_are_untrusted() {
        let command = scan_latex_blocks("\\custommacro{alpha. beta.}\n");
        assert!(command.valid);
        assert_eq!(command.blocks.len(), 1);
        assert_eq!(command.blocks[0].kind, LatexBlockKind::Unknown);

        let environment = scan_latex_blocks("\\begin{custom_layout}\ntext\n\\end{custom_layout}\n");
        assert!(environment.valid);
        assert_eq!(environment.blocks.len(), 1);
        assert_eq!(environment.blocks[0].kind, LatexBlockKind::Unknown);
    }

    #[test]
    fn commented_environment_commands_are_ignored() {
        let source = "% \\begin{table}\nText. More text.\n";
        let scan = scan_latex_blocks(source);
        assert!(scan.valid);
        assert!(scan.signature.is_empty());
        assert_eq!(scan.blocks.len(), 3);
    }
}
