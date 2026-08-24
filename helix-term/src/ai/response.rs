use helix_view::info::Info;

/// A navigation location extracted from an AI response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocationSuggestion {
    pub path: String,
    pub line: Option<usize>,
}

/// Extract file/line navigation suggestions from AI response text.
///
/// Recognizes patterns like:
/// - `src/main.rs:42`
/// - `src/lib.rs line 10`
/// - bare paths like `src/config.rs` (line unknown)
///
/// Returns up to `max` suggestions in order of appearance.
pub fn parse_location_suggestions(text: &str, max: usize) -> Vec<LocationSuggestion> {
    let mut suggestions = Vec::new();

    for word in text.split_whitespace() {
        if suggestions.len() >= max {
            break;
        }

        // Pattern: path:line
        if let Some((path_part, line_part)) = word.rsplit_once(':') {
            let path = path_part.trim_matches(|c| c == '`' || c == '*' || c == '(');
            if looks_like_path(path) && line_part.parse::<usize>().is_ok() {
                let sugg = LocationSuggestion {
                    path: path.to_string(),
                    line: Some(line_part.parse::<usize>().unwrap()),
                };
                if !suggestions.contains(&sugg) {
                    suggestions.push(sugg);
                }
                continue;
            }
        }

        // Pattern: path (no line)
        let cleaned = word.trim_matches(|c| c == '`' || c == '*' || c == '(' || c == ')' || c == ',');
        if looks_like_path(cleaned) && !word.contains(':') {
            let sugg = LocationSuggestion {
                path: cleaned.to_string(),
                line: None,
            };
            if !suggestions.contains(&sugg) {
                suggestions.push(sugg);
            }
        }
    }

    suggestions
}

/// Heuristic check that a token looks like a source file path.
fn looks_like_path(token: &str) -> bool {
    if token.len() < 4 {
        return false;
    }
    // Exclude URLs
    if token.contains("://") {
        return false;
    }
    let has_separator = token.contains('/');
    let has_extension = token.rsplit('.').next().map(|ext| {
        ext.len() >= 1 && ext.chars().all(|c| c.is_ascii_alphanumeric())
    }).unwrap_or(false);
    has_extension && (has_separator || token.ends_with(".rs") || token.ends_with(".py")
        || token.ends_with(".ts") || token.ends_with(".js") || token.ends_with(".go")
        || token.ends_with(".java") || token.ends_with(".c") || token.ends_with(".cpp"))
}

/// A suggested Target → Action candidate extracted from an AI response.
///
/// The AI never edits. This only describes what the user *could* do,
/// expressed in Tachyon's own operation language (Target + Action).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActionSuggestion {
    pub target: Option<&'static str>,
    pub action: Option<&'static str>,
}

/// Extract ALL actionable Target → Action suggestions from AI text,
/// deduplicated, in order of appearance.
///
/// Malformed or partial matches (target without action or vice versa) are
/// rejected — no suggestion is produced unless BOTH halves parse.
pub fn parse_all_action_suggestions(text: &str) -> Vec<ActionSuggestion> {
    let mut out: Vec<ActionSuggestion> = Vec::new();

    // Each "suggestion" keyword starts a NON-overlapping scope that ends at
    // the next keyword, so independent suggestion blocks are evaluated
    // independently. Without any keyword the whole text is one scope.
    let lower = text.to_lowercase();
    let starts: Vec<usize> = lower
        .match_indices("suggestion")
        .map(|(i, _)| i)
        .collect();

    let scopes: Vec<&str> = if starts.is_empty() {
        vec![lower.as_str()]
    } else {
        starts
            .iter()
            .enumerate()
            .map(|(idx, i)| {
                let s = i + "suggestion".len();
                let e = starts.get(idx + 1).copied().unwrap_or(lower.len());
                &lower[s..e]
            })
            .collect()
    };

    for scope in scopes {
        if let Some(s) = extract_pair(scope) {
            if !out.contains(&s) {
                out.push(s);
            }
        }
    }
    out
}

fn extract_pair(scope: &str) -> Option<ActionSuggestion> {
    const ACTIONS: [(&str, &str); 7] = [
        ("replace", "change"),
        ("rewrite", "change"),
        ("refactor", "change"),
        ("change", "change"),
        ("fix", "change"),
        ("delete", "delete"),
        ("remove", "delete"),
    ];
    const TARGETS: [(&str, &str); 15] = [
        ("function", "function"),
        ("method", "function"),
        ("statement", "statement"),
        ("expression", "expression"),
        ("identifier", "word"),
        ("variable", "word"),
        ("symbol", "word"),
        ("name", "word"),
        ("block", "block"),
        ("body", "block"),
        ("class", "class"),
        ("struct", "class"),
        ("enum", "class"),
        ("argument", "argument"),
        ("parameter", "argument"),
    ];

    let action = ACTIONS
        .into_iter()
        .find(|(kw, _)| scope.contains(kw))
        .map(|(_, act)| act)?;
    let target = TARGETS
        .into_iter()
        .find(|(kw, _)| scope.contains(kw))
        .map(|(_, tgt)| tgt)?;

    Some(ActionSuggestion { target: Some(target), action: Some(action) })
}

