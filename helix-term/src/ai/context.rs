use helix_view::Editor;

use std::collections::HashMap;

use crate::target::Target;

/// Maximum context size in approximate tokens.
#[allow(dead_code)]
const CONTEXT_BUDGET: usize = 8000;

/// Cached import extraction keyed by file path.
///
/// The cache fingerprint is `(text length, is_modified)`. Any edit that changes
/// document length invalidates the entry; same-length edits are rare and the
/// next length-changing edit heals the cache. Diagnostics and git diff are NOT
/// cached because Helix already maintains both incrementally.
#[derive(Debug, Default)]
pub struct ContextCache {
    entries: HashMap<String, CacheEntry>,
}

#[derive(Debug, Clone)]
struct CacheEntry {
    fingerprint: (usize, bool),
    imports: Option<String>,
}

impl ContextCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Get cached imports for a document, or extract and store them.
    pub fn imports_for(
        &mut self,
        path: Option<&str>,
        text_len: usize,
        is_modified: bool,
        mut extract: impl FnMut() -> Option<String>,
    ) -> Option<String> {
        let key = match path {
            Some(p) => p.to_string(),
            None => return extract(), // scratch buffers are never cached
        };
        let fingerprint = (text_len, is_modified);

        if let Some(entry) = self.entries.get(&key) {
            if entry.fingerprint == fingerprint {
                return entry.imports.clone();
            }
        }

        let imports = extract();
        self.entries.insert(
            key,
            CacheEntry {
                fingerprint,
                imports: imports.clone(),
            },
        );
        // Bound memory: drop oldest half when oversized.
        if self.entries.len() > 128 {
            let keys: Vec<String> =
                self.entries.keys().take(64).cloned().collect();
            for k in keys {
                self.entries.remove(&k);
            }
        }
        imports
    }
}

/// Context extracted from the editor for AI requests.
#[derive(Debug, Clone)]
pub struct EditorContext {
    pub file_path: Option<String>,
    pub language: Option<String>,
    pub cursor_line: usize,
    pub cursor_col: usize,
    pub selection_text: Option<String>,
    pub has_selection: bool,
    pub target_type: Option<Target>,
    pub surrounding_code: Option<String>,
    pub imports: Option<String>,
    pub diagnostics: Option<String>,
    pub git_diff: Option<String>,
    /// Definition of the symbol under the cursor (workspace, via LSP).
    pub related_definition: Option<String>,
    /// Callers/references of the symbol under the cursor (workspace, via LSP).
    pub related_references: Option<String>,
    /// Symbols related to the definition (1-hop heuristic).
    pub related_symbols: Option<String>,
}

