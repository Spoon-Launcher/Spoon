//! Lightweight syntax highlighting for the file editor.
//!
//! Tokenizes common server config formats into [`Span`]s with a semantic
//! [`TokenKind`]; the GUI maps kinds to colors. Spans are sorted, never
//! overlap, and always sit on char boundaries so the text can be sliced
//! safely.

use std::path::Path;

/// Language detected from a file's extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    Properties,
    Json,
    Yaml,
    Toml,
    Ini,
    Xml,
    Log,
}

/// Semantic token class; the GUI maps these to colors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    Comment,
    Key,
    Str,
    Number,
    Bool,
    Section,
    Punct,
    Info,
    Warn,
    Error,
}

/// A half-open byte range of the highlighted text.
#[derive(Debug, Clone, Copy)]
pub struct Span {
    pub start: usize,
    pub end: usize,
    pub kind: TokenKind,
}

/// Detects the highlighting language from a file extension.
pub fn detect(path: &Path) -> Option<Language> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "properties" => Some(Language::Properties),
        "json" | "mcmeta" => Some(Language::Json),
        "yml" | "yaml" => Some(Language::Yaml),
        "toml" => Some(Language::Toml),
        "ini" | "cfg" | "conf" => Some(Language::Ini),
        "xml" | "htm" | "html" => Some(Language::Xml),
        "log" | "txt" => Some(Language::Log),
        _ => None,
    }
}

/// Highlights `text` as `language`, returning sorted non-overlapping spans.
pub fn highlight(language: Language, text: &str) -> Vec<Span> {
    let mut spans = Vec::new();
    match language {
        Language::Properties => highlight_properties(text, &mut spans),
        Language::Json => highlight_json(text, &mut spans),
        Language::Yaml => highlight_yaml(text, &mut spans),
        Language::Toml => highlight_toml(text, &mut spans),
        Language::Ini => highlight_ini(text, &mut spans),
        Language::Xml => highlight_xml(text, &mut spans),
        Language::Log => highlight_log(text, &mut spans),
    }
    spans
}

fn push(spans: &mut Vec<Span>, kind: TokenKind, start: usize, end: usize) {
    if start < end {
        spans.push(Span { start, end, kind });
    }
}

/// Calls `f` with the byte offset and text of each line (without `\n`).
fn for_each_line(text: &str, mut f: impl FnMut(usize, &str)) {
    let mut start = 0;
    for line in text.split('\n') {
        f(start, line);
        start += line.len() + 1;
    }
}

/// Returns the offset just past the longest prefix matching `f`.
fn consume_while(
    iter: &mut std::iter::Peekable<std::str::CharIndices>,
    f: impl Fn(char) -> bool,
) -> usize {
    let mut end = 0;
    while let Some(&(_, c)) = iter.peek() {
        if !f(c) {
            break;
        }
        let (i, c) = iter.next().expect("peeked char must yield");
        end = i + c.len_utf8();
    }
    end
}

/// Colors a value after `=`/`:`: quoted strings, booleans and numbers.
fn classify_value(value: &str, offset: usize, spans: &mut Vec<Span>) {
    let leading = value.len() - value.trim_start().len();
    let token = value.trim();
    let start = offset + leading;
    if token.starts_with('"') || token.starts_with('\'') {
        push(spans, TokenKind::Str, start, start + token.len());
    } else if matches!(token, "true" | "false") {
        push(spans, TokenKind::Bool, start, start + token.len());
    } else if token.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        push(spans, TokenKind::Number, start, start + token.len());
    }
}

fn highlight_properties(text: &str, spans: &mut Vec<Span>) {
    for_each_line(text, |base, line| {
        let trimmed = line.trim_start();
        if trimmed.starts_with('#') || trimmed.starts_with('!') {
            push(spans, TokenKind::Comment, base, base + line.len());
            return;
        }
        let sep = line.find(['=', ':']).unwrap_or(line.len());
        push(spans, TokenKind::Key, base, base + sep);
        if sep < line.len() {
            push(spans, TokenKind::Punct, base + sep, base + sep + 1);
            push(spans, TokenKind::Str, base + sep + 1, base + line.len());
        }
    });
}

