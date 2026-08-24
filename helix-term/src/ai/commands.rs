use anyhow::{Context, Result};

use super::client::AiClient;
use super::config::AiConfig;
use super::context::EditorContext;
use super::credential::AiCredentials;
use super::provider::{AiMessage, AiModel, AiProviderKind};
use crate::compositor;
use crate::job;
use crate::target::Target;
use crate::ui::PromptEvent;

use futures_util::stream::FuturesUnordered;
use helix_core::syntax::config::LanguageServerFeature;
use helix_lsp::lsp;
use tokio_stream::StreamExt;

/// Shared AI state accessible from commands.
pub struct AiState {
    pub credentials: AiCredentials,
    pub client: AiClient,
    pub config: AiConfig,
    /// Cache for expensive context extraction (imports), keyed by file path.
    pub context_cache: super::context::ContextCache,
    /// Suggestions from the most recent AI response (Target + Action pairs).
    /// Advisory only — applied solely via explicit user command (`:ai-apply`).
    /// Never stores selections or ranges; targets re-resolve at apply time.
    pub last_suggestions: Vec<super::response::ActionSuggestion>,
}

impl Default for AiState {
    fn default() -> Self {
        Self {
            credentials: AiCredentials::new(),
            client: AiClient::new().expect("failed to create HTTP client"),
            config: AiConfig::default(),
            context_cache: super::context::ContextCache::new(),
            last_suggestions: Vec::new(),
        }
    }
}

impl AiState {
    pub fn new() -> Result<Self> {
        Ok(Self {
            credentials: AiCredentials::new(),
            client: AiClient::new()?,
            config: AiConfig::default(),
            context_cache: super::context::ContextCache::new(),
            last_suggestions: Vec::new(),
        })
    }

    pub fn new_with_credentials(credentials: AiCredentials) -> Self {
        Self {
            credentials,
            client: AiClient::new().expect("failed to create HTTP client"),
            config: AiConfig::default(),
            context_cache: super::context::ContextCache::new(),
            last_suggestions: Vec::new(),
        }
    }
}

// ============================================================
// :ai-connect <provider> [api_key]
// ============================================================

pub fn ai_connect(
    cx: &mut compositor::Context,
    args: helix_core::command_line::Args,
    _event: PromptEvent,
) -> Result<()> {
    let provider_name = args
        .first()
        .context("Usage: :ai-connect <zen|go> [api_key]")?;

    let provider = match provider_name.to_lowercase().as_str() {
        "zen" | "opencode-zen" => AiProviderKind::OpenCodeZen,
        "go" | "opencode-go" => AiProviderKind::OpenCodeGo,
        _ => {
            anyhow::bail!("Unknown provider: '{}'. Available: zen, go", provider_name);
        }
    };

    if let Some(api_key) = args.get(1) {
        let api_key = api_key.to_string();
        {
            let mut ai_state = super::state().context("AI state not initialized")?;
            ai_state.credentials.set_key(provider, api_key);
            ai_state.config.provider = Some(provider);
            if let Err(e) = ai_state.credentials.save() {
                log::warn!("Failed to save AI credentials: {}", e);
            }
        }

        let (client, credentials) = {
            let ai_state = super::state().unwrap();
            (ai_state.client.clone(), ai_state.credentials.clone())
        };

        let callback = async move {
            match client.fetch_models(provider, &credentials).await {
                Ok(models) => {
                    let count = models.len();
                    let cb: job::Callback = job::Callback::Editor(Box::new(move |editor| {
                        editor.set_status(format!(
                            "Connected to {}. Found {} models.",
                            provider, count
                        ));
                    }));
                    Ok(cb)
                }
                Err(e) => {
                    let msg = format!("{} connection failed: {}", provider, e);
                    let cb: job::Callback =
                        job::Callback::Editor(Box::new(move |editor| {
                            editor.set_error(msg);
                        }));
                    Ok(cb)
                }
            }
        };
        cx.jobs.callback(callback);
    } else {
        cx.editor.set_status(format!(
            "Usage: :ai-connect {} <api_key>",
            provider_name
        ));
    }

    Ok(())
}

// ============================================================
// :ai-model — Opens a fuzzy picker for model selection
// ============================================================

pub fn ai_model(
    cx: &mut compositor::Context,
    _args: helix_core::command_line::Args,
    _event: PromptEvent,
) -> Result<()> {
    let (provider, client, credentials) = {
        let ai_state = super::state().context("AI state not initialized")?;
        let provider = ai_state
            .config
            .provider
            .context("No AI provider configured. Use :ai-connect first.")?;
        if !ai_state.credentials.has_key(provider) {
            anyhow::bail!(
                "{} API key is not configured. Use :ai-connect {}",
                provider,
                match provider {
                    AiProviderKind::OpenCodeZen => "zen",
                    AiProviderKind::OpenCodeGo => "go",
                }
            );
        }
        (provider, ai_state.client.clone(), ai_state.credentials.clone())
    };

    let callback = async move {
        match client.fetch_models(provider, &credentials).await {
            Ok(models) => {
                let cb: job::Callback =
                    job::Callback::EditorCompositor(Box::new(move |editor, compositor| {
                        if models.is_empty() {
                            editor.set_error("No models available from provider");
                            return;
                        }

                        let columns = [crate::ui::PickerColumn::new(
                            "model",
                            |item: &AiModel, _| item.id.as_str().into(),
                        )];

                        let current_model = super::state()
                            .and_then(|s| s.config.model.clone());

                        let initial_cursor = current_model.as_ref().and_then(|current| {
                            models.iter().position(|m| m.id == *current)
                        });

                        let picker = crate::ui::Picker::new(
                            columns,
                            0,
                            models,
                            (),
                            |cx, model, _action| {
                                let model_id = model.id.clone();
                                if let Some(mut ai_state) = super::state() {
                                    ai_state.config.model = Some(model_id.clone());
                                }
                                cx.editor.set_status(format!("AI model: {}", model_id));
                            },
                        )
                        .truncate_start(false);

                        let picker = if let Some(cursor) = initial_cursor {
                            picker.with_initial_cursor(cursor as u32)
                        } else {
                            picker
                        };

                        use crate::ui::overlay::overlaid;
                        compositor.push(Box::new(overlaid(picker)));
                    }));
                Ok(cb)
            }
            Err(e) => {
                let msg = format!("Failed to fetch models: {}", e);
                let cb: job::Callback =
                    job::Callback::Editor(Box::new(move |editor| {
                        editor.set_error(msg);
                    }));
                Ok(cb)
            }
        }
    };

    cx.jobs.callback(callback);
    Ok(())
}

// ============================================================
// :ai-set-model <model>
// ============================================================

pub fn ai_set_model(
    cx: &mut compositor::Context,
    args: helix_core::command_line::Args,
    _event: PromptEvent,
) -> Result<()> {
    let model_name = args
        .first()
        .context("Usage: :ai-set-model <model_name>")?;

    {
        let mut ai_state = super::state().context("AI state not initialized")?;
        if ai_state.config.provider.is_none() {
            anyhow::bail!("No AI provider configured. Use :ai-connect first.");
        }
        ai_state.config.model = Some(model_name.to_string());
    }

    cx.editor
        .set_status(format!("AI model: {}", model_name));
    Ok(())
}

// ============================================================
// :ai <prompt>
// ============================================================