/// Extract the first actionable Target → Action suggestion from AI text.
///
/// Recognizes action verbs (change/replace, delete/remove) and target nouns
/// (function, statement, expression, word/identifier, block/body, class,
/// argument/parameter). Returns `None` when no clear pair is found.
pub fn parse_action_suggestion(text: &str) -> Option<ActionSuggestion> {
    parse_all_action_suggestions(text).into_iter().next()
}

/// Format an action suggestion for the status line.
pub fn format_action_suggestion(sugg: &ActionSuggestion) -> String {
    let target = sugg.target.unwrap_or("?");
    let action = sugg.action.unwrap_or("?");
    format!("[AI Suggestion] target: {} → action: {} (press t{}{} or :ai-apply)",
        target, action,
        crate::target::Target::from_name(target)
            .map(|t| t.key().to_string())
            .unwrap_or_else(|| "?".to_string()),
        match action {
            "change" => 'c',
            "delete" => 'd',
            "yank" => 'y',
            "indent" => '>',
            "outdent" => '<',
            _ => '?',
        })
}

/// Format an AI response for display in the editor.
pub fn format_response(response: &str, provider: &str, model: &str) -> Info {
    let title = format!("AI Response ({})", model);

    let display_text = if response.len() > 2000 {
        format!(
            "{}\n\n... (truncated, {} chars total)",
            &response[..2000],
            response.len()
        )
    } else {
        response.to_string()
    };

    Info::new(title, &[(provider, display_text.as_str())])
}