fn highlight_ini(text: &str, spans: &mut Vec<Span>) {
    for_each_line(text, |base, line| {
        let trimmed = line.trim_start();
        let indent = line.len() - trimmed.len();
        if trimmed.starts_with(';') || trimmed.starts_with('#') {
            push(spans, TokenKind::Comment, base + indent, base + line.len());
        } else if trimmed.starts_with('[') {
            push(spans, TokenKind::Section, base + indent, base + line.len());
        } else if let Some(eq) = line.find('=') {
            push(spans, TokenKind::Key, base, base + eq);
            push(spans, TokenKind::Punct, base + eq, base + eq + 1);
            push(spans, TokenKind::Str, base + eq + 1, base + line.len());
        }
    });
}

fn highlight_toml(text: &str, spans: &mut Vec<Span>) {
    for_each_line(text, |base, line| {
        let trimmed = line.trim_start();
        let indent = line.len() - trimmed.len();
        if trimmed.starts_with('#') {
            push(spans, TokenKind::Comment, base + indent, base + line.len());
        } else if trimmed.starts_with('[') {
            push(spans, TokenKind::Section, base + indent, base + line.len());
        } else if let Some(eq) = line.find('=') {
            push(spans, TokenKind::Key, base, base + eq);
            push(spans, TokenKind::Punct, base + eq, base + eq + 1);
            classify_value(&line[eq + 1..], base + eq + 1, spans);
        }
    });
}

fn highlight_yaml(text: &str, spans: &mut Vec<Span>) {
    for_each_line(text, |base, line| {
        // A comment starts at a '#' preceded by whitespace or line start and
        // not inside quotes.
        let mut cut = line.len();
        let mut in_single = false;
        let mut in_double = false;
        let mut prev_whitespace = true;
        for (i, c) in line.char_indices() {
            match c {
                '\'' if !in_double => in_single = !in_single,
                '"' if !in_single => in_double = !in_double,
                '#' if !in_single && !in_double && prev_whitespace => {
                    cut = i;
                    break;
                }
                _ => {}
            }
            prev_whitespace = c.is_whitespace();
        }
        let content = &line[..cut];
        if cut < line.len() {
            push(spans, TokenKind::Comment, base + cut, base + line.len());
        }
        let lead = content.len() - content.trim_start().len();
        let body = &content[lead..];
        let item = if body.starts_with("- ") {
            push(spans, TokenKind::Punct, base + lead, base + lead + 1);
            lead + 2
        } else {
            lead
        };
        let rest = &content[item..];
        if let Some(colon) = rest.find(':') {
            let after = rest[colon + 1..].chars().next();
            if after.is_none_or(|c| c == ' ' || c == '\t') {
                push(spans, TokenKind::Key, base + item, base + item + colon);
                push(
                    spans,
                    TokenKind::Punct,
                    base + item + colon,
                    base + item + colon + 1,
                );
                classify_value(&rest[colon + 1..], base + item + colon + 1, spans);
            }
        }
    });
}