pub fn ai_prompt(
    cx: &mut compositor::Context,
    args: helix_core::command_line::Args,
    _event: PromptEvent,
) -> Result<()> {
    let prompt_text = args.join(" ");
    if prompt_text.is_empty() {
        anyhow::bail!("Usage: :ai <prompt>");
    }

    let (provider, _model, client, credentials, config) = {
        let ai_state = super::state().context("AI state not initialized")?;
        let provider = ai_state
            .config
            .provider
            .context("No AI provider configured. Use :ai-connect first.")?;
        let model = ai_state
            .config
            .model
            .as_deref()
            .context("No model selected. Use :ai-model to select one.")?
            .to_string();
        if !ai_state.credentials.has_key(provider) {
            anyhow::bail!(
                "{} API key is not configured. Use :ai-connect {}",
                provider,
                match provider {
                    AiProviderKind::OpenCodeZen => "zen",
                    AiProviderKind::OpenCodeGo => "go",
                }
            );
        }
        (
            provider,
            model,
            ai_state.client.clone(),
            ai_state.credentials.clone(),
            ai_state.config.clone(),
        )
    };

    let context = EditorContext::from_editor(cx.editor);
    let context_prefix = context.to_context_prefix();
    let full_prompt = format!("{}{}", context_prefix, prompt_text);

    let provider_name = provider.display_name().to_string();
    cx.editor
        .set_status(format!("{}: thinking...", provider_name));

    let callback = async move {
        let messages = vec![AiMessage {
            role: "user".to_string(),
            content: full_prompt,
        }];

        match client.chat(provider, &credentials, &config, messages).await {
            Ok(response_text) => {
                let cb: job::Callback =
                    job::Callback::EditorCompositor(Box::new(move |editor, compositor| {
                        editor.set_status(format!(
                            "{}: response received ({} chars)",
                            provider_name,
                            response_text.len()
                        ));

                        // Create a scrollable response viewer using Popup.
                        // Markdown rendering gives syntax-highlighted code blocks.
                        let markdown =
                            crate::ui::Markdown::new(response_text, editor.syn_loader.clone());
                        let popup = crate::ui::Popup::new("ai-response", markdown)
                            .with_scrollbar(true)
                            .auto_close(true);

                        use crate::ui::overlay::overlaid;
                        compositor.push(Box::new(overlaid(popup)));
                    }));
                Ok(cb)
            }
            Err(e) => {
                let msg = format!("{}: {}", provider_name, e);
                let cb: job::Callback =
                    job::Callback::Editor(Box::new(move |editor| {
                        editor.set_error(msg);
                    }));
                Ok(cb)
            }
        }
    };

    cx.jobs.callback(callback);
    Ok(())
}

// ============================================================
// :ai-status
// ============================================================

pub fn ai_status(
    cx: &mut compositor::Context,
    _args: helix_core::command_line::Args,
    _event: PromptEvent,
) -> Result<()> {
    let ai_state = super::state().context("AI state not initialized")?;

    let provider_str = ai_state
        .config
        .provider
        .map(|p| p.display_name().to_string())
        .unwrap_or_else(|| "not configured".to_string());

    let model_str = ai_state
        .config
        .model
        .clone()
        .unwrap_or_else(|| "not selected".to_string());

    let key_status = match ai_state.config.provider {
        Some(p) => {
            if ai_state.credentials.has_key(p) {
                "configured".to_string()
            } else {
                "not configured".to_string()
            }
        }
        None => "no provider".to_string(),
    };

    cx.editor.set_status(format!(
        "AI provider: {} | AI model: {} | API key: {}",
        provider_str, model_str, key_status
    ));
    Ok(())
}

// ============================================================
// Helper: resolve pending target and build context
// ============================================================

fn resolve_target_context(
    cx: &compositor::Context,
    target: crate::target::Target,
) -> EditorContext {
    let (view, doc) = helix_view::current_ref!(cx.editor);
    let syn_loader = cx.editor.syn_loader.clone();
    let selection = target.resolve(doc, view, &syn_loader);

    // Build context with the resolved target text
    let text = doc.text().slice(..);
    let primary = selection.primary();
    let selected_text: String = primary.fragment(text).into_owned();

    let file_path = doc
        .path()
        .and_then(|p| p.to_str())
        .map(|s| s.to_string());
    let language = doc.language_id().map(|s| s.to_string());

    let cursor_char = primary.cursor(text);
    let line = text.char_to_line(cursor_char);
    let line_start = text.line_to_char(line);
    let col = cursor_char - line_start;

    // Surrounding context
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

    EditorContext {
        file_path,
        language,
        cursor_line: line,
        cursor_col: col,
        selection_text: Some(selected_text),
        has_selection: true,
        target_type: Some(target),
        surrounding_code,
        imports: None,
        diagnostics: None,
        git_diff: None,
        related_definition: None,
        related_references: None,
        related_symbols: None,
    }
}

/// Build the shared display callback for a completed AI response:
/// status line with navigation + action suggestions, Markdown popup.
fn response_display_callback(provider_name: String, response_text: String) -> job::Callback {
    job::Callback::EditorCompositor(Box::new(move |editor, compositor| {
        // Store suggestions for later user activation via :ai-apply.
        // Advisory only — nothing is executed automatically.
        let suggestions = super::response::parse_all_action_suggestions(&response_text);
        if let Some(mut ai_state) = super::state() {
            ai_state.last_suggestions = suggestions.clone();
        }

        // Extract navigation suggestions from the response
        let location_suggestions =
            super::response::parse_location_suggestions(&response_text, 1);
        let location_hint = match location_suggestions.first() {
            Some(s) => match s.line {
                Some(line) => format!(" [see {}:{}]", s.path, line),
                None => format!(" [see {}]", s.path),
            },
            None => String::new(),
        };

        // Extract Target → Action suggestion (advisory only)
        let action_hint = suggestions
            .first()
            .map(|s| format!(" {}", super::response::format_action_suggestion(s)))
            .unwrap_or_default();

        editor.set_status(format!(
            "{}: response received ({} chars){}{}",
            provider_name,
            response_text.len(),
            location_hint,
            action_hint
        ));

        // Markdown rendering gives syntax-highlighted code blocks.
        let markdown = crate::ui::Markdown::new(response_text, editor.syn_loader.clone());
        let popup = crate::ui::Popup::new("ai-response", markdown)
            .with_scrollbar(true)
            .auto_close(true);

        use crate::ui::overlay::overlaid;
        compositor.push(Box::new(overlaid(popup)));
    }))
}

/// Display callback for `:ai-suggest`: store recovered suggestions and report
/// the count. Advisory only — nothing is applied automatically.
/// Keep only suggestions whose canonical `Action` matches `requested`.
/// Comparison is by canonical `Action` enum (via `Action::from_name`), never by
/// raw strings. Unknown actions (parser yields `None`) do not survive.
fn filter_suggestions_by_action(
    suggestions: Vec<super::response::ActionSuggestion>,
    requested: Option<crate::target::Action>,
) -> Vec<super::response::ActionSuggestion> {
    match requested {
        None => suggestions,
        Some(req) => suggestions
            .into_iter()
            .filter(|s| s.action.and_then(crate::target::Action::from_name) == Some(req))
            .collect(),
    }
}

fn suggest_response_callback(
    provider_name: String,
    response_text: String,
    requested_action: Option<crate::target::Action>,
) -> job::Callback {
    job::Callback::EditorCompositor(Box::new(move |editor, compositor| {
        // Store suggestions for later user activation via :ai-apply.
        // Advisory only — nothing is executed automatically.
        let parsed = super::response::parse_all_action_suggestions(&response_text);
        let suggestions = filter_suggestions_by_action(parsed, requested_action);

        if let Some(mut ai_state) = super::state() {
            ai_state.last_suggestions = suggestions.clone();
        }

        let count = suggestions.len();
        let status = match count {
            0 => match requested_action {
                Some(a) => format!(
                    "{}: no matching '{}' suggestions available.",
                    provider_name,
                    a.name()
                ),
                None => format!("{}: no actionable suggestions found.", provider_name),
            },
            1 => format!(
                "{}: 1 matching suggestion available. Use :ai-apply to apply.",
                provider_name
            ),
            n => format!(
                "{}: {} matching suggestions available. Use :ai-apply to review.",
                provider_name, n
            ),
        };
        editor.set_status(status);

        // Markdown rendering gives syntax-highlighted code blocks.
        let markdown = crate::ui::Markdown::new(response_text, editor.syn_loader.clone());
        let popup = crate::ui::Popup::new("ai-response", markdown)
            .with_scrollbar(true)
            .auto_close(true);

        use crate::ui::overlay::overlaid;
        compositor.push(Box::new(overlaid(popup)));
    }))
}