/// Format an AI error for display.
pub fn format_error(provider: &str, error: &str) -> String {
    format!("{}: {}", provider, error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_location_path_line() {
        let text = "The problem is in src/db.rs:42 where the lock is dropped.";
        let sugg = parse_location_suggestions(text, 5);
        assert_eq!(sugg.len(), 1);
        assert_eq!(sugg[0].path, "src/db.rs");
        assert_eq!(sugg[0].line, Some(42));
    }

    #[test]
    fn test_parse_location_bare_path() {
        let text = "Check `src/config.rs` for the settings.";
        let sugg = parse_location_suggestions(text, 5);
        assert!(sugg.iter().any(|s| s.path == "src/config.rs" && s.line.is_none()));
    }

    #[test]
    fn test_parse_location_multiple() {
        let text = "See src/a.rs:1 and src/b.rs:2 and src/a.rs:1 again.";
        let sugg = parse_location_suggestions(text, 5);
        // duplicate src/a.rs:1 should be deduplicated
        assert_eq!(sugg.len(), 2);
        assert_eq!(sugg[0].path, "src/a.rs");
        assert_eq!(sugg[0].line, Some(1));
        assert_eq!(sugg[1].path, "src/b.rs");
        assert_eq!(sugg[1].line, Some(2));
    }

    #[test]
    fn test_parse_location_max_limit() {
        let text = "a.rs:1 b.rs:2 c.rs:3 d.rs:4 e.rs:5 f.rs:6";
        let sugg = parse_location_suggestions(text, 3);
        assert_eq!(sugg.len(), 3);
    }

    #[test]
    fn test_parse_location_no_false_positives() {
        let text = "The ratio 3:2 is not a path. Also http://example.com:8080 is not.";
        let sugg = parse_location_suggestions(text, 10);
        // URLs and ratios should not match
        assert!(!sugg.iter().any(|s| s.path.contains("http")));
        assert!(!sugg.iter().any(|s| s.path == "3"));
    }

    #[test]
    fn test_parse_location_empty() {
        let sugg = parse_location_suggestions("", 5);
        assert!(sugg.is_empty());
    }

    #[test]
    fn test_format_response() {
        let info = format_response("Hello world", "OpenCode Zen", "gpt-4");
        assert!(info.title.contains("gpt-4"));
    }

    #[test]
    fn test_format_response_truncation() {
        let long = "x".repeat(3000);
        let info = format_response(&long, "OpenCode Zen", "gpt-4");
        assert!(info.text.contains("truncated"));
        assert!(info.text.contains("3000 chars total"));
    }

    #[test]
    fn test_format_error() {
        let err = format_error("OpenCode Zen", "API key invalid");
        assert_eq!(err, "OpenCode Zen: API key invalid");
    }

    #[test]
    fn test_parse_action_suggestion_explicit() {
        let text = "Suggestion: Replace the function body with a match expression.";
        let sugg = parse_action_suggestion(text).unwrap();
        assert_eq!(sugg.target, Some("function"));
        assert_eq!(sugg.action, Some("change"));
    }

    #[test]
    fn test_parse_action_suggestion_delete_statement() {
        let text = "You could delete this statement entirely; it is redundant.";
        let sugg = parse_action_suggestion(text).unwrap();
        assert_eq!(sugg.target, Some("statement"));
        assert_eq!(sugg.action, Some("delete"));
    }

    #[test]
    fn test_parse_action_suggestion_no_target() {
        let text = "This looks fine, no changes needed.";
        assert!(parse_action_suggestion(text).is_none());
    }

    #[test]
    fn test_parse_action_suggestion_word_delete() {
        let text = "Consider removing the unused variable here.";
        // "removing" does not contain "remove" as a substring, so no action
        // keyword matches — the parser correctly returns None.
        assert!(parse_action_suggestion(text).is_none());

        let text = "You should remove the unused variable here.";
        let sugg = parse_action_suggestion(text).unwrap();
        assert_eq!(sugg.target, Some("word"));
        assert_eq!(sugg.action, Some("delete"));
    }

    #[test]
    fn test_format_action_suggestion() {
        let sugg = ActionSuggestion { target: Some("function"), action: Some("change") };
        let msg = format_action_suggestion(&sugg);
        assert!(msg.contains("[AI Suggestion]"));
        assert!(msg.contains("target: function"));
        assert!(msg.contains("action: change"));
        // Should include the keystroke hint tfc
        assert!(msg.contains("tfc"));
    }

    // ============================================================
    // Phase 21: multi-suggestion parsing tests
    // ============================================================

    #[test]
    fn test_parse_all_zero() {
        let text = "The code looks fine. No action required.";
        assert!(parse_all_action_suggestions(text).is_empty());
    }

    #[test]
    fn test_parse_all_single() {
        let text = "Suggestion: Replace the function body.";
        let all = parse_all_action_suggestions(text);
        assert_eq!(all.len(), 1);
        assert_eq!(all[0], ActionSuggestion { target: Some("function"), action: Some("change") });
    }

    #[test]
    fn test_parse_all_multiple_distinct_blocks() {
        let text = "First suggestion: delete this statement entirely.\n\
                    Second suggestion: replace the expression with a constant.\n\
                    Third: rename the variable (no action keyword pairing here).";
        let all = parse_all_action_suggestions(text);
        // Each "suggestion" region is scanned; third has no pair → rejected.
        assert!(all.len() >= 2);
        assert!(all.contains(&ActionSuggestion { target: Some("statement"), action: Some("delete") }));
        assert!(all.contains(&ActionSuggestion { target: Some("expression"), action: Some("change") }));
    }

    #[test]
    fn test_parse_all_deduplicates() {
        let text = "suggestion: delete statement\nsuggestion: delete the statement";
        let all = parse_all_action_suggestions(text);
        // Both regions yield the identical (statement, delete) pair.
        assert_eq!(all.len(), 1);
    }

    #[test]
    fn test_parse_all_rejects_partial_pairs() {
        // Target without any action verb anywhere → no suggestion.
        assert!(parse_all_action_suggestions("the function is long").is_empty());
        // Action without target noun → no suggestion.
        assert!(parse_all_action_suggestions("you could delete it").is_empty());
    }

    #[test]
    fn test_parse_all_target_and_action_names_are_canonical() {
        // Every returned suggestion must round-trip through the canonical
        // enums — this guarantees :ai-apply can never receive arbitrary text.
        let text = "suggestion: refactor the class\n\
                    suggestion: fix the argument order";
        for s in parse_all_action_suggestions(text) {
            assert!(crate::target::Target::from_name(s.target.unwrap()).is_some());
            assert!(crate::target::Action::from_name(s.action.unwrap()).is_some());
        }
    }

    #[test]
    fn test_parse_canonical_block_format_from_suggest_prompt() {
        // The exact block format `:ai-suggest` instructs the model to emit.
        let text = "Here are my thoughts.\n\n\
                    Suggestion:\n\
                    target: function\n\
                    action: change\n\n\
                    Suggestion:\n\
                    target: statement\n\
                    action: delete";
        let all = parse_all_action_suggestions(text);
        assert_eq!(all.len(), 2);
        assert!(all.contains(&ActionSuggestion { target: Some("function"), action: Some("change") }));
        assert!(all.contains(&ActionSuggestion { target: Some("statement"), action: Some("delete") }));
        // Suggestions carry only target/action — no stored Selection/Range/Position.
        let s = all[0];
        let _no_extra: (Option<&'static str>, Option<&'static str>) = (s.target, s.action);
        // Round-trip through canonical enums.
        assert!(crate::target::Target::from_name(s.target.unwrap()).is_some());
        assert!(crate::target::Action::from_name(s.action.unwrap()).is_some());
    }
}