fn highlight_json(text: &str, spans: &mut Vec<Span>) {
    let mut iter = text.char_indices().peekable();
    while let Some((i, c)) = iter.next() {
        match c {
            '{' | '}' | '[' | ']' | ':' | ',' => push(spans, TokenKind::Punct, i, i + 1),
            '"' => {
                let start = i;
                let mut end = text.len();
                while let Some((j, c)) = iter.next() {
                    if c == '\\' {
                        iter.next();
                    } else if c == '"' {
                        end = j + 1;
                        break;
                    }
                }
                push(spans, TokenKind::Str, start, end);
            }
            '-' if iter.peek().is_some_and(|&(_, c)| c.is_ascii_digit()) => {
                let end = consume_while(&mut iter, |c| {
                    c.is_ascii_alphanumeric() || matches!(c, '.' | '+' | '-')
                });
                push(spans, TokenKind::Number, i, end.max(i + 1));
            }
            c if c.is_ascii_digit() => {
                let end = consume_while(&mut iter, |c| {
                    c.is_ascii_alphanumeric() || matches!(c, '.' | '+' | '-')
                });
                push(spans, TokenKind::Number, i, end.max(i + c.len_utf8()));
            }
            c if c.is_alphabetic() => {
                let start = i;
                let mut end = i + c.len_utf8();
                while let Some(&(_, c)) = iter.peek() {
                    if !c.is_alphabetic() {
                        break;
                    }
                    let (j, c) = iter.next().expect("peeked char must yield");
                    end = j + c.len_utf8();
                }
                if matches!(&text[start..end], "true" | "false" | "null") {
                    push(spans, TokenKind::Bool, start, end);
                }
            }
            _ => {}
        }
    }
}

fn highlight_xml(text: &str, spans: &mut Vec<Span>) {
    let mut base = 0;
    let mut rest = text;
    while let Some(lt) = rest.find('<') {
        base += lt;
        rest = &rest[lt..];
        let end = if rest.starts_with("<!--") {
            rest.find("-->").map(|i| i + 3)
        } else {
            // Find '>' outside quotes.
            let mut quote = None;
            let mut end = None;
            for (i, c) in rest.char_indices() {
                if let Some(q) = quote {
                    if c == q {
                        quote = None;
                    }
                } else if c == '"' || c == '\'' {
                    quote = Some(c);
                } else if c == '>' {
                    end = Some(i + 1);
                    break;
                }
            }
            end
        };
        let tag_len = end.unwrap_or(rest.len());
        highlight_xml_tag(&rest[..tag_len], base, spans);
        base += tag_len;
        rest = &rest[tag_len..];
    }
}

fn highlight_xml_tag(tag: &str, base: usize, spans: &mut Vec<Span>) {
    if tag.starts_with("<!--") || tag.starts_with("<?") || tag.starts_with("<!") {
        push(spans, TokenKind::Comment, base, base + tag.len());
        return;
    }
    let mut iter = tag.char_indices().peekable();
    // '<'
    iter.next();
    push(spans, TokenKind::Punct, base, base + 1);
    // Optional closing '/'.
    if let Some(&(_, '/')) = iter.peek() {
        iter.next();
        push(spans, TokenKind::Punct, base + 1, base + 2);
    }
    // Tag name.
    let name_end = consume_while(&mut iter, |c| {
        c.is_alphanumeric() || c == '_' || c == '-' || c == ':'
    });
    let start = if tag.as_bytes().get(1) == Some(&b'/') {
        2
    } else {
        1
    };
    push(
        spans,
        TokenKind::Key,
        base + start,
        base + name_end.max(start),
    );
    // Attributes: name, '=', quoted value; other punctuation as-is.
    while let Some((i, c)) = iter.next() {
        match c {
            '>' | '/' | '?' => push(spans, TokenKind::Punct, base + i, base + i + 1),
            '"' | '\'' => {
                let start = i;
                let quote = c;
                let mut end = tag.len();
                for (j, c) in iter.by_ref() {
                    if c == quote {
                        end = j + 1;
                        break;
                    }
                }
                push(spans, TokenKind::Str, base + start, base + end);
            }
            c if c.is_whitespace() => {}
            c if c.is_alphanumeric() || c == '_' || c == '-' || c == ':' => {
                let start = i;
                let mut end = i + c.len_utf8();
                while let Some(&(_, c)) = iter.peek() {
                    if !(c.is_alphanumeric() || c == '_' || c == '-' || c == ':') {
                        break;
                    }
                    let (j, c) = iter.next().expect("peeked char must yield");
                    end = j + c.len_utf8();
                }
                push(spans, TokenKind::Key, base + start, base + end);
            }
            _ => {}
        }
    }
}