/// Shared helper to send an AI request (streaming) and display the response.
fn send_ai_request(
    cx: &mut compositor::Context,
    prompt: String,
    provider_name: String,
) -> Result<()> {
    let (client, credentials, config, provider) = {
        let ai_state = super::state().context("AI state not initialized")?;
        let provider = ai_state
            .config
            .provider
            .context("No AI provider configured. Use :ai-connect first.")?;
        let _model = ai_state
            .config
            .model
            .as_deref()
            .context("No model selected. Use :ai-model to select one.")?
            .to_string();
        if !ai_state.credentials.has_key(provider) {
            anyhow::bail!(
                "{} API key is not configured. Use :ai-connect {}",
                provider,
                match provider {
                    AiProviderKind::OpenCodeZen => "zen",
                    AiProviderKind::OpenCodeGo => "go",
                }
            );
        }
        (
            ai_state.client.clone(),
            ai_state.credentials.clone(),
            ai_state.config.clone(),
            provider,
        )
    };

    cx.editor
        .set_status(format!("{}: connecting...", provider_name));

    let callback = async move {
        let messages = vec![AiMessage {
            role: "user".to_string(),
            content: prompt,
        }];

        let result = match client
            .chat_stream(provider, &credentials, &config, messages)
            .await
        {
            Ok(mut rx) => {
                // Accumulate all streaming chunks.
                let mut full_text = String::new();
                while let Some(chunk) = rx.recv().await {
                    if chunk.done {
                        break;
                    }
                    if !chunk.content.is_empty() {
                        full_text.push_str(&chunk.content);
                    }
                }
                Ok(full_text)
            }
            Err(e) => Err(format!("{}: {}", provider_name, e)),
        };

        match result {
            Ok(response_text) => {
                Ok(response_display_callback(provider_name, response_text))
            }
            Err(msg) => {
                let cb: job::Callback =
                    job::Callback::Editor(Box::new(move |editor| {
                        editor.set_error(msg);
                    }));
                Ok(cb)
            }
        }
    };

    cx.jobs.callback(callback);
    Ok(())
}

// ============================================================
// :ai-explain
// ============================================================

pub fn ai_explain(
    cx: &mut compositor::Context,
    _args: helix_core::command_line::Args,
    _event: PromptEvent,
) -> Result<()> {
    let target = crate::target::Target::Word;
    let context = resolve_target_context(cx, target);
    let prompt = context.explain_prompt();
    let provider_name = super::state()
        .context("AI state not initialized")?
        .config
        .provider
        .map(|p| p.display_name().to_string())
        .unwrap_or_else(|| "AI".to_string());
    send_ai_request(cx, prompt, provider_name)
}

// ============================================================
// Workspace Symbol Context helpers (Phase 20)
// ============================================================

/// Maximum references sent to the AI.
const MAX_REFERENCES: usize = 10;
/// Maximum lines for a definition snippet.
const MAX_DEFINITION_LINES: usize = 40;

/// Extract the identifier under the primary cursor.
fn symbol_under_cursor(cx: &compositor::Context) -> Option<String> {
    let (view, doc) = helix_view::current_ref!(cx.editor);
    let text = doc.text().slice(..);
    let cursor = doc.selection(view.id).primary().cursor(text);

    let is_word =
        |c: char| c.is_alphanumeric() || c == '_';

    let mut start = cursor;
    while start > 0 && text.get_char(start - 1).map(is_word).unwrap_or(false) {
        start -= 1;
    }
    let mut end = cursor;
    while end < text.len_chars() && text.get_char(end).map(is_word).unwrap_or(false) {
        end += 1;
    }
    if end <= start {
        return None;
    }
    Some(text.slice(start..end).chars().collect())
}

/// Read `[start_line, end_line]` (0-based, inclusive) from a file on disk,
/// bounded to `max_lines`. Returns None on any IO failure.
fn read_line_snippet(
    path: &std::path::Path,
    start_line: u32,
    end_line: u32,
    max_lines: usize,
) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    let lines: Vec<&str> = content.lines().collect();
    if lines.is_empty() {
        return None;
    }
    let last = lines.len() - 1;
    let s = (start_line as usize).min(last);
    let e = (end_line as usize).min(last).max(s);
    let e = e.min(s.saturating_add(max_lines.saturating_sub(1)));
    Some(lines[s..=e].join("\n"))
}

/// Extract capitalized identifiers (types) from a definition snippet.
/// 1-hop only: no recursive expansion.
fn extract_related_symbols(text: &str) -> Option<String> {
    use helix_core::regex::Regex;
    let re = Regex::new(r"\b[A-Z][A-Za-z0-9_]*\b").ok()?;
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for m in re.find_iter(text) {
        let name = m.as_str();
        if seen.insert(name.to_string()) {
            out.push(name.to_string());
        }
        if out.len() >= 8 {
            break;
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out.join("\n"))
    }
}

/// Convert a GotoDefinitionResponse into (path, start_line, end_line) tuples.
fn collect_definition_locations(
    resp: lsp::GotoDefinitionResponse,
    out: &mut Vec<(std::path::PathBuf, u32, u32)>,
) {
    fn push(uri: &lsp::Url, range: lsp::Range, out: &mut Vec<(std::path::PathBuf, u32, u32)>) {
        if let Ok(uri) = helix_core::Uri::try_from(uri.clone()) {
            if let Some(path) = uri.as_path() {
                out.push((path.to_path_buf(), range.start.line, range.end.line));
            }
        }
    }
    match resp {
        lsp::GotoDefinitionResponse::Scalar(l) => push(&l.uri, l.range, out),
        lsp::GotoDefinitionResponse::Array(ls) => {
            for l in ls {
                push(&l.uri, l.range, out);
            }
        }
        lsp::GotoDefinitionResponse::Link(links) => {
            for l in links {
                push(&l.target_uri, l.target_range, out);
            }
        }
    }
}

// ============================================================
// :ai-review — workspace-aware review (definition + callers)
// ============================================================