impl EditorContext {
    /// Extract context from the current editor state.
    pub fn from_editor(editor: &Editor) -> Self {
        let (view, doc) = helix_view::current_ref!(editor);

        let file_path = doc
            .path()
            .and_then(|p| p.to_str())
            .map(|s| s.to_string());

        let language = doc.language_id().map(|s| s.to_string());

        let text = doc.text().slice(..);
        let selection = doc.selection(view.id);
        let primary = selection.primary();

        let cursor_char = primary.cursor(text);
        let line = text.char_to_line(cursor_char);
        let line_start = text.line_to_char(line);
        let col = cursor_char - line_start;

        let selection_text = if selection.len() > 1 || primary.from() != primary.to() {
            Some(
                selection
                    .fragments(text)
                    .map(|s| s.into_owned())
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
        } else {
            None
        };

        let total_lines = text.len_lines();
        let context_start = line.saturating_sub(5);
        let context_end = (line + 6).min(total_lines);
        let surrounding_code = if context_start < context_end {
            let start_char = text.line_to_char(context_start);
            let end_char = text.line_to_char(context_end);
            Some(text.slice(start_char..end_char).to_string())
        } else {
            None
        };

        // Extract imports (first 50 lines, looking for use/import statements)
        // Consulted through the global context cache; extraction is skipped
        // when the document fingerprint is unchanged.
        let imports = {
            let path_str = file_path.as_deref();
            let text_len = text.len_chars();
            let modified = doc.is_modified();
            match super::state().as_mut() {
                Some(ai_state) => ai_state.context_cache.imports_for(
                    path_str,
                    text_len,
                    modified,
                    || extract_imports(text, &language),
                ),
                None => extract_imports(text, &language),
            }
        };

        // Extract diagnostics for the current file
        let diagnostics = extract_diagnostics(editor);

        // Extract git diff context
        let git_diff = extract_git_diff(doc);

        EditorContext {
            file_path,
            language,
            cursor_line: line,
            cursor_col: col,
            selection_text: selection_text.clone(),
            has_selection: selection_text.is_some(),
            target_type: None,
            surrounding_code,
            imports,
            diagnostics,
            git_diff,
            related_definition: None,
            related_references: None,
            related_symbols: None,
        }
    }

    /// Create context with a specific target type.
    pub fn with_target(mut self, target: Target) -> Self {
        self.target_type = Some(target);
        self
    }

    /// Attach workspace definition context.
    pub fn with_definition(mut self, definition: Option<String>) -> Self {
        self.related_definition = definition;
        self
    }

    /// Attach workspace references/callers context.
    pub fn with_references(mut self, references: Option<String>) -> Self {
        self.related_references = references;
        self
    }

    /// Attach related symbols context (1-hop).
    pub fn with_related_symbols(mut self, symbols: Option<String>) -> Self {
        self.related_symbols = symbols;
        self
    }

    /// Calculate approximate token count for context budgeting.
    #[allow(dead_code)]
    fn approximate_tokens(&self) -> usize {
        let mut tokens = 0;
        if let Some(ref t) = self.selection_text {
            tokens += t.len() / 4; // ~4 chars per token
        }
        if let Some(ref c) = self.surrounding_code {
            tokens += c.len() / 4;
        }
        if let Some(ref i) = self.imports {
            tokens += i.len() / 4;
        }
        if let Some(ref d) = self.diagnostics {
            tokens += d.len() / 4;
        }
        if let Some(ref g) = self.git_diff {
            tokens += g.len() / 4;
        }
        if let Some(ref d) = self.related_definition {
            tokens += d.len() / 4;
        }
        if let Some(ref r) = self.related_references {
            tokens += r.len() / 4;
        }
        if let Some(ref s) = self.related_symbols {
            tokens += s.len() / 4;
        }
        tokens
    }

    /// Apply context budget: remove lowest-priority context first.
    ///
    /// Priority (never removed ↁEremoved first):
    ///   1. target source (selection_text)  ENEVER removed
    ///   2. diagnostics
    ///   3. related definition
    ///   4. related references
    ///   5. imports
    ///   6. surrounding code
    ///   7. git diff
    ///   8. related symbols  Eremoved FIRST
    #[allow(dead_code)]
    fn apply_budget(&mut self) {
        while self.approximate_tokens() > CONTEXT_BUDGET {
            if self.related_symbols.is_some() {
                self.related_symbols = None;
            } else if self.git_diff.is_some() {
                self.git_diff = None;
            } else if self.surrounding_code.is_some() {
                self.surrounding_code = None;
            } else if self.imports.is_some() {
                self.imports = None;
            } else if self.related_references.is_some() {
                self.related_references = None;
            } else if self.related_definition.is_some() {
                self.related_definition = None;
            } else if self.diagnostics.is_some() {
                self.diagnostics = None;
            } else {
                break;
            }
        }
    }

    /// Format a bounded list of references for AI context.
    pub fn format_reference_list(items: &[String], max: usize) -> Option<String> {
        if items.is_empty() {
            return None;
        }
        let shown = items.iter().take(max).cloned().collect::<Vec<_>>();
        let mut out = shown.join("\n");
        if items.len() > max {
            out.push_str(&format!("\n... and {} more", items.len() - max));
        }
        Some(out)
    }

    /// Format context as a system message for the AI.
    pub fn to_system_message(&self) -> String {
        let mut parts = Vec::new();

        if let Some(path) = &self.file_path {
            parts.push(format!("File: {}", path));
        }
        if let Some(lang) = &self.language {
            parts.push(format!("Language: {}", lang));
        }
        parts.push(format!(
            "Cursor: line {}, col {}",
            self.cursor_line + 1,
            self.cursor_col + 1
        ));

        if let Some(target) = &self.target_type {
            parts.push(format!("Semantic target: {}", target.name()));
        }

        if let Some(text) = &self.selection_text {
            parts.push(format!("Selected text:\n```\n{}\n```", text));
        }

        if let Some(code) = &self.surrounding_code {
            parts.push(format!(
                "Surrounding code:\n```{}\n{}\n```",
                self.language.as_deref().unwrap_or(""),
                code
            ));
        }

        if let Some(ref imports) = self.imports {
            parts.push(format!("Imports:\n{}", imports));
        }

        if let Some(ref diagnostics) = self.diagnostics {
            parts.push(format!("Diagnostics:\n{}", diagnostics));
        }

        if let Some(ref diff) = self.git_diff {
            parts.push(format!("Git changes:\n{}", diff));
        }

        if let Some(ref def) = self.related_definition {
            parts.push(format!("Definition of current symbol:\n{}", def));
        }

        if let Some(ref refs) = self.related_references {
            parts.push(format!("Callers / references:\n{}", refs));
        }

        if let Some(ref sym) = self.related_symbols {
            parts.push(format!("Related symbols:\n{}", sym));
        }

        parts.join("\n")
    }

    /// Format context as a user message prefix.
    pub fn to_context_prefix(&self) -> String {
        let mut parts = Vec::new();

        if let Some(path) = &self.file_path {
            parts.push(format!("[File: {}]", path));
        }
        if let Some(target) = &self.target_type {
            parts.push(format!("[Target: {}]", target.name()));
        }
        if let Some(text) = &self.selection_text {
            parts.push(format!("[Selection]:\n{}", text));
        }
        if let Some(def) = &self.related_definition {
            parts.push(format!("[Definition]:\n{}", def));
        }
        if let Some(refs) = &self.related_references {
            parts.push(format!("[References]:\n{}", refs));
        }
        if let Some(sym) = &self.related_symbols {
            parts.push(format!("[Related symbols]:\n{}", sym));
        }
        if let Some(ref imports) = self.imports {
            parts.push(format!("[Imports]:\n{}", imports));
        }
        if let Some(ref diagnostics) = self.diagnostics {
            parts.push(format!("[Diagnostics]:\n{}", diagnostics));
        }
        if let Some(ref diff) = self.git_diff {
            parts.push(format!("[Git changes]:\n{}", diff));
        }

        if parts.is_empty() {
            String::new()
        } else {
            format!("{}\n\n", parts.join("\n"))
        }
    }

    /// Build a prompt for explaining the current semantic target.
    pub fn explain_prompt(&self) -> String {
        let mut prompt = String::from(
            "Explain the following code element. Be concise and actionable.\n\n",
        );
        prompt.push_str(&self.to_context_prefix());
        prompt
    }

    /// Build a prompt for reviewing the current semantic target.
    pub fn review_prompt(&self) -> String {
        let mut prompt = String::from(
            "Review this code for correctness, ownership issues, performance problems, \
             idiomatic style, and possible bugs. Be concise and actionable. \
             Do not modify files.\n",
        );

        let has_workspace = self.related_definition.is_some()
            || self.related_references.is_some();
        if has_workspace {
            prompt.push_str(
                "Consider the function itself along with its definition, its callers, \
                 diagnostics, imports, and git changes when provided.\n",
            );
        }

        prompt.push('\n');
        prompt.push_str(&self.to_context_prefix());
        prompt
    }

    /// Build a prompt for suggesting fixes.
    pub fn fix_prompt(&self) -> String {
        let mut prompt = String::from(
            "Analyze this code for issues and suggest fixes. \
             Provide: problem description, suggested patch, explanation. \
             Do not automatically apply edits.\n\n",
        );
        prompt.push_str(&self.to_context_prefix());
        prompt
    }

    /// Build a prompt for debugging compiler/runtime issues.
    pub fn debug_prompt(&self) -> String {
        let mut prompt = String::from(
            "You are debugging code. Analyze the diagnostics and explain possible causes. \
             Provide: cause analysis, explanation, suggested fix. \
             Do not automatically apply edits.\n\n",
        );
        prompt.push_str(&self.to_context_prefix());
        prompt
    }

    /// Build a prompt for generating documentation.
    pub fn doc_prompt(&self) -> String {
        let mut prompt = String::from(
            "Generate concise technical documentation for this code element. \
             Include: purpose, parameters (if function), return value, usage notes. \
             Do not insert documentation automatically.\n\n",
        );
        prompt.push_str(&self.to_context_prefix());
        prompt
    }

    /// Build a prompt for asking the AI to propose concrete, applicable
    /// Target ↁEAction improvements for the current semantic target.
    ///
    /// The AI only proposes; it never edits. Suggestions must be expressed in
    /// Tachyon's own operation language so the existing parser can recover them.
    pub fn suggest_prompt(&self, requested_action: Option<crate::target::Action>) -> String {
        let mut prompt = String::from(
            "Analyze the code element below and propose concrete, applicable \
             improvements you could make to it. Consider: the element's purpose, \
             correctness, edge cases, clarity, idiomatic style, and possible bugs. \
             Do NOT modify the file or emit any editor commands. Only describe \
             what could be done.\n\n\
             When you have a concrete proposal, write it in this exact format so it \
             can be parsed:\n\n\
             Suggestion:\n\
             target: <function|statement|expression|word|block|class|argument|line|paragraph|string|brackets>\n\
             action: <change|delete|yank|indent|outdent>\n\n\
             You may list multiple Suggestion blocks (one per proposal). \
             Use only the target and action values shown above. \
             If you find no worthwhile change, respond with your analysis but do \
             not invent a suggestion.\n\n",
        );
        if let Some(action) = requested_action {
            prompt.push_str(&format!(
                "Requested action:\n{}\n\n\
                 You MUST only propose suggestions whose action is '{}'. \
                 Do not propose any other action.\n\n",
                action.name(),
                action.name()
            ));
        }
        prompt.push_str(&self.to_context_prefix());
        prompt
    }

    /// Build a complete prompt with full context for complex analysis.
    pub fn full_context_prompt(&self) -> String {
        let mut prompt = String::from("Full code context:\n\n");
        prompt.push_str(&self.to_system_message());
        prompt
    }
}

// ============================================================
// Context extraction helpers
// ============================================================

/// Extract import statements from the beginning of a file.
fn extract_imports(text: helix_core::RopeSlice, language: &Option<String>) -> Option<String> {
    let max_lines = 50.min(text.len_lines());
    let mut imports = Vec::new();

    for i in 0..max_lines {
        let line_start = text.line_to_char(i);
        let line_end = text.line_to_char(i + 1);
        let line = text.slice(line_start..line_end).to_string();
        let trimmed = line.trim();

        let is_import = match language.as_deref() {
            Some("rust") => trimmed.starts_with("use "),
            Some("python") => {
                trimmed.starts_with("import ") || trimmed.starts_with("from ")
            }
            Some("typescript") | Some("javascript") => {
                trimmed.starts_with("import ") || trimmed.starts_with("from ")
            }
            Some("go") => trimmed.starts_with("import "),
            Some("java") => trimmed.starts_with("import "),
            Some("cpp") | Some("c") => {
                trimmed.starts_with("#include") || trimmed.starts_with("use ")
            }
            _ => trimmed.starts_with("use ")
                || trimmed.starts_with("import ")
                || trimmed.starts_with("from ")
                || trimmed.starts_with("#include"),
        };

        if is_import {
            imports.push(trimmed.to_string());
        }
    }

    if imports.is_empty() {
        None
    } else {
        Some(imports.join("\n"))
    }
}

/// Extract diagnostics for the current file from the editor.
fn extract_diagnostics(editor: &Editor) -> Option<String> {
    let (_view, doc) = helix_view::current_ref!(editor);
    let diags = doc.diagnostics();

    if diags.is_empty() {
        return None;
    }

    let mut diag_strings = Vec::new();
    for diag in diags.iter().take(10) {
        // Limit to 10 diagnostics
        let severity = match diag.severity {
            Some(helix_core::diagnostic::Severity::Error) => "Error",
            Some(helix_core::diagnostic::Severity::Warning) => "Warning",
            Some(helix_core::diagnostic::Severity::Hint) => "Hint",
            Some(helix_core::diagnostic::Severity::Info) => "Info",
            None => "Unknown",
        };
        diag_strings.push(format!(
            "{} at line {}: {}",
            severity,
            diag.line + 1,
            diag.message
        ));
    }

    if diag_strings.is_empty() {
        None
    } else {
        Some(diag_strings.join("\n"))
    }
}

/// Extract git diff context for the current file.
fn extract_git_diff(doc: &helix_view::Document) -> Option<String> {
    let diff_handle = doc.diff_handle()?;
    let diff = diff_handle.load();
    let hunks = diff.len();

    if hunks == 0 {
        return None;
    }

    let mut diff_lines = Vec::new();
    let text = doc.text().slice(..);

    for i in 0..hunks.min(20) {
        // Limit to 20 hunks
        let hunk = diff.nth_hunk(i);
        let start = text.line_to_char(hunk.after.start as usize);
        let end = text.line_to_char(hunk.after.end as usize);
        let hunk_text = text.slice(start..end).to_string();

        for line in hunk_text.lines() {
            diff_lines.push(format!("+ {}", line));
        }
    }

    if diff_lines.is_empty() {
        None
    } else {
        Some(diff_lines.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_context_cache_hit() {
        let mut cache = ContextCache::new();
        let mut calls = 0;
        let mut extract = || {
            calls += 1;
            Some("use std::sync::Arc;".to_string())
        };

        let first = cache.imports_for(Some("src/a.rs"), 100, false, &mut extract);
        assert_eq!(first, Some("use std::sync::Arc;".to_string()));

        // Same fingerprint ↁEcache hit, extractor not called again
        let second = cache.imports_for(Some("src/a.rs"), 100, false, &mut extract);
        assert_eq!(second, Some("use std::sync::Arc;".to_string()));
        assert_eq!(calls, 1);
    }

    #[test]
    fn test_context_cache_invalidate_on_length_change() {
        let mut cache = ContextCache::new();
        let mut calls = 0;
        let mut extract = || {
            calls += 1;
            Some(format!("import #{}", calls))
        };

        cache.imports_for(Some("a.py"), 50, false, &mut extract);
        // Length changed ↁEre-extract
        let result = cache.imports_for(Some("a.py"), 60, false, &mut extract);
        assert_eq!(result, Some("import #2".to_string()));
        assert_eq!(calls, 2);
    }

    #[test]
    fn test_context_cache_invalidate_on_modified() {
        let mut cache = ContextCache::new();
        let mut calls = 0;
        let mut extract = || {
            calls += 1;
            None
        };

        cache.imports_for(Some("x.rs"), 10, false, &mut extract);
        cache.imports_for(Some("x.rs"), 10, true, &mut extract);
        assert_eq!(calls, 2);
    }

    #[test]
    fn test_context_cache_never_caches_scratch() {
        let mut cache = ContextCache::new();
        let mut calls = 0;
        let mut extract = || {
            calls += 1;
            Some("import x".to_string())
        };

        cache.imports_for(None, 10, false, &mut extract);
        cache.imports_for(None, 10, false, &mut extract);
        // No path ↁEalways extract, never cached
        assert_eq!(calls, 2);
    }

    #[test]
    fn test_target_name_key_roundtrip() {
        let targets = [
            Target::Word, Target::Line, Target::Expression, Target::Statement,
            Target::Function, Target::Block, Target::Class, Target::Paragraph,
            Target::Argument, Target::Brackets, Target::All,
        ];
        for target in &targets {
            // name ↁEfrom_name ↁEsame target
            assert_eq!(Target::from_name(target.name()), Some(*target));
            // key ↁEfrom_key ↁEsame target (String target uses '"' key)
            if *target != Target::String {
                assert_eq!(Target::from_key(target.key()), Some(*target));
            }
        }
    }

    fn test_context() -> EditorContext {
        EditorContext {
            file_path: Some("src/main.rs".to_string()),
            language: Some("Rust".to_string()),
            cursor_line: 10,
            cursor_col: 5,
            selection_text: Some("fn hello() {}".to_string()),
            has_selection: true,
            target_type: Some(Target::Function),
            surrounding_code: Some("fn main() {\n    hello();\n}".to_string()),
            imports: Some("use std::sync::Arc;".to_string()),
            diagnostics: Some("Warning at line 5: unused variable".to_string()),
            git_diff: None,
            related_definition: None,
            related_references: None,
            related_symbols: None,
        }
    }

    #[test]
    fn test_context_to_system_message() {
        let ctx = test_context();
        let msg = ctx.to_system_message();
        assert!(msg.contains("File: src/main.rs"));
        assert!(msg.contains("Language: Rust"));
        assert!(msg.contains("line 11, col 6"));
        assert!(msg.contains("fn hello() {}"));
        assert!(msg.contains("Semantic target: function"));
        assert!(msg.contains("Surrounding code:"));
        assert!(msg.contains("Imports:"));
        assert!(msg.contains("use std::sync::Arc;"));
        assert!(msg.contains("Diagnostics:"));
        assert!(msg.contains("Warning at line 5"));
    }

    #[test]
    fn test_context_to_system_message_no_selection() {
        let ctx = EditorContext {
            file_path: Some("test.py".to_string()),
            language: Some("Python".to_string()),
            cursor_line: 0,
            cursor_col: 0,
            selection_text: None,
            has_selection: false,
            target_type: None,
            surrounding_code: None,
            imports: None,
            diagnostics: None,
            git_diff: None,
            related_definition: None,
            related_references: None,
            related_symbols: None,
        };

        let msg = ctx.to_system_message();
        assert!(msg.contains("File: test.py"));
        assert!(!msg.contains("Selected text"));
        assert!(!msg.contains("Semantic target"));
        assert!(!msg.contains("Imports:"));
        assert!(!msg.contains("Diagnostics:"));
    }

    #[test]
    fn test_context_to_context_prefix_with_all() {
        let ctx = test_context();
        let prefix = ctx.to_context_prefix();
        assert!(prefix.contains("[File: src/main.rs]"));
        assert!(prefix.contains("[Target: function]"));
        assert!(prefix.contains("[Selection]:"));
        assert!(prefix.contains("[Imports]:"));
        assert!(prefix.contains("[Diagnostics]:"));
    }

    #[test]
    fn test_explain_prompt() {
        let ctx = test_context();
        let prompt = ctx.explain_prompt();
        assert!(prompt.contains("Explain the following code element"));
        assert!(prompt.contains("[Target: function]"));
    }

    #[test]
    fn test_review_prompt() {
        let ctx = test_context();
        let prompt = ctx.review_prompt();
        assert!(prompt.contains("Review this code"));
        assert!(prompt.contains("correctness"));
        assert!(prompt.contains("ownership issues"));
    }

    #[test]
    fn test_fix_prompt() {
        let ctx = test_context();
        let prompt = ctx.fix_prompt();
        assert!(prompt.contains("suggest fixes"));
        assert!(prompt.contains("Do not automatically apply"));
    }

    #[test]
    fn test_debug_prompt() {
        let ctx = test_context();
        let prompt = ctx.debug_prompt();
        assert!(prompt.contains("debugging code"));
        assert!(prompt.contains("diagnostics"));
        assert!(prompt.contains("Do not automatically apply"));
    }

    #[test]
    fn test_doc_prompt() {
        let ctx = test_context();
        let prompt = ctx.doc_prompt();
        assert!(prompt.contains("Generate concise technical documentation"));
        assert!(prompt.contains("Do not insert documentation"));
    }

    #[test]
    fn test_suggest_prompt() {
        let ctx = test_context();
        let prompt = ctx.suggest_prompt(None);
        // Instructs the AI to use Tachyon's own Target -> Action language.
        assert!(prompt.contains("Suggestion:"));
        assert!(prompt.contains("target:"));
        assert!(prompt.contains("action:"));
        // Lists the canonical target/action vocabulary.
        assert!(prompt.contains("function"));
        assert!(prompt.contains("statement"));
        assert!(prompt.contains("expression"));
        assert!(prompt.contains("change"));
        assert!(prompt.contains("delete"));
        // The AI must not edit or emit editor commands.
        assert!(prompt.contains("Do NOT modify the file"));
        // The current target + context are included via the prefix.
        assert!(prompt.contains("[Target: function]"));
        assert!(prompt.contains("fn hello() {}"));
    }

    #[test]
    fn test_suggest_prompt_includes_specified_target() {
        // A non-default target (Expression) must be reflected in the prompt so
        // the AI analyzes the correct element.
        let mut ctx = test_context();
        ctx.target_type = Some(Target::Expression);
        let prompt = ctx.suggest_prompt(None);
        assert!(prompt.contains("[Target: expression]"));
        assert!(!prompt.contains("[Target: function]"));
    }

    #[test]
    fn test_suggest_prompt_action_constraint() {
        let ctx = test_context();
        // No constraint → no "Requested action" block, base prompt intact.
        let none = ctx.suggest_prompt(None);
        assert!(!none.contains("Requested action:"));
        assert!(none.contains("Do NOT modify the file"));

        // Change constraint → prompt names the canonical action.
        let constrained = ctx.suggest_prompt(Some(crate::target::Action::Change));
        assert!(constrained.contains("Requested action:"));
        assert!(constrained.contains("change"));
        assert!(constrained.contains("You MUST only propose suggestions whose action is 'change'"));
        assert!(constrained.contains("Do NOT modify the file"));
    }

    #[test]
    fn test_full_context_prompt() {
        let ctx = test_context();
        let prompt = ctx.full_context_prompt();
        assert!(prompt.contains("Full code context:"));
        assert!(prompt.contains("File: src/main.rs"));
    }

    #[test]
    fn test_with_target() {
        let ctx = EditorContext {
            file_path: None,
            language: None,
            cursor_line: 0,
            cursor_col: 0,
            selection_text: None,
            has_selection: false,
            target_type: None,
            surrounding_code: None,
            imports: None,
            diagnostics: None,
            git_diff: None,
            related_definition: None,
            related_references: None,
            related_symbols: None,
        };
        let ctx = ctx.with_target(Target::Expression);
        assert_eq!(ctx.target_type, Some(Target::Expression));
    }

    #[test]
    fn test_approximate_tokens() {
        let ctx = EditorContext {
            file_path: None,
            language: None,
            cursor_line: 0,
            cursor_col: 0,
            selection_text: Some("hello world".to_string()), // ~2 tokens
            has_selection: true,
            target_type: None,
            surrounding_code: None,
            imports: None,
            diagnostics: None,
            git_diff: None,
            related_definition: None,
            related_references: None,
            related_symbols: None,
        };
        assert_eq!(ctx.approximate_tokens(), 2); // 11 chars / 4 = 2
    }

    #[test]
    fn test_context_budget_removes_low_priority() {
        let mut ctx = EditorContext {
            file_path: None,
            language: None,
            cursor_line: 0,
            cursor_col: 0,
            selection_text: Some("x".repeat(40000)), // 10000 tokens
            has_selection: true,
            target_type: None,
            surrounding_code: Some("y".repeat(40000)),
            imports: Some("z".repeat(40000)),
            diagnostics: None,
            git_diff: Some("w".repeat(40000)),
            related_definition: None,
            related_references: None,
            related_symbols: None,
        };
        ctx.apply_budget();
        // selection_text should remain (highest priority)
        assert!(ctx.selection_text.is_some());
        // git_diff should be removed first (lowest priority)
        assert!(ctx.git_diff.is_none());
    }

    #[test]
    fn test_context_prefix_empty() {
        let ctx = EditorContext {
            file_path: None,
            language: None,
            cursor_line: 0,
            cursor_col: 0,
            selection_text: None,
            has_selection: false,
            target_type: None,
            surrounding_code: None,
            imports: None,
            diagnostics: None,
            git_diff: None,
            related_definition: None,
            related_references: None,
            related_symbols: None,
        };
        let prefix = ctx.to_context_prefix();
        assert!(prefix.is_empty());
    }

    #[test]
    fn test_context_with_git_diff() {
        let ctx = EditorContext {
            file_path: Some("src/lib.rs".to_string()),
            language: Some("Rust".to_string()),
            cursor_line: 5,
            cursor_col: 0,
            selection_text: Some("fn foo() {}".to_string()),
            has_selection: true,
            target_type: Some(Target::Function),
            surrounding_code: None,
            imports: None,
            diagnostics: None,
            git_diff: Some("+ added lock\n- removed direct access".to_string()),
            related_definition: None,
            related_references: None,
            related_symbols: None,
        };
        let msg = ctx.to_system_message();
        assert!(msg.contains("Git changes:"));
        assert!(msg.contains("+ added lock"));
        assert!(msg.contains("- removed direct access"));
    }

    #[test]
    fn test_debug_prompt_with_diagnostics() {
        let ctx = EditorContext {
            file_path: Some("src/main.rs".to_string()),
            language: Some("Rust".to_string()),
            cursor_line: 10,
            cursor_col: 0,
            selection_text: Some("let x: i32 = \"hello\";".to_string()),
            has_selection: true,
            target_type: Some(Target::Statement),
            surrounding_code: None,
            imports: None,
            diagnostics: Some("Error at line 10: mismatched types".to_string()),
            git_diff: None,
            related_definition: None,
            related_references: None,
            related_symbols: None,
        };
        let prompt = ctx.debug_prompt();
        assert!(prompt.contains("debugging code"));
        assert!(prompt.contains("[Diagnostics]:"));
        assert!(prompt.contains("mismatched types"));
    }

    #[test]
    fn test_doc_prompt_with_function() {
        let ctx = EditorContext {
            file_path: Some("src/utils.rs".to_string()),
            language: Some("Rust".to_string()),
            cursor_line: 20,
            cursor_col: 0,
            selection_text: Some("pub fn calculate(x: i32) -> i32 { x * 2 }".to_string()),
            has_selection: true,
            target_type: Some(Target::Function),
            surrounding_code: None,
            imports: Some("use std::collections::HashMap;".to_string()),
            diagnostics: None,
            git_diff: None,
            related_definition: None,
            related_references: None,
            related_symbols: None,
        };
        let prompt = ctx.doc_prompt();
        assert!(prompt.contains("Generate concise technical documentation"));
        assert!(prompt.contains("pub fn calculate"));
        assert!(prompt.contains("[Imports]:"));
    }

    // ============================================================
    // Phase 20: Workspace Symbol Context tests
    // ============================================================

    fn workspace_context() -> EditorContext {
        EditorContext {
            file_path: Some("src/auth.rs".to_string()),
            language: Some("Rust".to_string()),
            cursor_line: 10,
            cursor_col: 4,
            selection_text: Some("fn authenticate(user: User) -> Result<Session> {}".to_string()),
            has_selection: true,
            target_type: Some(Target::Function),
            surrounding_code: None,
            imports: None,
            diagnostics: None,
            git_diff: None,
            related_definition: Some(
                "struct User {\n    id: u64,\n    name: String,\n}".to_string(),
            ),
            related_references: Some("login()\nrefresh_session()".to_string()),
            related_symbols: Some("User\nSession".to_string()),
        }
    }

    #[test]
    fn test_workspace_context_in_prefix() {
        let ctx = workspace_context();
        let prefix = ctx.to_context_prefix();
        assert!(prefix.contains("[Definition]:"));
        assert!(prefix.contains("struct User"));
        assert!(prefix.contains("[References]:"));
        assert!(prefix.contains("login()"));
        assert!(prefix.contains("[Related symbols]:"));
        assert!(prefix.contains("Session"));
    }

    #[test]
    fn test_workspace_context_in_system_message() {
        let ctx = workspace_context();
        let msg = ctx.to_system_message();
        assert!(msg.contains("Definition of current symbol:"));
        assert!(msg.contains("Callers / references:"));
        assert!(msg.contains("Related symbols:"));
    }

    #[test]
    fn test_empty_workspace_fields_produce_valid_prompt() {
        // No LSP data at all ↁEprompt still valid, no workspace sections
        let ctx = EditorContext {
            file_path: None,
            language: None,
            cursor_line: 0,
            cursor_col: 0,
            selection_text: Some("fn foo() {}".to_string()),
            has_selection: true,
            target_type: Some(Target::Function),
            surrounding_code: None,
            imports: None,
            diagnostics: None,
            git_diff: None,
            related_definition: None,
            related_references: None,
            related_symbols: None,
        };
        let prompt = ctx.review_prompt();
        assert!(prompt.contains("Review this code"));
        assert!(!prompt.contains("[Definition]"));
        assert!(!prompt.contains("[References]"));
        // No workspace hint line when no workspace context present
        assert!(!prompt.contains("its callers"));
    }

    #[test]
    fn test_review_prompt_mentions_callers_with_workspace() {
        let mut ctx = EditorContext {
            file_path: None,
            language: None,
            cursor_line: 0,
            cursor_col: 0,
            selection_text: Some("fn foo() {}".to_string()),
            has_selection: true,
            target_type: Some(Target::Function),
            surrounding_code: None,
            imports: None,
            diagnostics: None,
            git_diff: None,
            related_definition: None,
            related_references: None,
            related_symbols: None,
        };
        ctx.related_references = Some("caller_a()\ncaller_b()".to_string());
        let prompt = ctx.review_prompt();
        assert!(prompt.contains("its definition, its callers"));
        assert!(prompt.contains("[References]:"));
        assert!(prompt.contains("caller_a()"));
    }

    #[test]
    fn test_budget_removes_related_symbols_first() {
        // Totals ~10500 tokens: removal order = symbols(1500) then git_diff(2000)
        // brings us to ~7000 tokens, so higher-priority items survive.
        let mut ctx = EditorContext {
            file_path: None,
            language: None,
            cursor_line: 0,
            cursor_col: 0,
            selection_text: Some("x".repeat(20000)), // ~5000 tokens
            has_selection: true,
            target_type: None,
            surrounding_code: Some("y".repeat(8000)), // ~2000 tokens
            imports: None,
            diagnostics: None,
            git_diff: Some("d".repeat(8000)), // ~2000 tokens
            related_definition: Some("def ".repeat(40)), // ~160 chars ≁E40 tokens
            related_references: Some("refs ".repeat(40)), // ~200 chars ≁E50 tokens
            related_symbols: Some("sym".repeat(6000)),   // ~1500 tokens
        };
        ctx.apply_budget();
        // Target source never removed
        assert!(ctx.selection_text.is_some());
        // Lowest priority removed first
        assert!(ctx.related_symbols.is_none());
        assert!(ctx.git_diff.is_none());
        // Higher-priority items survive
        assert!(ctx.related_definition.is_some());
        assert!(ctx.related_references.is_some());
        assert!(ctx.surrounding_code.is_some());
    }

    #[test]
    fn test_budget_strips_everything_except_target_when_target_alone_exceeds() {
        let mut ctx = EditorContext {
            file_path: None,
            language: None,
            cursor_line: 0,
            cursor_col: 0,
            selection_text: Some("x".repeat(40000)), // ~10000 tokens, cannot fit
            has_selection: true,
            target_type: None,
            surrounding_code: None,
            imports: None,
            diagnostics: None,
            git_diff: None,
            related_definition: Some("d".to_string()),
            related_references: Some("r".to_string()),
            related_symbols: Some("s".to_string()),
        };
        ctx.apply_budget();
        // Everything removable is removed; only target source remains
        assert!(ctx.selection_text.is_some());
        assert!(ctx.related_definition.is_none());
        assert!(ctx.related_references.is_none());
        assert!(ctx.related_symbols.is_none());
    }

    #[test]
    fn test_format_reference_list_limits_and_counts() {
        let refs: Vec<String> = (1..=15).map(|i| format!("caller_{}()", i)).collect();

        let formatted = EditorContext::format_reference_list(&refs, 10).unwrap();
        assert!(formatted.contains("caller_1()"));
        assert!(formatted.contains("caller_10()"));
        assert!(!formatted.contains("caller_11()"));
        assert!(formatted.contains("... and 5 more"));
    }

    #[test]
    fn test_format_reference_list_empty_is_none() {
        let refs: Vec<String> = vec![];
        assert!(EditorContext::format_reference_list(&refs, 10).is_none());
    }

    #[test]
    fn test_builders_attach_workspace_context() {
        let ctx = EditorContext {
            file_path: None,
            language: None,
            cursor_line: 0,
            cursor_col: 0,
            selection_text: None,
            has_selection: false,
            target_type: None,
            surrounding_code: None,
            imports: None,
            diagnostics: None,
            git_diff: None,
            related_definition: None,
            related_references: None,
            related_symbols: None,
        }
        .with_definition(Some("struct S;".to_string()))
        .with_references(Some("main()".to_string()))
        .with_related_symbols(None);

        assert_eq!(ctx.related_definition.as_deref(), Some("struct S;"));
        assert_eq!(ctx.related_references.as_deref(), Some("main()"));
        assert!(ctx.related_symbols.is_none());
    }
}