fn highlight_log(text: &str, spans: &mut Vec<Span>) {
    // Timestamps like [12:03:45].
    let text_bytes = text.as_bytes();
    for (i, _) in text.match_indices('[') {
        let bytes = &text_bytes[i..];
        let digits = |b: u8| b.is_ascii_digit();
        if bytes.len() >= 10
            && digits(bytes[1])
            && digits(bytes[2])
            && bytes[3] == b':'
            && digits(bytes[4])
            && digits(bytes[5])
            && bytes[6] == b':'
            && digits(bytes[7])
            && digits(bytes[8])
            && bytes[9] == b']'
        {
            push(spans, TokenKind::Number, i, i + 10);
        }
    }
    // Log levels as whole words (case-insensitive).
    const LEVELS: &[(&str, TokenKind)] = &[
        ("INFO", TokenKind::Info),
        ("DEBUG", TokenKind::Info),
        ("WARN", TokenKind::Warn),
        ("WARNING", TokenKind::Warn),
        ("ERROR", TokenKind::Error),
        ("SEVERE", TokenKind::Error),
        ("FATAL", TokenKind::Error),
    ];
    let is_word_char = |c: char| c.is_alphanumeric() || c == '_' || c == '-';
    let mut word_start = None;
    let mut words = Vec::new();
    for (i, c) in text.char_indices() {
        if is_word_char(c) {
            word_start.get_or_insert(i);
        } else if let Some(start) = word_start.take() {
            words.push((start, i));
        }
    }
    if let Some(start) = word_start.take() {
        words.push((start, text.len()));
    }
    for (start, end) in words {
        let word = &text[start..end];
        if let Some((_, kind)) = LEVELS
            .iter()
            .find(|(needle, _)| word.eq_ignore_ascii_case(needle))
        {
            push(spans, *kind, start, end);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind_at(spans: &[Span], needle: &str, text: &str) -> Option<TokenKind> {
        let pos = text.find(needle).unwrap();
        spans
            .iter()
            .find(|span| span.start <= pos && pos < span.end)
            .map(|span| span.kind)
    }

    #[test]
    fn detects_languages_by_extension() {
        use Language::*;
        let cases = [
            ("server.properties", Some(Properties)),
            ("options.json", Some(Json)),
            ("pack.mcmeta", Some(Json)),
            ("bukkit.yml", Some(Yaml)),
            ("sponge.toml", Some(Toml)),
            ("server.conf", Some(Ini)),
            ("paper-global.yml", Some(Yaml)),
            ("mods.toml", Some(Toml)),
            ("log4j2.xml", Some(Xml)),
            ("latest.log", Some(Log)),
            ("server.jar", None),
            ("world", None),
        ];
        for (name, expected) in cases {
            assert_eq!(detect(Path::new(name)), expected, "{name}");
        }
    }

    #[test]
    fn highlights_properties() {
        let text = "# comment\nmotd=A Minecraft Server\nlevel-name : world\n";
        let spans = highlight(Language::Properties, text);
        assert_eq!(kind_at(&spans, "comment", text), Some(TokenKind::Comment));
        assert_eq!(kind_at(&spans, "motd", text), Some(TokenKind::Key));
        assert_eq!(kind_at(&spans, "=", text), Some(TokenKind::Punct));
        assert_eq!(
            kind_at(&spans, "A Minecraft Server", text),
            Some(TokenKind::Str)
        );
        assert_eq!(kind_at(&spans, "level-name", text), Some(TokenKind::Key));
        assert_eq!(kind_at(&spans, "world", text), Some(TokenKind::Str));
    }

    #[test]
    fn highlights_json() {
        let text = r#"{"max-players": 20, "online-mode": true, "motd": "hi \"there\""}"#;
        let spans = highlight(Language::Json, text);
        assert_eq!(kind_at(&spans, "max-players", text), Some(TokenKind::Str));
        assert_eq!(kind_at(&spans, "20", text), Some(TokenKind::Number));
        assert_eq!(kind_at(&spans, "true", text), Some(TokenKind::Bool));
        assert_eq!(kind_at(&spans, "hi", text), Some(TokenKind::Str));
    }

    #[test]
    fn highlights_yaml() {
        let text = "# head\nsettings:\n  online-mode: true # tail\n  - item: 42\n";
        let spans = highlight(Language::Yaml, text);
        assert_eq!(kind_at(&spans, "head", text), Some(TokenKind::Comment));
        assert_eq!(kind_at(&spans, "settings", text), Some(TokenKind::Key));
        assert_eq!(kind_at(&spans, "online-mode", text), Some(TokenKind::Key));
        assert_eq!(kind_at(&spans, "true", text), Some(TokenKind::Bool));
        assert_eq!(kind_at(&spans, "tail", text), Some(TokenKind::Comment));
        assert_eq!(kind_at(&spans, "42", text), Some(TokenKind::Number));
    }

    #[test]
    fn highlights_toml_and_ini() {
        let toml = "[general]\nspeed = 2.5\nenabled = true\n";
        let spans = highlight(Language::Toml, toml);
        assert_eq!(kind_at(&spans, "[general]", toml), Some(TokenKind::Section));
        assert_eq!(kind_at(&spans, "speed", toml), Some(TokenKind::Key));
        assert_eq!(kind_at(&spans, "2.5", toml), Some(TokenKind::Number));
        assert_eq!(kind_at(&spans, "true", toml), Some(TokenKind::Bool));

        let ini = "; note\n[section]\nkey=value\n";
        let spans = highlight(Language::Ini, ini);
        assert_eq!(kind_at(&spans, "note", ini), Some(TokenKind::Comment));
        assert_eq!(kind_at(&spans, "[section]", ini), Some(TokenKind::Section));
        assert_eq!(kind_at(&spans, "key", ini), Some(TokenKind::Key));
        assert_eq!(kind_at(&spans, "value", ini), Some(TokenKind::Str));
    }

    #[test]
    fn highlights_xml_and_log() {
        let xml = "<!-- note --><a href=\"x\">text</a>";
        let spans = highlight(Language::Xml, xml);
        assert_eq!(kind_at(&spans, "note", xml), Some(TokenKind::Comment));
        assert_eq!(kind_at(&spans, "a href", xml), Some(TokenKind::Key));
        assert_eq!(kind_at(&spans, "\"x\"", xml), Some(TokenKind::Str));

        let log =
            "[12:03:45] [INFO] Started\n[12:03:46] [WARN] Low memory\n[12:03:47] [ERROR] Crash\n";
        let spans = highlight(Language::Log, log);
        assert_eq!(kind_at(&spans, "12:03:45", log), Some(TokenKind::Number));
        assert_eq!(kind_at(&spans, "INFO", log), Some(TokenKind::Info));
        assert_eq!(kind_at(&spans, "WARN", log), Some(TokenKind::Warn));
        assert_eq!(kind_at(&spans, "ERROR", log), Some(TokenKind::Error));
        assert_eq!(kind_at(&spans, "Started", log), None);
    }

    #[test]
    fn spans_are_sorted_and_on_char_boundaries() {
        let nasty = "key=valé\n{\"k\": \"v\\\"\u{1F600}\"} # ü\n[sec]\n<!-- c --> <a href=\"x\">t</a>\n[12:03:45] [INFO] warn ERROR\n- item: 42\nurl: http://x\n";
        for language in [
            Language::Properties,
            Language::Json,
            Language::Yaml,
            Language::Toml,
            Language::Ini,
            Language::Xml,
            Language::Log,
        ] {
            let spans = highlight(language, nasty);
            let mut last = 0;
            for span in &spans {
                assert!(span.start <= span.end);
                assert!(span.end <= nasty.len());
                assert!(nasty.is_char_boundary(span.start));
                assert!(nasty.is_char_boundary(span.end));
                assert!(span.start >= last, "overlapping spans for {language:?}");
                last = span.end;
            }
        }
    }
}