pub fn ai_review(
    cx: &mut compositor::Context,
    _args: helix_core::command_line::Args,
    _event: PromptEvent,
) -> Result<()> {
    let base_context = resolve_target_context(cx, Target::Function);
    let symbol = symbol_under_cursor(cx).unwrap_or_else(|| "cursor".to_string());

    let provider_name = super::state()
        .context("AI state not initialized")?
        .config
        .provider
        .map(|p| p.display_name().to_string())
        .unwrap_or_else(|| "AI".to_string());

    // Gather AI client state up front so the async job owns everything.
    let (provider, client, credentials, config) = {
        let ai_state = super::state().context("AI state not initialized")?;
        let provider = ai_state
            .config
            .provider
            .context("No AI provider configured. Use :ai-connect first.")?;
        if ai_state.config.model.is_none() {
            anyhow::bail!("No model selected. Use :ai-model to select one.");
        }
        if !ai_state.credentials.has_key(provider) {
            anyhow::bail!(
                "{} API key is not configured. Use :ai-connect {}",
                provider,
                match provider {
                    AiProviderKind::OpenCodeZen => "zen",
                    AiProviderKind::OpenCodeGo => "go",
                }
            );
        }
        (
            provider,
            ai_state.client.clone(),
            ai_state.credentials.clone(),
            ai_state.config.clone(),
        )
    };

    // Collect LSP futures following the existing goto_single_impl pattern.
    // All lookups are best-effort: failures log and yield empty context.
    let mut def_futs: FuturesUnordered<_> = {
        let (view, doc) = helix_view::current_ref!(cx.editor);
        doc.language_servers_with_feature(LanguageServerFeature::GotoDefinition)
            .filter_map(|ls| {
                let enc = ls.offset_encoding();
                let pos = doc.position(view.id, enc);
                ls.goto_definition(doc.identifier(), pos, None)
                    .map(|fut| async move { anyhow::Ok((fut.await?, enc)) })
            })
            .collect()
    };
    let mut ref_futs: FuturesUnordered<_> = {
        let (view, doc) = helix_view::current_ref!(cx.editor);
        doc.language_servers_with_feature(LanguageServerFeature::GotoReference)
            .filter_map(|ls| {
                let enc = ls.offset_encoding();
                let pos = doc.position(view.id, enc);
                ls.goto_reference(doc.identifier(), pos, false, None)
                    .map(|fut| async move { anyhow::Ok((fut.await?, enc)) })
            })
            .collect()
    };

    // If no LSP supports either feature, fall back to plain single-buffer review.
    if def_futs.is_empty() && ref_futs.is_empty() {
        let prompt = base_context.review_prompt();
        return send_ai_request(cx, prompt, provider_name);
    }

    cx.editor.set_status(format!(
        "{}: collecting workspace context for '{}'...",
        provider_name, symbol
    ));

    let callback = async move {
        // Await definitions (best-effort)
        let mut definitions: Vec<(std::path::PathBuf, u32, u32)> = Vec::new();
        while let Some(res) = def_futs.next().await {
            match res {
                Ok((Some(resp), _enc)) => {
                    collect_definition_locations(resp, &mut definitions);
                }
                Ok((None, _)) => {}
                Err(err) => log::warn!("AI workspace: definition lookup failed: {err}"),
            }
        }

        // Await references (best-effort)
        let mut references: Vec<(std::path::PathBuf, u32, u32)> = Vec::new();
        while let Some(res) = ref_futs.next().await {
            match res {
                Ok((Some(locs), enc)) => {
                    for loc in locs {
                        if let Ok(uri) = helix_core::Uri::try_from(loc.uri) {
                            if let Some(path) = uri.as_path() {
                                let _ = enc; // line numbers are encoding-independent
                                references.push((
                                    path.to_path_buf(),
                                    loc.range.start.line,
                                    loc.range.end.line,
                                ));
                            }
                        }
                    }
                }
                Ok((None, _)) => {}
                Err(err) => log::warn!("AI workspace: references lookup failed: {err}"),
            }
        }

        // Build snippets from disk (bounded).
        let definition_text = definitions.first().and_then(|(p, s, e)| {
            read_line_snippet(p, *s, *e, MAX_DEFINITION_LINES)
        });
        let related_symbols =
            definition_text.as_deref().and_then(extract_related_symbols);

        let mut ref_items: Vec<String> = Vec::new();
        for (path, line, _) in references.iter().take(MAX_REFERENCES) {
            let source = read_line_snippet(path, *line, *line, 1)
                .map(|l| format!("  {}", l.trim()))
                .unwrap_or_default();
            ref_items.push(format!("{}:{}{}", path.display(), line + 1, source));
        }
        let references_text = EditorContext::format_reference_list(&ref_items, MAX_REFERENCES);

        let context = base_context
            .with_definition(definition_text)
            .with_references(references_text)
            .with_related_symbols(related_symbols);
        let prompt = context.review_prompt();

        let messages = vec![AiMessage {
            role: "user".to_string(),
            content: prompt,
        }];

        let result = match client
            .chat_stream(provider, &credentials, &config, messages)
            .await
        {
            Ok(mut rx) => {
                let mut full_text = String::new();
                while let Some(chunk) = rx.recv().await {
                    if chunk.done {
                        break;
                    }
                    if !chunk.content.is_empty() {
                        full_text.push_str(&chunk.content);
                    }
                }
                Ok(full_text)
            }
            Err(e) => Err(format!("{}: {}", provider_name, e)),
        };

        match result {
            Ok(response_text) => {
                Ok(response_display_callback(provider_name, response_text))
            }
            Err(msg) => {
                let cb: job::Callback = job::Callback::Editor(Box::new(move |editor| {
                    editor.set_error(msg);
                }));
                Ok(cb)
            }
        }
    };

    cx.jobs.callback(callback);
    Ok(())
}

// ============================================================
// :ai-fix
// ============================================================

pub fn ai_fix(
    cx: &mut compositor::Context,
    _args: helix_core::command_line::Args,
    _event: PromptEvent,
) -> Result<()> {
    let target = crate::target::Target::Expression;
    let context = resolve_target_context(cx, target);
    let prompt = context.fix_prompt();
    let provider_name = super::state()
        .context("AI state not initialized")?
        .config
        .provider
        .map(|p| p.display_name().to_string())
        .unwrap_or_else(|| "AI".to_string());
    send_ai_request(cx, prompt, provider_name)
}

// ============================================================
// :ai-debug
// ============================================================

pub fn ai_debug(
    cx: &mut compositor::Context,
    _args: helix_core::command_line::Args,
    _event: PromptEvent,
) -> Result<()> {
    let target = crate::target::Target::Expression;
    let context = resolve_target_context(cx, target);
    let prompt = context.debug_prompt();
    let provider_name = super::state()
        .context("AI state not initialized")?
        .config
        .provider
        .map(|p| p.display_name().to_string())
        .unwrap_or_else(|| "AI".to_string());
    send_ai_request(cx, prompt, provider_name)
}

// ============================================================
// :ai-doc
// ============================================================

pub fn ai_doc(
    cx: &mut compositor::Context,
    _args: helix_core::command_line::Args,
    _event: PromptEvent,
) -> Result<()> {
    let target = crate::target::Target::Function;
    let context = resolve_target_context(cx, target);
    let prompt = context.doc_prompt();
    let provider_name = super::state()
        .context("AI state not initialized")?
        .config
        .provider
        .map(|p| p.display_name().to_string())
        .unwrap_or_else(|| "AI".to_string());
    send_ai_request(cx, prompt, provider_name)
}

// ============================================================
// :ai-suggest
// ============================================================

/// Propose concrete, applicable Target → Action improvements for the code
/// element at the cursor. The AI only suggests; applying is delegated to
/// `:ai-apply`.
/// Parse the optional target argument for `:ai-suggest [target]`.
///
/// - `None`            → default to `Target::Function` (existing behavior)
/// - `Some(name)`      → resolved via `Target::from_name`, then `Target::from_key`
///                       for a single canonical key char (e.g. `f`, `e`)
/// - unknown string    → `Err` (no silent fallback to another target)
///
/// Only the canonical `Target` enum reaches the execution layer.
fn parse_suggest_target(arg: Option<&str>) -> Result<crate::target::Target, String> {
    use crate::target::Target;
    match arg {
        None => Ok(Target::Function),
        Some(s) => {
            if let Some(target) = Target::from_name(s) {
                return Ok(target);
            }
            // A single character may be a canonical target key (e.g. 'f', 'e').
            // Multi-char strings are never treated as keys, to avoid ambiguity
            // (e.g. "banana" must not resolve via its first letter).
            if s.chars().count() == 1 {
                if let Some(target) = s.chars().next().and_then(Target::from_key) {
                    return Ok(target);
                }
            }
            Err(format!("Unknown target for :ai-suggest: '{}'", s))
        }
    }
}

/// Parse the optional action argument for `:ai-suggest [target] [action]`.
///
/// - `None`            → no requested action
/// - `Some(name)`      → resolved via `Action::from_name` (canonical only)
/// - unknown action    → `Err` (no silent fallback)
fn parse_suggest_action(arg: Option<&str>) -> Result<Option<crate::target::Action>, String> {
    match arg {
        None => Ok(None),
        Some(s) => crate::target::Action::from_name(s)
            .map(Some)
            .ok_or_else(|| format!("Unknown action for :ai-suggest: '{}'", s)),
    }
}

/// Parse `:ai-suggest [target] [action]` into a canonical `(Target, Option<Action>)`.
///
/// Reuses `parse_suggest_target` (which already handles names and canonical
/// target keys). The action, when present, is resolved through `Action::from_name`.
/// Invalid target or action yields `Err` with no silent fallback.
pub fn parse_suggest_args(
    args: &helix_core::command_line::Args,
) -> Result<(crate::target::Target, Option<crate::target::Action>), String> {
    let target = parse_suggest_target(args.first())?;
    let action = parse_suggest_action(args.get(1))?;
    Ok((target, action))
}

