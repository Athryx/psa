//! Lightweight, lossless highlighting for tool source. Text is always rendered as
//! Dioxus text nodes, not HTML; unknown or incomplete syntax remains visible.
use dioxus::prelude::*;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum CodeLanguage {
    Bash,
    Python,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TokenKind {
    Plain,
    Keyword,
    String,
    Comment,
    Number,
    Variable,
}

impl TokenKind {
    fn class(self) -> &'static str {
        match self {
            Self::Plain => "syntax-plain",
            Self::Keyword => "syntax-keyword",
            Self::String => "syntax-string",
            Self::Comment => "syntax-comment",
            Self::Number => "syntax-number",
            Self::Variable => "syntax-variable",
        }
    }
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn is_keyword(word: &str, language: CodeLanguage) -> bool {
    match language {
        CodeLanguage::Python => matches!(
            word,
            "False"
                | "None"
                | "True"
                | "and"
                | "as"
                | "assert"
                | "async"
                | "await"
                | "break"
                | "class"
                | "continue"
                | "def"
                | "del"
                | "elif"
                | "else"
                | "except"
                | "finally"
                | "for"
                | "from"
                | "global"
                | "if"
                | "import"
                | "in"
                | "is"
                | "lambda"
                | "nonlocal"
                | "not"
                | "or"
                | "pass"
                | "raise"
                | "return"
                | "try"
                | "while"
                | "with"
                | "yield"
        ),
        CodeLanguage::Bash => matches!(
            word,
            "if" | "then"
                | "else"
                | "elif"
                | "fi"
                | "for"
                | "while"
                | "until"
                | "do"
                | "done"
                | "case"
                | "esac"
                | "in"
                | "function"
                | "select"
                | "time"
                | "coproc"
        ),
    }
}

/// Return byte ranges into the original text, including all whitespace and newlines.
fn tokenize(text: &str, language: CodeLanguage) -> Vec<(TokenKind, &str)> {
    let mut tokens: Vec<(TokenKind, &str)> = Vec::new();
    let mut pos = 0;
    while pos < text.len() {
        let rest = &text[pos..];
        let first = rest.chars().next().unwrap();
        let mut end = pos + first.len_utf8();
        let kind;
        if first == '#'
            && (language == CodeLanguage::Python
                || pos == 0
                || text[..pos]
                    .chars()
                    .next_back()
                    .is_some_and(|c| c.is_whitespace() || ";|&()".contains(c)))
        {
            end = text[pos..].find('\n').map_or(text.len(), |n| pos + n);
            kind = TokenKind::Comment;
        } else if first == '\'' || first == '"' {
            let triple =
                language == CodeLanguage::Python && rest.starts_with(&first.to_string().repeat(3));
            if triple {
                end = pos + 3;
            }
            while end < text.len() {
                let c = text[end..].chars().next().unwrap();
                if c == '\\' && !(language == CodeLanguage::Bash && first == '\'') {
                    end += 1;
                    if end < text.len() {
                        end += text[end..].chars().next().unwrap().len_utf8();
                    }
                } else if c == first {
                    if !triple {
                        end += c.len_utf8();
                        break;
                    }
                    if text[end..].starts_with(&first.to_string().repeat(3)) {
                        end += 3;
                        break;
                    }
                    end += c.len_utf8();
                } else {
                    end += c.len_utf8();
                }
            }
            kind = TokenKind::String;
        } else if language == CodeLanguage::Bash && first == '$' {
            if text[end..].starts_with('{') {
                end = text[end..].find('}').map_or(text.len(), |n| end + n + 1);
            } else if let Some(c) = text[end..].chars().next() {
                if c.is_ascii_digit() || "?!#$@*-".contains(c) {
                    end += c.len_utf8();
                } else {
                    while end < text.len() && text[end..].chars().next().is_some_and(is_word) {
                        end += text[end..].chars().next().unwrap().len_utf8();
                    }
                }
            }
            kind = TokenKind::Variable;
        } else if first.is_ascii_digit() {
            while end < text.len()
                && text[end..]
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
            {
                end += text[end..].chars().next().unwrap().len_utf8();
            }
            kind = TokenKind::Number;
        } else if is_word(first) {
            while end < text.len() && text[end..].chars().next().is_some_and(is_word) {
                end += text[end..].chars().next().unwrap().len_utf8();
            }
            kind = if is_keyword(&text[pos..end], language) {
                TokenKind::Keyword
            } else {
                TokenKind::Plain
            };
        } else {
            kind = TokenKind::Plain;
        }
        // Coalesce adjacent tokens to avoid creating a DOM node per punctuation or space.
        if let Some((previous_kind, previous_text)) = tokens.last_mut() {
            if *previous_kind == kind {
                let start = pos - previous_text.len();
                *previous_text = &text[start..end];
                pos = end;
                continue;
            }
        }
        tokens.push((kind, &text[pos..end]));
        pos = end;
    }
    tokens
}

/// Render a complete source snippet inside a preformatted block.
#[component]
pub(crate) fn HighlightedCode(code: String, language: CodeLanguage) -> Element {
    // Large tool inputs stay responsive and retain their complete text.
    let tokens = if code.len() > 16 * 1024 {
        vec![(TokenKind::Plain, code.as_str())]
    } else {
        tokenize(&code, language)
    };
    rsx! {
        pre { class: "syntax-code", code {
            for (kind, text) in tokens {
                span { class: kind.class(), "{text}" }
            }
        } }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_source_and_marks_expected_tokens() {
        for (source, language) in [
            (
                "if [ \"$x\" ]; then # hello\n echo ${name}\nfi\n",
                CodeLanguage::Bash,
            ),
            (
                "def greet(name):\n    return \"héllo # world\" # comment\n",
                CodeLanguage::Python,
            ),
            ("'''multi\nline'''\n42 < 100 & x", CodeLanguage::Python),
        ] {
            let tokens = tokenize(source, language);
            assert_eq!(
                tokens.iter().map(|(_, text)| *text).collect::<String>(),
                source
            );
            assert!(tokens.iter().any(|(kind, _)| *kind == TokenKind::String));
        }
        let bash = tokenize("if true; then echo $HOME # hi\nfi", CodeLanguage::Bash);
        assert!(bash.contains(&(TokenKind::Keyword, "if")));
        assert!(bash.contains(&(TokenKind::Variable, "$HOME")));
        assert!(bash.contains(&(TokenKind::Comment, "# hi")));
        assert!(
            !tokenize("echo foo#bar", CodeLanguage::Bash)
                .iter()
                .any(|(kind, _)| *kind == TokenKind::Comment)
        );
    }

    #[test]
    fn renders_source_as_escaped_text() {
        let html = dioxus_ssr::render_element(rsx! {
            HighlightedCode { code: "if <script>alert(1)</script>".to_owned(), language: CodeLanguage::Python }
        });
        assert!(html.contains("syntax-keyword"));
        assert!(!html.contains("<script>"), "{html}");
        assert!(html.contains("script"), "{html}");
    }

    #[test]
    fn incomplete_input_is_lossless() {
        for source in ["", "'unfinished", "${unfinished", "\"abc\\", "🐍 # comment"] {
            for language in [CodeLanguage::Bash, CodeLanguage::Python] {
                assert_eq!(
                    tokenize(source, language)
                        .iter()
                        .map(|(_, text)| *text)
                        .collect::<String>(),
                    source
                );
            }
        }
    }
}