pub fn ai_suggest(
    cx: &mut compositor::Context,
    args: helix_core::command_line::Args,
    _event: PromptEvent,
) -> Result<()> {
    let (target, requested_action) =
        parse_suggest_args(&args).map_err(|m| anyhow::anyhow!(m))?;
    let base_context = resolve_target_context(cx, target);
    let symbol = symbol_under_cursor(cx).unwrap_or_else(|| "cursor".to_string());

    let provider_name = super::state()
        .context("AI state not initialized")?
        .config
        .provider
        .map(|p| p.display_name().to_string())
        .unwrap_or_else(|| "AI".to_string());

    // Gather AI client state up front so the async job owns everything.
    let (provider, client, credentials, config) = {
        let ai_state = super::state().context("AI state not initialized")?;
        let provider = ai_state
            .config
            .provider
            .context("No AI provider configured. Use :ai-connect first.")?;
        if ai_state.config.model.is_none() {
            anyhow::bail!("No model selected. Use :ai-model to select one.");
        }
        if !ai_state.credentials.has_key(provider) {
            anyhow::bail!(
                "{} API key is not configured. Use :ai-connect {}",
                provider,
                match provider {
                    AiProviderKind::OpenCodeZen => "zen",
                    AiProviderKind::OpenCodeGo => "go",
                }
            );
        }
        (
            provider,
            ai_state.client.clone(),
            ai_state.credentials.clone(),
            ai_state.config.clone(),
        )
    };

    // Clear any stale suggestions from a previous response before starting.
    if let Some(mut ai_state) = super::state() {
        ai_state.last_suggestions.clear();
    }

    // Collect LSP futures following the existing goto_single_impl pattern.
    // All lookups are best-effort: failures log and yield empty context.
    let mut def_futs: FuturesUnordered<_> = {
        let (view, doc) = helix_view::current_ref!(cx.editor);
        doc.language_servers_with_feature(LanguageServerFeature::GotoDefinition)
            .filter_map(|ls| {
                let enc = ls.offset_encoding();
                let pos = doc.position(view.id, enc);
                ls.goto_definition(doc.identifier(), pos, None)
                    .map(|fut| async move { anyhow::Ok((fut.await?, enc)) })
            })
            .collect()
    };
    let mut ref_futs: FuturesUnordered<_> = {
        let (view, doc) = helix_view::current_ref!(cx.editor);
        doc.language_servers_with_feature(LanguageServerFeature::GotoReference)
            .filter_map(|ls| {
                let enc = ls.offset_encoding();
                let pos = doc.position(view.id, enc);
                ls.goto_reference(doc.identifier(), pos, false, None)
                    .map(|fut| async move { anyhow::Ok((fut.await?, enc)) })
            })
            .collect()
    };

    // If no LSP supports either feature, fall back to single-buffer suggest.
    if def_futs.is_empty() && ref_futs.is_empty() {
        let prompt = base_context.suggest_prompt(requested_action);
        return send_ai_request_with(
            cx,
            prompt,
            provider_name,
            move |pn, rt| suggest_response_callback(pn, rt, requested_action),
        );
    }

    cx.editor.set_status(format!(
        "{}: collecting workspace context for '{}'...",
        provider_name, symbol
    ));

    let callback = async move {
        // Await definitions (best-effort)
        let mut definitions: Vec<(std::path::PathBuf, u32, u32)> = Vec::new();
        while let Some(res) = def_futs.next().await {
            match res {
                Ok((Some(resp), _enc)) => {
                    collect_definition_locations(resp, &mut definitions);
                }
                Ok((None, _)) => {}
                Err(err) => log::warn!("AI workspace: definition lookup failed: {err}"),
            }
        }

        // Await references (best-effort)
        let mut references: Vec<(std::path::PathBuf, u32, u32)> = Vec::new();
        while let Some(res) = ref_futs.next().await {
            match res {
                Ok((Some(locs), enc)) => {
                    for loc in locs {
                        if let Ok(uri) = helix_core::Uri::try_from(loc.uri) {
                            if let Some(path) = uri.as_path() {
                                let _ = enc; // line numbers are encoding-independent
                                references.push((
                                    path.to_path_buf(),
                                    loc.range.start.line,
                                    loc.range.end.line,
                                ));
                            }
                        }
                    }
                }
                Ok((None, _)) => {}
                Err(err) => log::warn!("AI workspace: references lookup failed: {err}"),
            }
        }

        // Build snippets from disk (bounded).
        let definition_text = definitions.first().and_then(|(p, s, e)| {
            read_line_snippet(p, *s, *e, MAX_DEFINITION_LINES)
        });
        let related_symbols =
            definition_text.as_deref().and_then(extract_related_symbols);

        let mut ref_items: Vec<String> = Vec::new();
        for (path, line, _) in references.iter().take(MAX_REFERENCES) {
            let source = read_line_snippet(path, *line, *line, 1)
                .map(|l| format!("  {}", l.trim()))
                .unwrap_or_default();
            ref_items.push(format!("{}:{}{}", path.display(), line + 1, source));
        }
        let references_text = EditorContext::format_reference_list(&ref_items, MAX_REFERENCES);

        let context = base_context
            .with_definition(definition_text)
            .with_references(references_text)
            .with_related_symbols(related_symbols);
        let prompt = context.suggest_prompt(requested_action);

        let messages = vec![AiMessage {
            role: "user".to_string(),
            content: prompt,
        }];

        let result = match client
            .chat_stream(provider, &credentials, &config, messages)
            .await
        {
            Ok(mut rx) => {
                let mut full_text = String::new();
                while let Some(chunk) = rx.recv().await {
                    if chunk.done {
                        break;
                    }
                    if !chunk.content.is_empty() {
                        full_text.push_str(&chunk.content);
                    }
                }
                Ok(full_text)
            }
            Err(e) => Err(format!("{}: {}", provider_name, e)),
        };

        match result {
            Ok(response_text) => {
                Ok(suggest_response_callback(provider_name, response_text, requested_action))
            }
            Err(msg) => {
                let cb: job::Callback = job::Callback::Editor(Box::new(move |editor| {
                    editor.set_error(msg);
                }));
                Ok(cb)
            }
        }
    };

    cx.jobs.callback(callback);
    Ok(())
}

/// Like `send_ai_request` but accepts a custom completion callback, so that
/// `:ai-suggest` can report suggestion counts without altering other commands.
fn send_ai_request_with(
    cx: &mut compositor::Context,
    prompt: String,
    provider_name: String,
    on_done: impl Fn(String, String) -> job::Callback + Send + 'static,
) -> Result<()> {
    let (client, credentials, config, provider) = {
        let ai_state = super::state().context("AI state not initialized")?;
        let provider = ai_state
            .config
            .provider
            .context("No AI provider configured. Use :ai-connect first.")?;
        if ai_state.config.model.is_none() {
            anyhow::bail!("No model selected. Use :ai-model to select one.");
        }
        if !ai_state.credentials.has_key(provider) {
            anyhow::bail!(
                "{} API key is not configured. Use :ai-connect {}",
                provider,
                match provider {
                    AiProviderKind::OpenCodeZen => "zen",
                    AiProviderKind::OpenCodeGo => "go",
                }
            );
        }
        (
            ai_state.client.clone(),
            ai_state.credentials.clone(),
            ai_state.config.clone(),
            provider,
        )
    };

    cx.editor
        .set_status(format!("{}: connecting...", provider_name));

    let callback = async move {
        let messages = vec![AiMessage {
            role: "user".to_string(),
            content: prompt,
        }];

        let result = match client
            .chat_stream(provider, &credentials, &config, messages)
            .await
        {
            Ok(mut rx) => {
                let mut full_text = String::new();
                while let Some(chunk) = rx.recv().await {
                    if chunk.done {
                        break;
                    }
                    if !chunk.content.is_empty() {
                        full_text.push_str(&chunk.content);
                    }
                }
                Ok(full_text)
            }
            Err(e) => Err(format!("{}: {}", provider_name, e)),
        };

        match result {
            Ok(response_text) => Ok(on_done(provider_name, response_text)),
            Err(msg) => {
                let cb: job::Callback = job::Callback::Editor(Box::new(move |editor| {
                    editor.set_error(msg);
                }));
                Ok(cb)
            }
        }
    };

    cx.jobs.callback(callback);
    Ok(())
}

// ============================================================
// :ai-review-diff
// ============================================================

pub fn ai_review_diff(
    cx: &mut compositor::Context,
    _args: helix_core::command_line::Args,
    _event: PromptEvent,
) -> Result<()> {
    let prompt = {
        let (_view, doc) = helix_view::current_ref!(cx.editor);

        // Check if git diff is available
        let diff_handle = doc
            .diff_handle()
            .context("No git diff available for this file. Is it in a git repository?")?;

        let diff = diff_handle.load();
        let hunks = diff.len();

        if hunks == 0 {
            anyhow::bail!("No changes in the current file.");
        }

        // Build context with diff information
        let text = doc.text().slice(..);
        let mut diff_context = String::new();
        let mut functions_affected = std::collections::HashSet::new();

        for i in 0..hunks.min(20) {
            let hunk = diff.nth_hunk(i);
            let start = text.line_to_char(hunk.after.start as usize);
            let end = text.line_to_char(hunk.after.end as usize);
            let hunk_text = text.slice(start..end).to_string();

            // Try to identify which function this hunk belongs to
            let line = hunk.after.start as usize;
            let mut func_name = "unknown".to_string();

            // Simple heuristic: look backwards for function definition
            for check_line in (0..line).rev() {
                let line_start = text.line_to_char(check_line);
                let line_end = text.line_to_char(check_line + 1);
                let line_text = text.slice(line_start..line_end).to_string();
                let trimmed = line_text.trim();

                if trimmed.starts_with("fn ")
                    || trimmed.starts_with("pub fn ")
                    || trimmed.starts_with("async fn ")
                    || trimmed.starts_with("pub async fn ")
                    || (trimmed.contains("fn ") && trimmed.contains('('))
                {
                    func_name = trimmed.to_string();
                    if func_name.chars().count() > 80 {
                        func_name = format!("{}...", func_name.chars().take(77).collect::<String>());
                    }
                    break;
                }
                if trimmed.starts_with("impl ") || trimmed.starts_with("pub impl ") {
                    func_name = trimmed.to_string();
                    if func_name.chars().count() > 80 {
                        func_name = format!("{}...", func_name.chars().take(77).collect::<String>());
                    }
                    break;
                }
            }

            functions_affected.insert(func_name.clone());
            diff_context.push_str(&format!(
                "--- Hunk {} (function: {}) ---\n{}\n",
                i + 1,
                func_name,
                hunk_text
            ));
        }

        let file_path = doc
            .path()
            .and_then(|p| p.to_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| "unknown".to_string());

        format!(
            "Review the following git changes in {}.\n\
             {} function(s) affected.\n\n\
             Changes:\n{}\n\
             Provide a focused review of the changes. \
             Analyze correctness, potential issues, and suggest improvements. \
             Do not modify files.\n",
            file_path,
            functions_affected.len(),
            diff_context
        )
    }; // immutable borrow of cx.editor ends here

    let provider_name = super::state()
        .context("AI state not initialized")?
        .config
        .provider
        .map(|p| p.display_name().to_string())
        .unwrap_or_else(|| "AI".to_string());
    send_ai_request(cx, prompt, provider_name)
}

// ============================================================
// :ai-apply — activate the last AI suggestion (user-confirmed)
// ============================================================

/// Build the callback that executes one suggestion through the normal
/// Target → Action machinery (EditorView::execute_target_action).
/// Resolve a suggestion's target against the CURRENT editor state and produce
/// Pure check: does the suggestion name a canonical `Target` we can attempt to
/// resolve? Does not touch the editor. Used to classify un-previewable items.
fn suggestion_target_known(sugg: &super::response::ActionSuggestion) -> bool {
    sugg.target.and_then(crate::target::Target::from_name).is_some()
}

/// Whether the suggestion can be previewed at the CURRENT editor state.
/// Read-only: never resolves mutably and never edits. Combines the target-name
/// check with the same tree-sitter guard `apply_suggestion_callback` uses, so
/// preview and apply agree on resolvability.
fn suggestion_preview_available(
    sugg: &super::response::ActionSuggestion,
    editor: &helix_view::Editor,
) -> bool {
    if !suggestion_target_known(sugg) {
        return false;
    }
    let target = sugg.target.and_then(crate::target::Target::from_name).unwrap();
    let ts_based = !matches!(
        target,
        crate::target::Target::Word
            | crate::target::Target::Line
            | crate::target::Target::Paragraph
            | crate::target::Target::String
            | crate::target::Target::Argument
            | crate::target::Target::Brackets
            | crate::target::Target::All
    );
    if ts_based {
        let (_view, doc) = helix_view::current_ref!(editor);
        if doc.syntax().is_none() {
            return false;
        }
    }
    true
}

/// the `FileLocation` the existing Picker preview can render. Read-only: it
/// never edits the document. Returns `None` when the suggestion cannot be
/// previewed at the CURRENT cursor (unknown target or a tree-sitter target
/// with no usable syntax tree). Shared by Phase 22 (multi-suggestion Picker
/// preview) and `:ai-preview` (single-suggestion preview).
fn suggestion_preview_location(
    sugg: &super::response::ActionSuggestion,
    editor: &helix_view::Editor,
) -> Option<(crate::ui::picker::PathOrId<'static>, Option<(usize, usize)>)> {
    if !suggestion_preview_available(sugg, editor) {
        return None;
    }
    let target = sugg.target.and_then(crate::target::Target::from_name).unwrap();
    let (view, doc) = helix_view::current_ref!(editor);
    let syn_loader = editor.syn_loader.clone();
    let selection = target.resolve(doc, view, &syn_loader);
    let primary = selection.primary();
    let text = doc.text().slice(..);
    let start_line = text.char_to_line(primary.from());
    let end_line = text.char_to_line(primary.to());
    let doc_id = doc.id();
    Some((crate::ui::picker::PathOrId::Id(doc_id), Some((start_line, end_line))))
}

/// Stable display label for a preview Picker item, keyed by the suggestion's
/// original 1-based order in `AiState.last_suggestions`. Numbering never
/// changes when the Picker filters entries. `available` marks whether the
/// suggestion can be previewed at the current cursor.
fn preview_item_label(
    n: usize,
    sugg: &super::response::ActionSuggestion,
    available: bool,
) -> String {
    if available {
        format!("#{} {}", n, super::response::format_action_suggestion(sugg))
    } else {
        format!(
            "#{} {} (cannot preview)",
            n,
            super::response::format_action_suggestion(sugg)
        )
    }
}

fn apply_suggestion_callback(
    sugg: super::response::ActionSuggestion,
) -> job::Callback {
    job::Callback::EditorCompositor(Box::new(move |editor, compositor| {
        // Parse back into the canonical enums. Only this enum pair may reach
        // the execution layer — never raw model text.
        let Some(target) = sugg.target.and_then(crate::target::Target::from_name) else {
            editor.set_error("AI suggestion has an unknown target");
            return;
        };
        let Some(action) = sugg.action.and_then(crate::target::Action::from_name) else {
            editor.set_error("AI suggestion has an unknown action");
            return;
        };

        // Tree-sitter targets require a syntax tree; refuse cleanly instead of
        // silently degrading to a cursor-level edit (unresolvable target must
        // not modify the document).
        let ts_based = !matches!(
            target,
            crate::target::Target::Word
                | crate::target::Target::Line
                | crate::target::Target::Paragraph
                | crate::target::Target::String
                | crate::target::Target::Argument
                | crate::target::Target::Brackets
                | crate::target::Target::All
        );
        if ts_based {
            let (_view, doc) = helix_view::current_ref!(editor);
            if doc.syntax().is_none() {
                editor.set_error(format!(
                    "Cannot resolve target '{}' here: no syntax tree for this buffer",
                    target.name()
                ));
                return;
            }
        }

        // Execute through the SAME path as manual `t{key}{action}`:
        // resolves at CURRENT cursor, records repeat intent, uses normal
        // transactions/undo, and Change enters insert mode for the user.
        if let Some(editor_view) = compositor.find::<crate::ui::EditorView>() {
            editor_view.execute_target_action(
                editor,
                target,
                action,
                1,
                crate::target::Direction::Forward,
            );
        } else {
            editor.set_error("Editor view unavailable");
        }
    }))
}

/// Parse the optional 1-based index for `:ai-apply <n>`.
///
/// Returns:
/// - `None`            → no argument supplied (default behavior)
/// - `Some(Ok(idx))`   → valid 0-based suggestion index
/// - `Some(Err(msg))`  → invalid argument (non-numeric / negative) or 0
fn parse_apply_index(arg: Option<&str>) -> Option<Result<usize, String>> {
    let arg = arg?;
    match arg.parse::<i64>() {
        // Negative → invalid argument.
        Ok(n) if n < 0 => Some(Err(format!("Invalid argument for :ai-apply: '{}'", arg))),
        // 0 is not a valid 1-based index.
        Ok(n) if n == 0 => Some(Err(format!("Invalid suggestion index: {}", n))),
        // Positive → convert to 0-based. Range is checked at apply time.
        Ok(n) => Some(Ok(n as usize - 1)),
        Err(_) => Some(Err(format!("Invalid argument for :ai-apply: '{}'", arg))),
    }
}

pub fn ai_apply(
    cx: &mut compositor::Context,
    args: helix_core::command_line::Args,
    _event: PromptEvent,
) -> Result<()> {
    let suggestions = super::state()
        .context("AI state not initialized")?
        .last_suggestions
        .clone();

    if suggestions.is_empty() {
        anyhow::bail!("No AI suggestion available. Run :ai-explain/:ai-review etc. first.");
    }

    // Optional 1-based index: `:ai-apply <n>` → direct apply of that suggestion.
    if let Some(res) = parse_apply_index(args.first()) {
        let idx = res.map_err(|m| anyhow::anyhow!(m))?;
        let Some(&sugg) = suggestions.get(idx) else {
            anyhow::bail!(
                "Invalid suggestion index: {} (have {} suggestion(s))",
                idx + 1,
                suggestions.len()
            );
        };
        let label = super::response::format_action_suggestion(&sugg);
        cx.editor
            .set_status(format!("applying suggestion {}: {}", idx + 1, label));
        cx.jobs
            .callback(async move { Ok(apply_suggestion_callback(sugg)) });
        return Ok(());
    }

    // Single suggestion → apply directly (explicit user command = confirmation).
    if let [only] = &suggestions[..] {
        let sugg = *only;
        cx.jobs
            .callback(async move { Ok(apply_suggestion_callback(sugg)) });
        return Ok(());
    }

    // Multiple → let the user pick via the existing Picker infrastructure.
    // Picker is non-Send, so it is constructed inside the UI-thread callback
    // from plain Send data (same pattern as :ai-model).
    struct SuggestionItem {
        label: String,
        sugg: super::response::ActionSuggestion,
    }

    let items: Vec<SuggestionItem> = suggestions
        .iter()
        .map(|s| SuggestionItem {
            label: super::response::format_action_suggestion(s),
            sugg: *s,
        })
        .collect();

    cx.jobs.callback(async move {
        Ok(job::Callback::EditorCompositor(Box::new(
            move |_editor, compositor| {
                let columns = [crate::ui::PickerColumn::new(
                    "suggestion",
                    |item: &SuggestionItem, _| item.label.as_str().into(),
                )];

                let picker = crate::ui::Picker::new(
                    columns,
                    0,
                    items,
                    (),
                    |cx, item, _action| {
                        let sugg = item.sugg;
                        cx.jobs
                            .callback(async move { Ok(apply_suggestion_callback(sugg)) });
                    },
                )
                .with_preview(|editor, item| suggestion_preview_location(&item.sugg, editor))
                .truncate_start(false);

                use crate::ui::overlay::overlaid;
                compositor.push(Box::new(overlaid(picker)));
            },
        )))
    });
    Ok(())
}

// ============================================================
// :ai-preview — read-only preview of a single suggestion
// ============================================================

/// Preview where stored AI suggestions would resolve at the CURRENT cursor,
/// without applying or modifying the document.
///
/// - `:ai-preview`        → browse ALL stored suggestions (read-only Picker)
/// - `:ai-preview <n>`    → preview only suggestion #n (Phase 26 behavior)
pub fn ai_preview(
    cx: &mut compositor::Context,
    args: helix_core::command_line::Args,
    _event: PromptEvent,
) -> Result<()> {
    let suggestions = super::state()
        .context("AI state not initialized")?
        .last_suggestions
        .clone();

    if suggestions.is_empty() {
        anyhow::bail!("No AI suggestion available. Run :ai-suggest first.");
    }

    // Build the (1-based number, suggestion) list to preview.
    // No argument → preview every stored suggestion.
    // Argument present → preview exactly that 1-based suggestion.
    let indexed: Vec<(usize, super::response::ActionSuggestion)> = match parse_apply_index(
        args.first(),
    ) {
        None => suggestions
            .iter()
            .enumerate()
            .map(|(i, s)| (i + 1, *s))
            .collect(),
        Some(res) => {
            let idx = res.map_err(|m| anyhow::anyhow!(m))?;
            let Some(&sugg) = suggestions.get(idx) else {
                anyhow::bail!(
                    "Invalid suggestion index: {} (have {} suggestion(s))",
                    idx + 1,
                    suggestions.len()
                );
            };
            vec![(idx + 1, sugg)]
        }
    };

    struct PreviewItem {
        label: String,
        sugg: super::response::ActionSuggestion,
    }

    // Classify each suggestion at the CURRENT cursor (read-only). Previewable
    // items resolve normally; un-previewable ones are kept in the list but
    // marked "(cannot preview)".
    let unavailable = indexed
        .iter()
        .filter(|(_, sugg)| !suggestion_preview_available(sugg, &cx.editor))
        .count();

    let items: Vec<PreviewItem> = indexed
        .into_iter()
        .map(|(n, sugg)| {
            let available = suggestion_preview_available(&sugg, &cx.editor);
            PreviewItem {
                label: preview_item_label(n, &sugg, available),
                sugg,
            }
        })
        .collect();

    if unavailable > 0 {
        cx.editor.set_status(format!(
            "{} suggestion(s) cannot be previewed at current cursor",
            unavailable
        ));
    }

    // Read-only Picker on the UI thread from plain Send data (same pattern as
    // :ai-apply). The selection callback is a no-op: preview never applies.
    cx.jobs.callback(async move {
        Ok(job::Callback::EditorCompositor(Box::new(
            move |_editor, compositor| {
                let columns = [crate::ui::PickerColumn::new(
                    "suggestion",
                    |item: &PreviewItem, _| item.label.as_str().into(),
                )];

                let picker = crate::ui::Picker::new(
                    columns,
                    0,
                    items,
                    (),
                    |cx, _item, _action| {
                        // Read-only preview only. Never apply.
                        cx.editor.set_status("preview closed — use :ai-apply to apply");
                    },
                )
                .with_preview(|editor, item| suggestion_preview_location(&item.sugg, editor))
                .truncate_start(false);

                use crate::ui::overlay::overlaid;
                compositor.push(Box::new(overlaid(picker)));
            },
        )))
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_apply_index_none() {
        // No argument → use default (Picker / single-direct) behavior.
        assert!(parse_apply_index(None).is_none());
    }

    #[test]
    fn test_parse_apply_index_1based() {
        // 1-based mapping to 0-based index.
        assert_eq!(parse_apply_index(Some("1")), Some(Ok(0)));
        assert_eq!(parse_apply_index(Some("2")), Some(Ok(1)));
        assert_eq!(parse_apply_index(Some("3")), Some(Ok(2)));
    }

    #[test]
    fn test_parse_apply_index_zero_rejected() {
        assert!(parse_apply_index(Some("0")).unwrap().is_err());
    }

    #[test]
    fn test_parse_apply_index_negative_rejected() {
        assert!(parse_apply_index(Some("-1")).unwrap().is_err());
        assert!(parse_apply_index(Some("-42")).unwrap().is_err());
    }

    #[test]
    fn test_parse_apply_index_non_numeric_rejected() {
        assert!(parse_apply_index(Some("abc")).unwrap().is_err());
        assert!(parse_apply_index(Some("1.5")).unwrap().is_err());
    }

    #[test]
    fn test_parse_apply_index_out_of_range_resolves_to_index() {
        // Range is validated against `last_suggestions` at apply time, not here.
        assert_eq!(parse_apply_index(Some("99")), Some(Ok(98)));
    }

    #[test]
    fn test_parse_suggest_target_none_defaults_to_function() {
        // No argument → existing default behavior.
        assert_eq!(parse_suggest_target(None), Ok(crate::target::Target::Function));
    }

    #[test]
    fn test_parse_suggest_target_by_name() {
        assert_eq!(
            parse_suggest_target(Some("function")),
            Ok(crate::target::Target::Function)
        );
        assert_eq!(
            parse_suggest_target(Some("expression")),
            Ok(crate::target::Target::Expression)
        );
        assert_eq!(
            parse_suggest_target(Some("statement")),
            Ok(crate::target::Target::Statement)
        );
        assert_eq!(
            parse_suggest_target(Some("word")),
            Ok(crate::target::Target::Word)
        );
    }

    #[test]
    fn test_parse_suggest_target_by_canonical_key() {
        // Single canonical key char resolves through Target::from_key.
        assert_eq!(
            parse_suggest_target(Some("f")),
            Ok(crate::target::Target::Function)
        );
        assert_eq!(
            parse_suggest_target(Some("e")),
            Ok(crate::target::Target::Expression)
        );
        assert_eq!(
            parse_suggest_target(Some("s")),
            Ok(crate::target::Target::Statement)
        );
        assert_eq!(
            parse_suggest_target(Some("w")),
            Ok(crate::target::Target::Word)
        );
    }

    #[test]
    fn test_parse_suggest_target_unknown_rejected() {
        // No silent fallback to another target.
        assert!(parse_suggest_target(Some("banana")).is_err());
        assert!(parse_suggest_target(Some("unknown_target")).is_err());
    }

    #[test]
    fn test_parse_suggest_target_multichar_not_treated_as_key() {
        // "banana" must not resolve via its first letter 'b' (Block).
        assert!(parse_suggest_target(Some("banana")).is_err());
    }

    #[test]
    fn test_target_name_from_name_roundtrip() {
        let targets = [
            crate::target::Target::Word,
            crate::target::Target::Line,
            crate::target::Target::Expression,
            crate::target::Target::Statement,
            crate::target::Target::Function,
            crate::target::Target::Block,
            crate::target::Target::Class,
            crate::target::Target::Paragraph,
            crate::target::Target::String,
            crate::target::Target::Argument,
            crate::target::Target::Brackets,
            crate::target::Target::All,
        ];
        for t in &targets {
            assert_eq!(crate::target::Target::from_name(t.name()), Some(*t));
        }
    }

    #[test]
    fn test_target_key_from_key_roundtrip() {
        let targets = [
            crate::target::Target::Word,
            crate::target::Target::Line,
            crate::target::Target::Expression,
            crate::target::Target::Statement,
            crate::target::Target::Function,
            crate::target::Target::Block,
            crate::target::Target::Class,
            crate::target::Target::Paragraph,
            crate::target::Target::String,
            crate::target::Target::Argument,
            crate::target::Target::Brackets,
            crate::target::Target::All,
        ];
        for t in &targets {
            // String target uses the '"' key; skip its round-trip via char.
            if *t == crate::target::Target::String {
                continue;
            }
            assert_eq!(crate::target::Target::from_key(t.key()), Some(*t));
        }
    }

    #[test]
    fn test_ai_preview_reuses_apply_index_parsing() {
        // :ai-preview shares Phase 24's parse_apply_index semantics.
        // No argument → None (means "preview all"); argument present uses it.
        assert!(parse_apply_index(None).is_none());
        // 1-based valid index.
        assert_eq!(parse_apply_index(Some("1")), Some(Ok(0)));
        assert_eq!(parse_apply_index(Some("3")), Some(Ok(2)));
        // Index 0 rejected.
        assert!(parse_apply_index(Some("0")).unwrap().is_err());
        // Negative rejected.
        assert!(parse_apply_index(Some("-1")).unwrap().is_err());
        // Non-numeric rejected.
        assert!(parse_apply_index(Some("abc")).unwrap().is_err());
        // Out-of-range is parsed here, validated at apply/preview time.
        assert_eq!(parse_apply_index(Some("99")), Some(Ok(98)));
    }

    #[test]
    fn test_preview_item_label_is_stable_and_numbered() {
        // Label carries the suggestion's original 1-based order; filtering in
        // the Picker must never renumber it. `available` toggles the
        // "(cannot preview)" marker without changing the number.
        let sugg = crate::ai::response::ActionSuggestion {
            target: Some("function"),
            action: Some("change"),
        };
        let label = preview_item_label(2, &sugg, true);
        assert!(label.starts_with("#2 "));
        assert!(label.contains("target: function"));
        assert!(label.contains("action: change"));
        assert!(!label.contains("cannot preview"));
        // Unavailable → marker appended, number unchanged.
        let label_unavail = preview_item_label(2, &sugg, false);
        assert!(label_unavail.starts_with("#2 "));
        assert!(label_unavail.contains("cannot preview"));
        // Different order → different stable number.
        assert!(preview_item_label(3, &sugg, true).starts_with("#3 "));
    }

    #[test]
    fn test_suggestion_target_known_pure() {
        // Pure name check: no editor required.
        assert!(suggestion_target_known(&crate::ai::response::ActionSuggestion {
            target: Some("function"),
            action: Some("change"),
        }));
        assert!(suggestion_target_known(&crate::ai::response::ActionSuggestion {
            target: Some("statement"),
            action: Some("delete"),
        }));
        // Unknown target name → not previewable.
        assert!(!suggestion_target_known(&crate::ai::response::ActionSuggestion {
            target: Some("banana"),
            action: Some("change"),
        }));
        // Missing target → not previewable.
        assert!(!suggestion_target_known(&crate::ai::response::ActionSuggestion {
            target: None,
            action: Some("change"),
        }));
    }

    #[test]
    fn test_parse_suggest_action() {
        // No argument → no requested action.
        assert_eq!(parse_suggest_action(None), Ok(None));
        // Canonical action names.
        assert_eq!(
            parse_suggest_action(Some("change")),
            Ok(Some(crate::target::Action::Change))
        );
        assert_eq!(
            parse_suggest_action(Some("delete")),
            Ok(Some(crate::target::Action::Delete))
        );
        // Unknown action → Err, no silent fallback.
        assert!(parse_suggest_action(Some("banana")).is_err());
    }

    // Build an Args value from a command line for testing parse_suggest_args.
    fn suggest_args(line: &str) -> helix_core::command_line::Args<'_> {
        helix_core::command_line::Args::parse(
            line,
            helix_core::command_line::Signature {
                positionals: (0, Some(2)),
                ..helix_core::command_line::Signature::DEFAULT
            },
            false,
            |t| Ok(t.content),
        )
        .unwrap()
    }

    #[test]
    fn test_parse_suggest_args_combinations() {
        // No args → Function / None.
        assert_eq!(
            parse_suggest_args(&suggest_args("")),
            Ok((crate::target::Target::Function, None))
        );
        // Target only.
        assert_eq!(
            parse_suggest_args(&suggest_args("expression")),
            Ok((crate::target::Target::Expression, None))
        );
        // Target + valid action.
        assert_eq!(
            parse_suggest_args(&suggest_args("statement delete")),
            Ok((
                crate::target::Target::Statement,
                Some(crate::target::Action::Delete)
            ))
        );
        // Canonical target key also resolves.
        assert_eq!(
            parse_suggest_args(&suggest_args("e change")),
            Ok((
                crate::target::Target::Expression,
                Some(crate::target::Action::Change)
            ))
        );
        // Invalid target → Err.
        assert!(parse_suggest_args(&suggest_args("banana change")).is_err());
        // Invalid action → Err.
        assert!(parse_suggest_args(&suggest_args("expression banana")).is_err());
    }

    fn sugg(t: &'static str, a: &'static str) -> crate::ai::response::ActionSuggestion {
        crate::ai::response::ActionSuggestion {
            target: Some(t),
            action: Some(a),
        }
    }

    #[test]
    fn test_filter_suggestions_by_action() {
        let all = vec![
            sugg("function", "change"),
            sugg("statement", "delete"),
            sugg("expression", "change"),
        ];
        // No request → all retained.
        assert_eq!(filter_suggestions_by_action(all.clone(), None).len(), 3);

        // Change → only the two change suggestions.
        let change = filter_suggestions_by_action(all.clone(), Some(crate::target::Action::Change));
        assert_eq!(change.len(), 2);
        assert!(change.contains(&sugg("function", "change")));
        assert!(change.contains(&sugg("expression", "change")));

        // Delete → only the delete suggestion.
        let del = filter_suggestions_by_action(all, Some(crate::target::Action::Delete));
        assert_eq!(del, vec![sugg("statement", "delete")]);

        // Action not present → empty Vec.
        let none = filter_suggestions_by_action(
            vec![sugg("function", "change")],
            Some(crate::target::Action::Yank),
        );
        assert!(none.is_empty());

        // Unknown action in a suggestion does not survive (parser yields None).
        let unknown = filter_suggestions_by_action(
            vec![sugg("function", "frobnicate")],
            Some(crate::target::Action::Change),
        );
        assert!(unknown.is_empty());
    }
}
