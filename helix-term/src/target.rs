use helix_core::{
    syntax,
    textobject::{self, TextObject},
    Range, RopeSlice, Selection, Syntax,
};
use helix_view::{Document, View};

use arc_swap::access::DynAccess;

/// Semantic editing targets.
/// Each target represents a kind of code region the user can select.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Word,
    Line,
    Expression,
    Statement,
    Function,
    Block,
    Class,
    Paragraph,
    String,
    Argument,
    Parameter,
    Brackets,
    All,
}

impl Target {
    /// Human-readable name for status display.
    pub fn name(self) -> &'static str {
        match self {
            Target::Word => "word",
            Target::Line => "line",
            Target::Expression => "expression",
            Target::Statement => "statement",
            Target::Function => "function",
            Target::Block => "block",
            Target::Class => "class",
            Target::Paragraph => "paragraph",
            Target::String => "string",
            Target::Argument => "argument",
            Target::Parameter => "parameter",
            Target::Brackets => "brackets",
            Target::All => "all",
        }
    }
    /// Resolve this target to a Selection based on the current cursor position.
    /// Uses tree-sitter when available, falls back to text primitives.
    pub fn resolve(
        self,
        doc: &Document,
        view: &View,
        syn_loader: &std::sync::Arc<arc_swap::ArcSwap<syntax::Loader>>,
    ) -> Selection {
        let text = doc.text().slice(..);
        let selection = doc.selection(view.id).clone();
        let loader = syn_loader.load();

        selection.transform(|range| match self {
            Target::Word => textobject::textobject_word(text, range, TextObject::Inside, 1, false),
            Target::Line => {
                let line = range.cursor_line(text);
                let start = text.line_to_char(line);
                let end = if line + 1 < text.len_lines() {
                    text.line_to_char(line + 1)
                } else {
                    text.len_chars()
                };
                Range::new(start, end)
            }
            Target::Expression => {
                resolve_treesitter_target(doc.syntax(), text, range, "expression", &loader)
            }
            Target::Statement => resolve_treesitter_target(doc.syntax(), text, range, "statement", &loader),
            Target::Function => resolve_treesitter_target(doc.syntax(), text, range, "function", &loader),
            Target::Block => resolve_treesitter_target(doc.syntax(), text, range, "block", &loader),
            Target::Class => resolve_treesitter_target(doc.syntax(), text, range, "class", &loader),
            Target::Paragraph => {
                textobject::textobject_paragraph(text, range, TextObject::Inside, 1)
            }
            Target::String => {
                textobject::textobject_pair_surround(doc.syntax(), text, range, TextObject::Inside, '"', 1)
            }
            Target::Argument => {
                textobject::textobject_pair_surround(doc.syntax(), text, range, TextObject::Inside, '(', 1)
            }
            Target::Parameter => {
                resolve_treesitter_target(doc.syntax(), text, range, "parameter", &loader)
            }
            Target::Brackets => {
                textobject::textobject_pair_surround_closest(doc.syntax(), text, range, TextObject::Inside, 1)
            }
            Target::All => Range::new(0, text.len_chars()),
        })
    }

    /// Resolve this target `count` times starting from `start`, expanding forward
    /// to the next `count` reachable semantic objects. Returns a `Selection`
    /// holding each resolved range (in document order, non-overlapping).
    ///
    /// Count is purely a *scope* multiplier: it never changes the action that is
    /// later applied to the resulting selection. The document selection is used
    /// only as scratch state and is overwritten by the caller afterwards.
    ///
    /// Guards against empty/duplicate/non-advancing resolutions so a count can
    /// never produce an infinite loop, an empty selection, or a panic.
    pub fn resolve_counted(
        &self,
        doc: &mut Document,
        view: &View,
        syn_loader: &std::sync::Arc<arc_swap::ArcSwap<syntax::Loader>>,
        count: usize,
        start: usize,
    ) -> Selection {
        let text = doc.text().slice(..);
        let total = text.len_chars();
        let count = count.clamp(1, 4096);
        let mut cursor = start.min(total);
        let mut ranges: Vec<Range> = Vec::with_capacity(count);
        let mut guard: usize = 0;
        // Generous but strictly bounded: each iteration either collects a
        // target or advances one char, so traversal always terminates.
        let max_iter = count.saturating_mul(32) + 128;

        while ranges.len() < count && guard < max_iter {
            guard += 1;
            doc.set_selection(view.id, Selection::point(cursor));
            let resolved = if self.is_structural() {
                match self.structural_capture(doc, syn_loader, cursor) {
                    Some(r) => r,
                    None => {
                        if cursor >= total {
                            break;
                        }
                        cursor = (cursor + 1).min(total);
                        continue;
                    }
                }
            } else {
                self.resolve(doc, view, syn_loader).primary()
            };
            let from = resolved.from();
            let to = resolved.to();

            // Empty resolution: nudge forward, never collect junk.
            if from == to {
                if cursor >= total {
                    break;
                }
                cursor = (cursor + 1).min(total);
                continue;
            }
            // Skip duplicates / overlaps with the previously collected range.
            if let Some(last) = ranges.last() {
                if *last == resolved || last.to() > from {
                    if cursor >= total {
                        break;
                    }
                    cursor = (cursor + 1).min(total);
                    continue;
                }
            }
            ranges.push(resolved);
            cursor = to;
        }

        if ranges.is_empty() {
            return Selection::point(start);
        }
        let mut selection = Selection::single(ranges[0].anchor, ranges[0].head);
        for r in &ranges[1..] {
            selection = selection.push(*r);
        }
        selection.set_primary_index(0);
        selection
    }

    /// Resolve the NEXT semantic object(s) after whatever currently occupies
    /// `cursor`. Used by semantic repeat (`.`) after scope-preserving actions:
    /// the object under the cursor still exists, so repeat must step past its
    /// end before collecting `count` fresh objects from the CURRENT document.
    /// Falls back to `Selection::point` when nothing further resolves.
    pub fn resolve_advance(
        &self,
        doc: &mut Document,
        view: &View,
        syn_loader: &std::sync::Arc<arc_swap::ArcSwap<syntax::Loader>>,
        count: usize,
        cursor: usize,
    ) -> Selection {
        let total = doc.text().len_chars();
        doc.set_selection(view.id, Selection::point(cursor.min(total)));
        let current = self.resolve(doc, view, syn_loader).primary();
        let start = repeat_start_position(cursor, current.to(), total);
        self.resolve_counted(doc, view, syn_loader, count, start)
    }

    /// Directional entry point: resolve `count` semantic objects starting at
    /// `start`, scanning in `dir`. The returned Selection is ALWAYS document-
    /// ordered so the existing action core is unaffected by direction.
    pub fn resolve_counted_dir(
        &self,
        doc: &mut Document,
        view: &View,
        syn_loader: &std::sync::Arc<arc_swap::ArcSwap<syntax::Loader>>,
        count: usize,
        start: usize,
        dir: Direction,
    ) -> Selection {
        match dir {
            Direction::Forward => self.resolve_counted(doc, view, syn_loader, count, start),
            Direction::Backward => self.resolve_counted_back(doc, view, syn_loader, count, start),
        }
    }

    /// Backward twin of `resolve_counted`: discover up to `count` semantic
    /// objects strictly BEFORE the object containing `start`, walking toward
    /// the document start. Ranges are collected in reverse discovery order and
    /// reversed before building the Selection so the result is document-ordered.
    ///
    /// Acceptance boundary: an object qualifies only when it ends at or before
    /// the current boundary (initially `start`); after each acceptance the
    /// boundary moves to the accepted object's start. Objects that merely
    /// contain the scan position (the "current" object, ancestors) are skipped.
    /// All arithmetic is saturating; traversal is strictly bounded and stops at
    /// position 0  Efewer than `count` objects simply yields fewer ranges.
    pub fn resolve_counted_back(
        &self,
        doc: &mut Document,
        view: &View,
        syn_loader: &std::sync::Arc<arc_swap::ArcSwap<syntax::Loader>>,
        count: usize,
        start: usize,
    ) -> Selection {
        let total = doc.text().len_chars();
        let count = count.clamp(1, 4096);
        let mut pos = start.min(total);
        let mut boundary = pos;
        let mut ranges: Vec<Range> = Vec::with_capacity(count);
        let mut guard: usize = 0;
        // Bounded like the forward engine; skips of overlapping objects walk a
        // char at a time, so the budget is doubled.
        let max_iter = count.saturating_mul(64) + 256;

        while ranges.len() < count && guard < max_iter {
            guard += 1;
            if pos == 0 && !ranges.is_empty() {
                break;
            }
            doc.set_selection(view.id, Selection::point(pos));
            let resolved = if self.is_structural() {
                match self.structural_capture(doc, syn_loader, pos) {
                    Some(r) => r,
                    None => {
                        if pos == 0 {
                            break;
                        }
                        pos -= 1;
                        continue;
                    }
                }
            } else {
                self.resolve(doc, view, syn_loader).primary()
            };
            let from = resolved.from();
            let to = resolved.to();

            // Empty resolution at the scan cursor: nudge back.
            if from == to {
                if pos == 0 {
                    break;
                }
                pos -= 1;
                continue;
            }
            // Object entirely before the boundary and not already collected.
            if to <= boundary && !ranges.contains(&resolved) {
                ranges.push(resolved);
                if ranges.len() == count {
                    break;
                }
                boundary = from;
                pos = from.saturating_sub(1);
                continue;
            }
            // Overlapping object (contains the scan point): skip below its start.
            if pos == 0 {
                break;
            }
            pos = pos.saturating_sub(1);
        }

        ranges.reverse();
        if ranges.is_empty() {
            return Selection::point(start);
        }
        let mut selection = Selection::single(ranges[0].anchor, ranges[0].head);
        for r in &ranges[1..] {
            selection = selection.push(*r);
        }
        selection.set_primary_index(0);
        selection
    }

    /// Directional advance used by semantic repeat for scope-preserving
    /// actions: step past whatever occupies `cursor` (forward: past its end;
    /// backward: before its start), then collect `count` fresh objects in `dir`.
    pub fn resolve_advance_dir(
        &self,
        doc: &mut Document,
        view: &View,
        syn_loader: &std::sync::Arc<arc_swap::ArcSwap<syntax::Loader>>,
        count: usize,
        cursor: usize,
        dir: Direction,
    ) -> Selection {
        match dir {
            Direction::Forward => self.resolve_advance(doc, view, syn_loader, count, cursor),
            Direction::Backward => {
                let total = doc.text().len_chars();
                doc.set_selection(view.id, Selection::point(cursor.min(total)));
                let current = self.resolve(doc, view, syn_loader).primary();
                let entry = current.from().saturating_sub(1);
                self.resolve_counted_back(doc, view, syn_loader, count, entry)
            }
        }
    }

    /// Whether this target is backed by tree-sitter structure. Structural
    /// targets are resolved through exact capture lookup during traversal
    /// (see `structural_capture`), so gaps never produce junk ranges and
    /// legitimately width-1 captures are kept.
    pub fn is_structural(self) -> bool {
        matches!(
            self,
            Self::Expression | Self::Statement | Self::Function | Self::Block | Self::Class | Self::Parameter
        )
    }

    /// The tree-sitter textobject base name for structural targets.
    fn treesitter_object(self) -> Option<&'static str> {
        match self {
            Self::Expression => Some("expression"),
            Self::Statement => Some("statement"),
            Self::Function => Some("function"),
            Self::Block => Some("block"),
            Self::Class => Some("class"),
            Self::Parameter => Some("parameter"),
            _ => None,
        }
    }

    /// EXACT tree-sitter lookup for structural targets: the innermost
    /// `{object}.inside` capture containing `pos`, or `None` when no
    /// capture covers it. Replaces the old geometric width-1 heuristic,
    /// which misclassified legitimately width-1 semantic nodes (e.g.
    /// Python identifier parameters) as resolver misses.
    fn structural_capture(
        &self,
        doc: &Document,
        syn_loader: &std::sync::Arc<arc_swap::ArcSwap<syntax::Loader>>,
        pos: usize,
    ) -> Option<Range> {
        let object = self.treesitter_object()?;
        let syntax = doc.syntax()?;
        let text = doc.text().slice(..);
        let pos = pos.min(text.len_chars());
        let probe = Range::new(pos, pos);
        let loader = syn_loader.load();
        textobject::textobject_treesitter_captured(
            text,
            probe,
            TextObject::Inside,
            object,
            syntax,
            &loader,
        )
    }

    /// Parse a target from a single character key.
    pub fn from_key(ch: char) -> Option<Target> {
        match ch {
            'w' => Some(Target::Word),
            'l' => Some(Target::Line),
            'e' => Some(Target::Expression),
            's' => Some(Target::Statement),
            'f' => Some(Target::Function),
            'b' => Some(Target::Block),
            'g' => Some(Target::Class),
            'p' => Some(Target::Paragraph),
            '"' => Some(Target::String),
            '(' => Some(Target::Argument),
            'n' => Some(Target::Parameter),
            '%' => Some(Target::Brackets),
            'a' => Some(Target::All),
            _ => None,
        }
    }

    /// The single-character key for this target (inverse of `from_key`).
    pub fn key(self) -> char {
        match self {
            Target::Word => 'w',
            Target::Line => 'l',
            Target::Expression => 'e',
            Target::Statement => 's',
            Target::Function => 'f',
            Target::Block => 'b',
            Target::Class => 'g',
            Target::Paragraph => 'p',
            Target::String => '"',
            Target::Argument => '(',
            Target::Parameter => 'n',
            Target::Brackets => '%',
            Target::All => 'a',
        }
    }

    /// Parse a target from its human-readable name (inverse of `name`).
    pub fn from_name(name: &str) -> Option<Target> {
        match name {
            "word" => Some(Target::Word),
            "line" => Some(Target::Line),
            "expression" => Some(Target::Expression),
            "statement" => Some(Target::Statement),
            "function" => Some(Target::Function),
            "block" => Some(Target::Block),
            "class" => Some(Target::Class),
            "paragraph" => Some(Target::Paragraph),
            "string" => Some(Target::String),
            "argument" => Some(Target::Argument),
            "parameter" => Some(Target::Parameter),
            "brackets" => Some(Target::Brackets),
            "all" => Some(Target::All),
            _ => None,
        }
    }

    /// Discoverability reference for the Target ↁEAction editing model.
    /// Every `Target` variant appears exactly once, paired with its key and a
    /// short human description. Kept in sync with `from_name` / `from_key`.
    pub fn targets_help() -> &'static [(&'static str, char, &'static str)] {
        &[
            ("word", 'w', "the word under the cursor"),
            ("line", 'l', "the current line"),
            ("expression", 'e', "the syntactic expression at point"),
            ("statement", 's', "the enclosing statement"),
            ("function", 'f', "the enclosing function definition"),
            ("block", 'b', "the enclosing block"),
            ("class", 'g', "the enclosing class or type"),
            ("paragraph", 'p', "the current paragraph"),
            ("string", '"', "the enclosing string literal"),
            ("argument", '(', "the enclosing function argument"),
            ("parameter", 'n', "the enclosing function parameter"),
            ("brackets", '%', "the enclosing bracket pair"),
            ("all", 'a', "the whole document"),
        ]
    }
}

/// Resolve a tree-sitter target. Falls back to the original range if
/// tree-sitter is not available or the capture is not found.
fn resolve_treesitter_target(
    syntax: Option<&Syntax>,
    text: RopeSlice,
    range: Range,
    object_name: &str,
    loader: &syntax::Loader,
) -> Range {
    match syntax {
        Some(syntax) => {
            textobject::textobject_treesitter(
                text,
                range,
                TextObject::Inside,
                object_name,
                syntax,
                loader,
                1,
            )
        }
        _ => range,
    }
}

/// Actions that can be applied to a resolved target selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Delete,
    Change,
    Yank,
    Indent,
    Outdent,
}

impl Action {
    /// Human-readable name for status display.
    pub fn name(self) -> &'static str {
        match self {
            Action::Delete => "delete",
            Action::Change => "change",
            Action::Yank => "yank",
            Action::Indent => "indent",
            Action::Outdent => "outdent",
        }
    }

    /// Parse an action from its human-readable name (inverse of `name`).
    pub fn from_name(name: &str) -> Option<Action> {
        match name {
            "delete" => Some(Action::Delete),
            "change" => Some(Action::Change),
            "yank" => Some(Action::Yank),
            "indent" => Some(Action::Indent),
            "outdent" => Some(Action::Outdent),
            _ => None,
        }
    }

    /// Discoverability reference for actions. Each `Action` variant appears once,
    /// paired with its key and a short description. In sync with `from_name` /
    /// `from_key` (`action` variants never alias a target key).
    pub fn actions_help() -> &'static [(&'static str, char, &'static str)] {
        &[
            ("delete", 'd', "remove the target"),
            ("change", 'c', "remove it and enter insert to replace"),
            ("yank", 'y', "copy the target to a register"),
            ("indent", '>', "increase indentation"),
            ("outdent", '<', "decrease indentation"),
        ]
    }
}

/// Pure parse of the `t [-][count] <target>` key stream into
/// `(count, direction, target)`.
///
/// An optional LEADING `-` selects `Direction::Backward`; the magnitude is
/// accumulated separately as a plain `usize` (capped at 4096) so direction and
/// count never mix into a signed value. The first non-digit target char
/// finalizes the spec; any other key is invalid. This is the testable core of
/// the interactive `handle_target_input_step`  Ethe action key is handled
/// separately and is intentionally NOT part of this spec.
pub fn parse_target_spec(keys: &[char]) -> Option<(usize, Direction, Target)> {
    let mut count: Option<usize> = None;
    let mut backward = false;
    let mut target: Option<Target> = None;
    for (i, &ch) in keys.iter().enumerate() {
        if ch == '-' && i == 0 && !backward {
            backward = true;
        } else if ch.is_ascii_digit() {
            let digit = ch.to_digit(10).unwrap_or(0) as usize;
            let next = count
                .map_or(0, |v| v)
                .saturating_mul(10)
                .saturating_add(digit)
                .min(4096);
            count = Some(next);
        } else if let Some(t) = Target::from_key(ch) {
            target = Some(t);
            break;
        } else {
            return None;
        }
    }
    let dir = Direction::from_leading_minus(backward);
    target.map(|t| (count.unwrap_or(1), dir, t))
}

/// Delimiter-safe deletion spans for resolved `Target::Parameter` ranges.
///
/// For each parameter participating in a separator-delimited list (per the
/// repository's own `. ","?` query contract):
///   - a PRECEDING `,` sibling exists → delete `[sep, param]`
///     (LEFT-anchored: works identically for single, middle, and trailing
///     members, and merges cleanly for contiguous multi-parameter groups),
///   - otherwise a FOLLOWING `,` sibling → delete `[param, sep + one
///     space/tab]` (list-head parameters; never crosses a newline),
///   - neither (single parameter) → delete the parameter alone.
///
/// Expanded spans are merged when they touch or overlap, so counted deletion
/// of adjacent parameters removes exactly one contiguous region including
/// precisely one outer boundary separator. Ranges stay sorted and disjoint —
/// ready for one `Transaction::change`. Without syntax (or outside any
/// capture) each range passes through unchanged.
pub fn parameter_delete_selection(
    doc: &Document,
    syn_loader: &std::sync::Arc<arc_swap::ArcSwap<syntax::Loader>>,
    selection: &Selection,
) -> Selection {
    let text = doc.text().slice(..);
    let loader = syn_loader.load();
    let syntax = doc.syntax();

    // Zero-width ranges can never BE a parameter (captures have width); they
    // are the engines' documented "nothing found" fallback. Expanding one
    // against a capture under the point would fabricate a deletion, so they
    // pass through untouched.
    //
    // Each member yields: its deletion span, whether a PRECEDING separator
    // anchored it (group classification), and where its trailing separator
    // (+ one blank column) would end — needed to close HEAD groups on the
    // right after merging.
    struct Member {
        span: (usize, usize),
        head: bool,
        right_extra: Option<usize>,
    }
    let mut members: Vec<Member> = selection
        .ranges()
        .iter()
        .filter(|r| r.from() < r.to())
        .map(|r| {
            let base = (r.from(), r.to());
            let Some(syntax) = syntax else {
                return Member { span: base, head: true, right_extra: None };
            };
            let Some((capture, next_sep, prev_sep)) =
                textobject::textobject_capture_sibling_separators(
                    text, *r, "parameter", syntax, &loader,
                )
            else {
                return Member { span: base, head: true, right_extra: None };
            };
            // Trailing-separator end candidate for EVERY member (head groups
            // must close on the last member's outer boundary). The REMOVED
            // line's own terminator (`\n` or `\r\n`) goes with it — mirroring
            // how left-anchored members swallow the line ending before their
            // own line. Further line endings are never touched.
            let line_ending_end = |mut end: usize| -> usize {
                let len = text.len_chars();
                if end < len && matches!(text.get_char(end), Some(' ' | '\t')) {
                    end += 1;
                }
                if end + 1 < len
                    && text.get_char(end) == Some('\r')
                    && text.get_char(end + 1) == Some('\n')
                {
                    end + 2
                } else if end < len && text.get_char(end) == Some('\n') {
                    end + 1
                } else {
                    end
                }
            };
            let right_extra = next_sep.map(|(_, sep_to)| line_ending_end(sep_to));
            let span = if let Some((sep_from, _)) = prev_sep {
                // LEFT-anchored: the separator before this parameter belongs
                // to its slot. Contiguous groups then chain into one region.
                (base.0.min(sep_from), base.1.max(capture.to()))
            } else if let Some(extra_end) = right_extra {
                // List head: consume the trailing separator (+ one blank
                // column / own-line newline).
                (base.0.min(capture.from()), base.1.max(extra_end))
            } else {
                base
            };
            // A parameter sitting at the START of its line owns that line's
            // leading indentation: absorb the horizontal run back to (but
            // never past) the preceding newline. Mid-line parameters are
            // unaffected because the run stops at a non-whitespace neighbor.
            let mut start = span.0;
            while start > 0 && matches!(text.get_char(start - 1), Some(' ' | '\t')) {
                start -= 1;
            }
            let at_line_start =
                start == 0 || text.get_char(start - 1) == Some('\n');
            let span = if at_line_start { (start, span.1) } else { span };
            Member { span, head: prev_sep.is_none(), right_extra }
        })
        .collect();

    members.sort_by_key(|m| m.span);
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(members.len());
    let mut group_is_head = false;
    let mut group_right_extra: Option<usize> = None;
    for m in members {
        match merged.last_mut() {
            Some(last) if m.span.0 <= last.1 => {
                last.1 = last.1.max(m.span.1);
                group_right_extra = m.right_extra; // LAST member governs the edge
            }
            _ => {
                if !merged.is_empty() && group_is_head {
                    if let Some(re) = group_right_extra {
                        let last = merged.last_mut().unwrap();
                        last.1 = last.1.max(re);
                    }
                }
                merged.push(m.span);
                group_is_head = m.head;
                group_right_extra = m.right_extra;
            }
        }
    }
    if group_is_head {
        if let Some(re) = group_right_extra {
            let last = merged.last_mut().unwrap();
            last.1 = last.1.max(re);
        }
    }
    if merged.is_empty() {
        return selection.clone();
    }

    let mut out = Selection::single(merged[0].0, merged[0].1);
    for &(s, e) in &merged[1..] {
        out = out.push(Range::new(s, e));
    }
    out.set_primary_index(0);
    out
}

/// Format the active Tachyon semantic prefix for display in the Explorer.
///
/// Returns a string like `"t"`, `"t 3"`, `"t -3"`, `"t 3 function"`, etc.
/// Pure function — no side effects, no Selection/Range/Position.
pub fn format_tachyon_prefix(
    count: Option<usize>,
    backward: bool,
    target: Option<Target>,
) -> String {
    let mut s = String::from("t");
    if let Some(n) = count {
        if backward {
            s.push_str(&format!(" -{n}"));
        } else {
            s.push_str(&format!(" {n}"));
        }
    } else if backward {
        s.push_str(" -");
    }
    if let Some(t) = target {
        s.push(' ');
        s.push_str(t.name());
    }
    s
}

/// Whether `(count, action)` is a well-defined semantic editing combination.
///
/// `count == 0` is meaningless and rejected. Every action — including
/// `Change` — supports any positive count: counted Change resolves N
/// disjoint targets through the standard pipeline and applies Helix's
/// native multi-selection change (`Transaction::delete_by_selection`),
/// leaving one insertion cursor per target (Phase 50 contract).
pub fn is_supported_target_action(count: usize, _action: Action) -> bool {
    count > 0
}

/// Whether an action removes its target scope from the document.
///
/// Drives SEMANTIC REPEAT advancement: after a scope-removing action the next
/// object now occupies the cursor position, so repeat resolves AT the cursor;
/// after a preserving action (yank/indent/outdent) the operated object still
/// exists, so repeat must first step PAST it. Derived purely from the action  E/// no stale geometry is ever stored.
pub fn action_removes_target(action: Action) -> bool {
    matches!(action, Action::Delete | Action::Change)
}

/// Pure advancement rule for semantic repeat: given the cursor and the end of
/// whichever object currently occupies it, return the search start for the
/// NEXT object. Nudges one char past the boundary so the same tree-sitter node
/// cannot be resolved twice, and never exceeds `total` (end-of-file safe).
pub fn repeat_start_position(cursor: usize, current_end: usize, total: usize) -> usize {
    if current_end > cursor {
        current_end.saturating_add(1).min(total)
    } else {
        (cursor + 1).min(total)
    }
}

/// Traversal direction for semantic resolution. Direction controls the ORDER
/// in which objects are discovered; it never changes action behavior and is
/// always stored separately from its magnitude (`count: usize`)  Ea user's
/// `-3` becomes `(Backward, 3)`, never a negative number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Forward,
    Backward,
}

impl Direction {
    /// Default direction when the user types none.
    pub fn default() -> Self {
        Direction::Forward
    }

    /// Pure parse of an optional leading sign into a direction.
    pub fn from_leading_minus(minus_seen: bool) -> Self {
        if minus_seen {
            Direction::Backward
        } else {
            Direction::Forward
        }
    }
}

/// How a target selection should be expanded for multi-cursor operations.
/// Stored as part of repeat intent  Enever stores stale selections.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MultiSelectIntent {
    /// Single cursor: resolve target at current cursor position only.
    None,
    /// All matching: find all occurrences of the selected text in the document.
    AllMatching,
}

impl Action {
    pub fn from_key(ch: char) -> Option<Action> {
        match ch {
            'd' => Some(Action::Delete),
            'c' => Some(Action::Change),
            'y' => Some(Action::Yank),
            '>' => Some(Action::Indent),
            '<' => Some(Action::Outdent),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_action_from_key() {
        assert_eq!(Action::from_key('d'), Some(Action::Delete));
        assert_eq!(Action::from_key('c'), Some(Action::Change));
        assert_eq!(Action::from_key('y'), Some(Action::Yank));
        assert_eq!(Action::from_key('>'), Some(Action::Indent));
        assert_eq!(Action::from_key('<'), Some(Action::Outdent));
        assert_eq!(Action::from_key('x'), None);
        assert_eq!(Action::from_key('a'), None);
        assert_eq!(Action::from_key(' '), None);
    }

    #[test]
    fn test_target_from_key() {
        assert_eq!(Target::from_key('w'), Some(Target::Word));
        assert_eq!(Target::from_key('l'), Some(Target::Line));
        assert_eq!(Target::from_key('e'), Some(Target::Expression));
        assert_eq!(Target::from_key('s'), Some(Target::Statement));
        assert_eq!(Target::from_key('f'), Some(Target::Function));
        assert_eq!(Target::from_key('b'), Some(Target::Block));
        assert_eq!(Target::from_key('g'), Some(Target::Class));
        assert_eq!(Target::from_key('p'), Some(Target::Paragraph));
        assert_eq!(Target::from_key('"'), Some(Target::String));
        assert_eq!(Target::from_key('('), Some(Target::Argument));
        assert_eq!(Target::from_key('n'), Some(Target::Parameter));
        assert_eq!(Target::from_key('%'), Some(Target::Brackets));
        assert_eq!(Target::from_key('a'), Some(Target::All));
        assert_eq!(Target::from_key('x'), None);
        assert_eq!(Target::from_key('z'), None);
    }

    #[test]
    fn test_action_equality() {
        assert_eq!(Action::Delete, Action::Delete);
        assert_ne!(Action::Delete, Action::Change);
        assert_ne!(Action::Yank, Action::Indent);
        assert_eq!(Action::Indent, Action::Indent);
        assert_eq!(Action::Outdent, Action::Outdent);
    }

    #[test]
    fn test_target_equality() {
        assert_eq!(Target::Word, Target::Word);
        assert_ne!(Target::Word, Target::Line);
        assert_ne!(Target::Function, Target::Class);
        assert_eq!(Target::Expression, Target::Expression);
    }

    #[test]
    fn test_target_action_pair_clone() {
        let pair = (Target::Word, Action::Change);
        let cloned = pair.clone();
        assert_eq!(pair.0, cloned.0);
        assert_eq!(pair.1, cloned.1);
    }

    #[test]
    fn test_action_clone() {
        let actions = vec![
            Action::Delete,
            Action::Change,
            Action::Yank,
            Action::Indent,
            Action::Outdent,
        ];
        for action in &actions {
            let cloned = *action;
            assert_eq!(*action, cloned);
        }
    }

    #[test]
    fn test_multi_select_intent_equality() {
        assert_eq!(MultiSelectIntent::None, MultiSelectIntent::None);
        assert_eq!(MultiSelectIntent::AllMatching, MultiSelectIntent::AllMatching);
        assert_ne!(MultiSelectIntent::None, MultiSelectIntent::AllMatching);
    }

    #[test]
    fn test_multi_select_intent_clone() {
        let intent = MultiSelectIntent::AllMatching;
        let cloned = intent;
        assert_eq!(intent, cloned);
    }

    #[test]
    fn test_target_action_multi_select_tuple() {
        let state: Option<(Target, Action, MultiSelectIntent)> =
            Some((Target::Word, Action::Change, MultiSelectIntent::AllMatching));
        let (target, action, multi) = state.unwrap();
        assert_eq!(target, Target::Word);
        assert_eq!(action, Action::Change);
        assert_eq!(multi, MultiSelectIntent::AllMatching);
    }

    #[test]
    fn test_single_vs_multi_intent_distinction() {
        let single: (Target, Action, MultiSelectIntent) =
            (Target::Word, Action::Delete, MultiSelectIntent::None);
        let multi: (Target, Action, MultiSelectIntent) =
            (Target::Word, Action::Delete, MultiSelectIntent::AllMatching);
        assert_eq!(single.0, multi.0);
        assert_eq!(single.1, multi.1);
        assert_ne!(single.2, multi.2);
    }

    #[test]
    fn test_target_name() {
        assert_eq!(Target::Word.name(), "word");
        assert_eq!(Target::Line.name(), "line");
        assert_eq!(Target::Expression.name(), "expression");
        assert_eq!(Target::Statement.name(), "statement");
        assert_eq!(Target::Function.name(), "function");
        assert_eq!(Target::Block.name(), "block");
        assert_eq!(Target::Class.name(), "class");
        assert_eq!(Target::Paragraph.name(), "paragraph");
        assert_eq!(Target::String.name(), "string");
        assert_eq!(Target::Argument.name(), "argument");
        assert_eq!(Target::Parameter.name(), "parameter");
        assert_eq!(Target::Brackets.name(), "brackets");
        assert_eq!(Target::All.name(), "all");
    }

    #[test]
    fn test_action_name() {
        assert_eq!(Action::Delete.name(), "delete");
        assert_eq!(Action::Change.name(), "change");
        assert_eq!(Action::Yank.name(), "yank");
        assert_eq!(Action::Indent.name(), "indent");
        assert_eq!(Action::Outdent.name(), "outdent");
    }

    #[test]
    fn test_target_name_is_lowercase() {
        let targets = [
            Target::Word, Target::Line, Target::Expression, Target::Statement,
            Target::Function, Target::Block, Target::Class, Target::Paragraph,
            Target::String, Target::Argument, Target::Parameter, Target::Brackets, Target::All,
        ];
        for target in &targets {
            assert_eq!(target.name(), target.name().to_lowercase());
        }
    }

    #[test]
    fn test_action_name_is_lowercase() {
        let actions = [
            Action::Delete, Action::Change, Action::Yank,
            Action::Indent, Action::Outdent,
        ];
        for action in &actions {
            assert_eq!(action.name(), action.name().to_lowercase());
        }
    }

    #[test]
    fn test_targets_help_is_complete_and_consistent() {
        assert_eq!(Target::targets_help().len(), 13);
        for (name, key, _desc) in Target::targets_help() {
            let by_name = Target::from_name(name).unwrap_or_else(|| panic!("name resolves: {name}"));
            let by_key = Target::from_key(*key).unwrap_or_else(|| panic!("key resolves: {key}"));
            assert_eq!(by_name, by_key, "name/key agree for {name}");
        }
    }

    #[test]
    fn test_actions_help_is_complete_and_consistency() {
        assert_eq!(Action::actions_help().len(), 5);
        for (name, key, _desc) in Action::actions_help() {
            let by_name = Action::from_name(name).unwrap_or_else(|| panic!("name resolves: {name}"));
            let by_key = Action::from_key(*key).unwrap_or_else(|| panic!("key resolves: {key}"));
            assert_eq!(by_name, by_key, "name/key agree for {name}");
        }
    }

    #[test]
    fn test_parse_target_spec_count_and_direction() {
        // No count/direction -> count 1, Forward.
        assert_eq!(
            parse_target_spec(&['f']),
            Some((1, Direction::Forward, Target::Function))
        );
        assert_eq!(
            parse_target_spec(&['e']),
            Some((1, Direction::Forward, Target::Expression))
        );
        // Single digit count, forward.
        assert_eq!(
            parse_target_spec(&['2', 'f']),
            Some((2, Direction::Forward, Target::Function))
        );
        // Multi-digit count.
        assert_eq!(
            parse_target_spec(&['1', '2', 's']),
            Some((12, Direction::Forward, Target::Statement))
        );
        // Count 0 is parsed (rejected later by is_supported_target_action).
        assert_eq!(
            parse_target_spec(&['0', 'f']),
            Some((0, Direction::Forward, Target::Function))
        );
        // Backward: sign and magnitude stay separate.
        assert_eq!(
            parse_target_spec(&['-', 'f']),
            Some((1, Direction::Backward, Target::Function))
        );
        assert_eq!(
            parse_target_spec(&['-', '3', 'f']),
            Some((3, Direction::Backward, Target::Function))
        );
        assert_eq!(
            parse_target_spec(&['-', '1', '2', 'e']),
            Some((12, Direction::Backward, Target::Expression))
        );
        // '-' not leading is invalid; digits after a mid-stream '-' too.
        assert_eq!(parse_target_spec(&['2', '-', 'f']), None);
        assert_eq!(parse_target_spec(&['-', '-', 'f']), None);
        // Action key is not part of the spec and stops parsing at the target.
        assert_eq!(
            parse_target_spec(&['f', 'd']),
            Some((1, Direction::Forward, Target::Function))
        );
        // Invalid target key -> None.
        assert_eq!(parse_target_spec(&['x']), None);
        assert_eq!(parse_target_spec(&['-', 'x']), None);
        // Empty input -> None.
        assert_eq!(parse_target_spec(&[]), None);
    }

    #[test]
    fn test_is_supported_target_action() {
        // Phase 50: counted Change is supported through the standard
        // multi-selection pipeline; only count 0 remains invalid.
        assert!(!is_supported_target_action(0, Action::Delete));
        assert!(!is_supported_target_action(0, Action::Change));
        assert!(!is_supported_target_action(0, Action::Yank));
        assert!(is_supported_target_action(1, Action::Change));
        assert!(is_supported_target_action(2, Action::Change));
        assert!(is_supported_target_action(5, Action::Change));
        assert!(is_supported_target_action(1, Action::Delete));
        assert!(is_supported_target_action(1, Action::Yank));
        assert!(is_supported_target_action(1, Action::Indent));
        assert!(is_supported_target_action(1, Action::Outdent));
        assert!(is_supported_target_action(3, Action::Delete));
        assert!(is_supported_target_action(3, Action::Yank));
        assert!(is_supported_target_action(3, Action::Indent));
        assert!(is_supported_target_action(3, Action::Outdent));
    }

    #[test]
    fn test_action_removes_target() {
        // Scope-removing: the next object occupies the cursor after execution.
        assert!(action_removes_target(Action::Delete));
        assert!(action_removes_target(Action::Change));
        // Scope-preserving: repeat must step PAST the operated object.
        assert!(!action_removes_target(Action::Yank));
        assert!(!action_removes_target(Action::Indent));
        assert!(!action_removes_target(Action::Outdent));
    }

    #[test]
    fn test_repeat_start_position() {
        let total = 100usize;
        // Object under cursor extends forward -> step just past its end.
        assert_eq!(repeat_start_position(10, 40, total), 41);
        // Resolution fell back to a point (tree-sitter unavailable / gap).
        assert_eq!(repeat_start_position(10, 10, total), 11);
        // End-of-file clamping never exceeds `total`.
        assert_eq!(repeat_start_position(98, 99, total), 100);
        assert_eq!(repeat_start_position(100, 100, total), 100);
        assert_eq!(repeat_start_position(0, usize::MAX, 50), 50);
    }

    #[test]
    fn test_repeat_state_tuple_semantics_only() {
        use crate::target::MultiSelectIntent;
        let state: Option<(Target, Action, usize, Direction, MultiSelectIntent)> = Some((
            Target::Statement,
            Action::Delete,
            3,
            Direction::Backward,
            MultiSelectIntent::None,
        ));
        let (target, action, count, dir, multi) = state.unwrap();
        assert_eq!(target, Target::Statement);
        assert_eq!(action, Action::Delete);
        assert_eq!(count, 3); // count preserved for counted repeat
        assert_eq!(dir, Direction::Backward); // direction preserved
        assert_eq!(multi, MultiSelectIntent::None);
        // No previous Tachyon action is representable as None.
        let none: Option<(Target, Action, usize, Direction, MultiSelectIntent)> = None;
        assert!(none.is_none());
        // Default direction is Forward (back-compat for `t f d`).
        assert_eq!(Direction::default(), Direction::Forward);
    }

    // ============================================================
    // Document-level harness for the REAL semantic resolver.
    //
    // Mirrors helix-view's own test pattern (Document::from + ViewId::default)
    // and loads the real Rust grammar from runtime/grammars  Eno mocks.
    // ============================================================

    use helix_core::{Rope, Transaction};
    use helix_view::editor::{Config as EditorConfig, GutterConfig};

    type TestLoader = std::sync::Arc<arc_swap::ArcSwap<syntax::Loader>>;

    /// Real language loader built from the repo's runtime languages.toml.
    fn test_loader() -> TestLoader {
        let lang = helix_loader::config::default_lang_config();
        std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(
            syntax::Loader::new(lang.try_into().unwrap()).unwrap(),
        ))
    }

    /// A Document with a REAL rust syntax tree plus a View whose id keys the
    /// document selection  Ethe same shape the live editor provides.
    fn rust_doc_at(src: &str, cursor: usize) -> (Document, View, TestLoader) {
        let loader = test_loader();
        let mut doc = Document::from(
            Rope::from(src),
            None,
            std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(EditorConfig::default())),
            loader.clone(),
        );
        let loaded = loader.load();
        doc.set_language_by_language_id("rust", &loaded).unwrap();
        let view = View::new(doc.id(), GutterConfig::default());
        doc.set_selection(view.id, Selection::point(cursor));
        (doc, view, loader)
    }

    /// Pure local helper: Selection -> ordered (start, end) spans.
    fn spans(selection: &Selection) -> Vec<(usize, usize)> {
        selection.ranges().iter().map(|r| (r.from(), r.to())).collect()
    }

    /// A Document with a REAL c syntax tree (Phase 47 cross-grammar matrix).
    fn c_doc_at(src: &str, cursor: usize) -> (Document, View, TestLoader) {
        let loader = test_loader();
        let mut doc = Document::from(
            Rope::from(src),
            None,
            std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(EditorConfig::default())),
            loader.clone(),
        );
        let loaded = loader.load();
        doc.set_language_by_language_id("c", &loaded).unwrap();
        let view = View::new(doc.id(), GutterConfig::default());
        doc.set_selection(view.id, Selection::point(cursor));
        (doc, view, loader)
    }

    /// A Document with a REAL go syntax tree (Phase 46 cross-grammar matrix).
    fn go_doc_at(src: &str, cursor: usize) -> (Document, View, TestLoader) {
        let loader = test_loader();
        let mut doc = Document::from(
            Rope::from(src),
            None,
            std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(EditorConfig::default())),
            loader.clone(),
        );
        let loaded = loader.load();
        doc.set_language_by_language_id("go", &loaded).unwrap();
        let view = View::new(doc.id(), GutterConfig::default());
        doc.set_selection(view.id, Selection::point(cursor));
        (doc, view, loader)
    }

    const THREE_FNS: &str = "fn alpha() {\n    let a = 1;\n}\n\nfn beta() {\n    let b = 2;\n}\n\nfn gamma() {\n    let c = 3;\n}\n";

    /// Pure local helper: the open-brace offset of `fn <name>` in the fixture.
    fn fn_open(src: &str, name: &str) -> usize {
        let sig = src.find(&format!("fn {name}")).unwrap();
        sig + src[sig..].find('{').unwrap()
    }


    #[test]
    fn test_resolve_function_real_treesitter() {
        let src = THREE_FNS;
        let inside_beta = src.find("let b").unwrap() + 2;
        let (doc, view, loader) = rust_doc_at(src, inside_beta);
        let range = Target::Function.resolve(&doc, &view, &loader).primary();
        // Real contract for rust @function.inside: the body block, braces included.
        let open = fn_open(src, "beta");
        let close = src[open..].find('}').unwrap() + open;
        assert_eq!((range.from(), range.to()), (open, close + 1));
        assert!(range.from() > src.find("fn beta").unwrap(), "past the signature");
    }

    #[test]
    fn test_resolve_counted_functions_ordered_and_bounded() {
        let src = THREE_FNS;
        let in_alpha = src.find("let a").unwrap() + 2;
        let (mut doc, view, loader) = rust_doc_at(src, in_alpha);

        // count = 1 -> exactly one semantic object.
        let one = Target::Function.resolve_counted(&mut doc, &view, &loader, 1, in_alpha);
        assert_eq!(spans(&one), vec![(fn_open(src, "alpha"), src.find("}\n\nfn beta").unwrap() + 1)]);

        // count = 2 -> two objects, document order, non-overlapping.
        let two = Target::Function.resolve_counted(&mut doc, &view, &loader, 2, in_alpha);
        let s2 = spans(&two);
        assert_eq!(s2.len(), 2);
        assert_eq!(s2[0].0, fn_open(src, "alpha"));
        assert_eq!(s2[1].0, fn_open(src, "beta"));
        assert!(s2[0].1 <= s2[1].0);

        // count larger than available -> terminates cleanly (no hang) with 3.
        let many = Target::Function.resolve_counted(&mut doc, &view, &loader, 50, in_alpha);
        let sm = spans(&many);
        assert_eq!(sm.len(), 3);
        assert_eq!(sm[2].0, fn_open(src, "gamma"));

        // count = 0 is clamped to 1 by contract.
        let zero = Target::Function.resolve_counted(&mut doc, &view, &loader, 0, in_alpha);
        assert_eq!(spans(&zero).len(), 1);
    }

    #[test]
    fn test_resolve_advance_walks_functions_without_duplicates() {
        let src = THREE_FNS;
        let in_alpha = src.find("let a").unwrap() + 2;
        let (mut doc, view, loader) = rust_doc_at(src, in_alpha);

        let first = Target::Function.resolve(&doc, &view, &loader).primary();
        let second =
            Target::Function.resolve_advance(&mut doc, &view, &loader, 1, first.head).primary();
        assert_ne!((first.from(), first.to()), (second.from(), second.to()));
        assert_eq!(second.from(), fn_open(src, "beta"));

        // Repeated advancement reaches gamma...
        let third =
            Target::Function.resolve_advance(&mut doc, &view, &loader, 1, second.head).primary();
        assert_eq!(third.from(), fn_open(src, "gamma"));

        // ...and past the final target: clean stop at document end (no panic,
        // no junk target).
        let _fourth =
            Target::Function.resolve_advance(&mut doc, &view, &loader, 1, third.head);
        assert_eq!(spans(&_fourth), vec![(src.len(), src.len())]);
    }

    #[test]
    fn test_resolve_advance_counted_next_pair() {
        let src = THREE_FNS;
        let in_alpha = src.find("let a").unwrap() + 2;
        let (mut doc, view, loader) = rust_doc_at(src, in_alpha);

        let pair1 = Target::Function.resolve_counted(&mut doc, &view, &loader, 2, in_alpha);
        let p1 = spans(&pair1);
        assert_eq!(p1.len(), 2);

        // Advance beyond the previous scope (which ENDED at beta's close, not
        // at the primary range): only gamma remains, so exactly ONE object.
        let scope_end = pair1.ranges().iter().map(|r| r.to()).max().unwrap_or(0);
        let pair2 = Target::Function.resolve_advance(&mut doc, &view, &loader, 2, scope_end);
        let p2 = spans(&pair2);
        assert_eq!(p2.len(), 1);
        assert_eq!(p2[0].0, fn_open(src, "gamma"));
        assert_ne!(p1[0], p2[0], "advance never returns the same target");
    }

    #[test]
    fn test_resolution_after_document_mutation_is_not_stale() {
        let src = THREE_FNS;
        let in_alpha = src.find("let a").unwrap() + 2;
        let (mut doc, view, loader) = rust_doc_at(src, in_alpha);

        // Delete function alpha entirely through normal transactions.
        let a_start = src.find("fn alpha").unwrap();
        let a_end = src.find("\n\nfn beta").unwrap(); // up to the blank line before beta
        let transaction =
            Transaction::change(doc.text(), vec![(a_start, a_end, None)].into_iter());
        doc.apply(&transaction, view.id);

        let new_text = doc.text().slice(..).to_string();
        // Resolution against the UPDATED document sees beta where alpha was:
        // the resolved object must contain beta''s body and never alpha''s.
        let resolved =
            Target::Function.resolve_counted(&mut doc, &view, &loader, 1, a_start).primary();
        let frag: String = new_text.chars().skip(resolved.from()).take(resolved.to() - resolved.from()).collect();
        assert!(frag.contains("let b"), "sees beta: {frag}");
        assert!(!frag.contains("let a"), "never stale alpha: {frag}");
    }

    #[test]
    fn test_resolve_between_targets_falls_back_to_selection() {
        let src = THREE_FNS;
        // Cursor in the whitespace gap between alpha and beta: no @function
        // ancestor covers it -> resolver falls back to the (width-1) selection.
        let gap = src.find("}\n\nfn beta").unwrap() + 2;
        let (doc, view, loader) = rust_doc_at(src, gap);
        let range = Target::Function.resolve(&doc, &view, &loader).primary();
        assert_eq!((range.from(), range.to()), (gap, gap + 1));
    }

    #[test]
    fn test_advance_at_end_of_document_is_safe() {
        let src = THREE_FNS;
        let last = src.len() - 1;
        let (mut doc, view, loader) = rust_doc_at(src, last);
        // Advancing from EOF terminates without panic; nothing valid resolves
        // (engine clamps to the exclusive document end).
        let advanced = Target::Function.resolve_advance(&mut doc, &view, &loader, 3, last);
        assert_eq!(spans(&advanced), vec![(src.len(), src.len())]);
    }

    #[test]
    fn test_resolve_without_syntax_falls_back_to_selection() {
        let src = THREE_FNS;
        let loader = test_loader();
        let mut doc = Document::from(
            Rope::from(src),
            None,
            std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(EditorConfig::default())),
            loader.clone(),
        );
        // No set_language -> syntax unavailable: documented fallback to the
        // stored selection (min-width-1 cursor).
        let view = View::new(doc.id(), GutterConfig::default());
        let cursor = src.find("let b").unwrap() + 2;
        doc.set_selection(view.id, Selection::point(cursor));
        let range = Target::Function.resolve(&doc, &view, &loader).primary();
        assert_eq!((range.from(), range.to()), (cursor, cursor + 1), "documented fallback");
    }


    // ============================================================
    // Phase 34: backward (directional) resolution on REAL trees.
    // ============================================================

    #[test]
    fn test_backward_one_from_gamma_resolves_beta() {
        let src = THREE_FNS;
        let in_gamma = src.find("let c").unwrap() + 2;
        let (mut doc, view, loader) = rust_doc_at(src, in_gamma);
        let sel =
            Target::Function.resolve_counted_dir(&mut doc, &view, &loader, 1, in_gamma, Direction::Backward);
        assert_eq!(spans(&sel), vec![(fn_open(src, "beta"), src.find("}\n\nfn gamma").unwrap() + 1)]);
    }

    #[test]
    fn test_backward_two_from_gamma_is_document_ordered() {
        let src = THREE_FNS;
        let in_gamma = src.find("let c").unwrap() + 2;
        let (mut doc, view, loader) = rust_doc_at(src, in_gamma);
        let sel =
            Target::Function.resolve_counted_dir(&mut doc, &view, &loader, 2, in_gamma, Direction::Backward);
        let s = spans(&sel);
        assert_eq!(s.len(), 2);
        // Document order even though discovery ran backwards.
        assert_eq!(s[0].0, fn_open(src, "alpha"));
        assert_eq!(s[1].0, fn_open(src, "beta"));
    }

    #[test]
    fn test_backward_larger_than_available_terminates() {
        let src = THREE_FNS;
        let in_gamma = src.find("let c").unwrap() + 2;
        let (mut doc, view, loader) = rust_doc_at(src, in_gamma);
        let sel =
            Target::Function.resolve_counted_dir(&mut doc, &view, &loader, 50, in_gamma, Direction::Backward);
        let s = spans(&sel);
        assert_eq!(s.len(), 2); // alpha, beta ? no hang, no junk
        assert_eq!(s[0].0, fn_open(src, "alpha"));
        assert_eq!(s[1].0, fn_open(src, "beta"));
    }

    #[test]
    fn test_backward_from_first_target_is_safe() {
        let src = THREE_FNS;
        let in_alpha = src.find("let a").unwrap() + 2;
        let (mut doc, view, loader) = rust_doc_at(src, in_alpha);
        // No function exists before alpha: clean point fallback, no panic.
        let sel =
            Target::Function.resolve_counted_dir(&mut doc, &view, &loader, 1, in_alpha, Direction::Backward);
        assert_eq!(spans(&sel), vec![(in_alpha, in_alpha)]);
    }

    #[test]
    fn test_backward_from_gap_after_beta_resolves_beta() {
        let src = THREE_FNS;
        let gap = src.find("}\n\nfn gamma").unwrap() + 2;
        let (mut doc, view, loader) = rust_doc_at(src, gap);
        let sel =
            Target::Function.resolve_counted_dir(&mut doc, &view, &loader, 1, gap, Direction::Backward);
        assert_eq!(spans(&sel), vec![(fn_open(src, "beta"), src.find("}\n\nfn gamma").unwrap() + 1)]);
    }

    #[test]
    fn test_backward_after_mutation_sees_current_tree() {
        let src = THREE_FNS;
        let in_gamma = src.find("let c").unwrap() + 2;
        let (mut doc, view, loader) = rust_doc_at(src, in_gamma);

        // Delete alpha through normal transactions.
        let a_start = src.find("fn alpha").unwrap();
        let a_end = src.find("\n\nfn beta").unwrap();
        let transaction =
            Transaction::change(doc.text(), vec![(a_start, a_end, None)].into_iter());
        doc.apply(&transaction, view.id);

        // Backward from gamma must now yield beta (alpha is gone).
        let sel =
            Target::Function.resolve_counted_dir(&mut doc, &view, &loader, 1, in_gamma - a_end + a_start, Direction::Backward);
        let new_text = doc.text().slice(..).to_string();
        let s = spans(&sel);
        assert_eq!(s.len(), 1);
        let frag: String = new_text
            .chars()
            .skip(s[0].0)
            .take(s[0].1 - s[0].0)
            .collect();
        assert!(frag.contains("let b"), "backward sees beta after mutation: {frag}");
        assert!(!frag.contains("let a") && !frag.contains("let c"));
    }

    #[test]
    fn test_advance_dir_backward_steps_past_current_scope() {
        let src = THREE_FNS;
        let in_beta = src.find("let b").unwrap() + 2;
        let (mut doc, view, loader) = rust_doc_at(src, in_beta);
        // Preserving-action repeat semantics: from inside beta, the previous
        // object is alpha (beta itself is stepped past).
        let sel = Target::Function
            .resolve_advance_dir(&mut doc, &view, &loader, 1, in_beta, Direction::Backward);
        assert_eq!(spans(&sel), vec![(fn_open(src, "alpha"), src.find("}\n\nfn beta").unwrap() + 1)]);
    }

    // ============================================================
    // Phase 35: second real grammar (Python, @class.inside) plus the
    // unsupported-target negative case. Python's textobjects.scm defines
    // function/class captures but NO @statement.*; no bundled grammar does.
    // Statement is therefore fallback-only by existing Helix query reality.
    // These tests pin that contract instead of inventing captures.
    // ============================================================

    /// A Document with a REAL python syntax tree.
    fn python_doc_at(src: &str, cursor: usize) -> (Document, View, TestLoader) {
        let loader = test_loader();
        let mut doc = Document::from(
            Rope::from(src),
            None,
            std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(EditorConfig::default())),
            loader.clone(),
        );
        let loaded = loader.load();
        doc.set_language_by_language_id("python", &loaded).unwrap();
        let view = View::new(doc.id(), GutterConfig::default());
        doc.set_selection(view.id, Selection::point(cursor));
        (doc, view, loader)
    }

    const THREE_CLASSES_PY: &str = "class Alpha:\n    def one(self):\n        x = 1\n\n\nclass Beta:\n    def two(self):\n        y = 2\n\n\nclass Gamma:\n    def three(self):\n        z = 3\n";

    fn py_class_body(src: &str, name: &str) -> (usize, usize) {
        let sig = src.find(&format!("class {name}")).unwrap();
        let colon = sig + src[sig..].find(':').unwrap();
        // Real @class.inside contract: the indented BLOCK contents — leading
        // indentation of the first line and all trailing blank newlines are
        // EXCLUDED (verified empirically against the python grammar).
        let nl = colon + src[colon..].find('\n').unwrap();
        let bytes = src.as_bytes();
        let mut s = nl + 1;
        while s < src.len() && bytes[s] == b' ' {
            s += 1;
        }
        let seg_end = match src[s..].find("\nclass ") {
            Some(off) => s + off,
            None => src.len(),
        };
        let mut e = seg_end;
        while e > s && bytes[e - 1] == b'\n' {
            e -= 1;
        }
        (s, e)
    }

    fn frag_at(src: &str, s: (usize, usize)) -> String {
        src.chars().skip(s.0).take(s.1 - s.0).collect()
    }

    #[test]
    fn test_python_class_forward_resolve_real_treesitter() {
        let src = THREE_CLASSES_PY;
        let in_gamma = src.find("z = 3").unwrap() + 2;
        let (doc, view, loader) = python_doc_at(src, in_gamma);
        let range = Target::Class.resolve(&doc, &view, &loader).primary();
        assert_eq!((range.from(), range.to()), py_class_body(src, "Gamma"));
        let frag = frag_at(src, (range.from(), range.to()));
        assert!(frag.contains("def three"), "gamma body selected");
        assert!(
            !frag.contains("def two") && !frag.contains("def one"),
            "adjacent classes excluded"
        );
    }

    #[test]
    fn test_python_class_counted_forward_ordered_and_bounded() {
        let src = THREE_CLASSES_PY;
        let in_alpha = src.find("x = 1").unwrap() + 2;
        let (mut doc, view, loader) = python_doc_at(src, in_alpha);
        let sel = Target::Class.resolve_counted_dir(
            &mut doc, &view, &loader, 2, in_alpha, Direction::Forward,
        );
        let s = spans(&sel);
        assert_eq!(s.len(), 2);
        assert_eq!(s[0], py_class_body(src, "Alpha"));
        assert_eq!(s[1], py_class_body(src, "Beta"));
        // count beyond availability terminates with what exists (3).
        let many = Target::Class.resolve_counted_dir(
            &mut doc, &view, &loader, 50, in_alpha, Direction::Forward,
        );
        assert_eq!(spans(&many).len(), 3);
    }

    #[test]
    fn test_python_class_backward_single_counted_and_bob_boundary() {
        let src = THREE_CLASSES_PY;
        let in_gamma = src.find("z = 3").unwrap() + 2;
        let (mut doc, view, loader) = python_doc_at(src, in_gamma);

        let one = Target::Class.resolve_counted_dir(
            &mut doc, &view, &loader, 1, in_gamma, Direction::Backward,
        );
        assert_eq!(spans(&one), vec![py_class_body(src, "Beta")]);

        let two = Target::Class.resolve_counted_dir(
            &mut doc, &view, &loader, 2, in_gamma, Direction::Backward,
        );
        let s = spans(&two);
        assert_eq!(s.len(), 2);
        // Document order even though discovery ran backwards.
        assert_eq!(s[0], py_class_body(src, "Alpha"));
        assert_eq!(s[1], py_class_body(src, "Beta"));

        // Beginning-of-document boundary: nothing precedes Alpha's cursor.
        let at_alpha = src.find("x = 1").unwrap() + 2;
        let none = Target::Class.resolve_counted_dir(
            &mut doc, &view, &loader, 3, at_alpha, Direction::Backward,
        );
        assert_eq!(spans(&none), vec![(at_alpha, at_alpha)]);
    }

    #[test]
    fn test_python_class_backward_after_mutation_is_current() {
        let src = THREE_CLASSES_PY;
        let in_gamma = src.find("z = 3").unwrap() + 2;
        let (mut doc, view, loader) = python_doc_at(src, in_gamma);

        // Delete Alpha entirely through normal transactions.
        let a_start = src.find("class Alpha").unwrap();
        let a_end = src.find("class Beta").unwrap();
        let transaction =
            Transaction::change(doc.text(), vec![(a_start, a_end, None)].into_iter());
        doc.apply(&transaction, view.id);

        // Re-resolve backward from the CURRENT tree: only Beta precedes Gamma.
        let new_text = doc.text().slice(..).to_string();
        let new_cursor = new_text.find("z = 3").unwrap() + 2;
        let sel = Target::Class.resolve_counted_dir(
            &mut doc, &view, &loader, 2, new_cursor, Direction::Backward,
        );
        let s = spans(&sel);
        assert_eq!(s.len(), 1, "stale Alpha must not be manufactured");
        let frag = frag_at(&new_text, s[0]);
        assert!(frag.contains("def two"), "sees Beta: {frag}");
        assert!(!frag.contains("def one"), "never stale Alpha");
    }

    #[test]
    fn test_statement_target_has_no_real_capture_and_is_never_collected() {
        let src = THREE_CLASSES_PY;
        let cursor = src.find("y = 2").unwrap() + 2;
        let (mut doc, view, loader) = python_doc_at(src, cursor);

        // Plain resolve: documented width-1 selection fallback (min-width-1).
        let range = Target::Statement.resolve(&doc, &view, &loader).primary();
        assert_eq!((range.from(), range.to()), (cursor, cursor + 1));

        // Counted engines must not manufacture junk from the fallback:
        // nothing collectible -> clean point result, bounded traversal.
        for dir in [Direction::Forward, Direction::Backward] {
            let sel =
                Target::Statement.resolve_counted_dir(&mut doc, &view, &loader, 5, cursor, dir);
            assert_eq!(
                spans(&sel),
                vec![(cursor, cursor)],
                "{dir:?}: no fallback junk collected"
            );
        }
    }

    #[test]
    fn test_format_tachyon_prefix_default() {
        assert_eq!(format_tachyon_prefix(None, false, None), "t");
    }

    #[test]
    fn test_format_tachyon_prefix_forward_count() {
        assert_eq!(format_tachyon_prefix(Some(3), false, None), "t 3");
    }

    #[test]
    fn test_format_tachyon_prefix_backward_count() {
        assert_eq!(format_tachyon_prefix(Some(3), true, None), "t -3");
    }

    #[test]
    fn test_format_tachyon_prefix_with_target() {
        assert_eq!(
            format_tachyon_prefix(Some(3), false, Some(Target::Function)),
            "t 3 function"
        );
    }

    #[test]
    fn test_format_tachyon_prefix_backward_with_target() {
        assert_eq!(
            format_tachyon_prefix(Some(3), true, Some(Target::Function)),
            "t -3 function"
        );
    }

    #[test]
    fn test_format_tachyon_prefix_replacement() {
        let s = format_tachyon_prefix(Some(3), false, Some(Target::Class));
        assert_eq!(s, "t 3 class");
    }

    #[test]
    fn test_format_tachyon_prefix_backward_no_count() {
        assert_eq!(format_tachyon_prefix(None, true, None), "t -");
    }

    #[test]
    fn test_format_tachyon_prefix_count_one_forward() {
        assert_eq!(format_tachyon_prefix(Some(1), false, None), "t 1");
    }

    #[test]
    fn test_format_tachyon_prefix_count_twelve_backward() {
        assert_eq!(format_tachyon_prefix(Some(12), true, None), "t -12");
    }

    #[test]
    fn test_format_tachyon_prefix_no_geometry() {
        let s = format_tachyon_prefix(Some(1), false, Some(Target::Statement));
        assert_eq!(s, "t 1 statement");
        assert!(!s.contains("Range"));
        assert!(!s.contains("Selection"));
    }

    // ============================================================
    // Phase 38: expand real grammar coverage matrix.
    //
    // Currently tested:
    //   Rust  → Target::Function
    //   Python → Target::Class
    //
    // New:
    //   Python → Target::Function  (forward, counted, backward, mutation)
    //   Rust   → Target::Class     (struct bodies, forward, counted, backward, mutation)
    //
    // No grammar defines @expression.inside, @statement.inside, or
    // @block.inside — those targets are fallback-only by Helix query
    // reality.  These tests expand coverage for targets that genuinely
    // resolve through tree-sitter.
    // ============================================================

    // ---- Python function tests ----

    const THREE_FNS_PY: &str = "def alpha():\n    a = 1\n\n\ndef beta():\n    b = 2\n\n\ndef gamma():\n    c = 3\n";

    fn py_fn_body(src: &str, name: &str) -> (usize, usize) {
        let sig = src.find(&format!("def {name}")).unwrap();
        let colon = sig + src[sig..].find(':').unwrap();
        // Real @function.inside contract for Python: the indented block
        // contents, excluding the def line and trailing blank lines.
        let nl = colon + src[colon..].find('\n').unwrap();
        let bytes = src.as_bytes();
        let mut s = nl + 1;
        while s < src.len() && bytes[s] == b' ' {
            s += 1;
        }
        let seg_end = match src[s..].find("\ndef ") {
            Some(off) => s + off,
            None => src.len(),
        };
        let mut e = seg_end;
        while e > s && bytes[e - 1] == b'\n' {
            e -= 1;
        }
        (s, e)
    }

    #[test]
    fn test_python_function_forward_resolve_real_treesitter() {
        let src = THREE_FNS_PY;
        let in_beta = src.find("b = 2").unwrap() + 2;
        let (doc, view, loader) = python_doc_at(src, in_beta);
        let range = Target::Function.resolve(&doc, &view, &loader).primary();
        let expected = py_fn_body(src, "beta");
        assert_eq!((range.from(), range.to()), expected);
        let frag = frag_at(src, (range.from(), range.to()));
        assert!(frag.contains("b = 2"), "beta body selected: {frag}");
        assert!(!frag.contains("a = 1"), "alpha excluded: {frag}");
        assert!(!frag.contains("c = 3"), "gamma excluded: {frag}");
    }

    #[test]
    fn test_python_function_counted_forward_ordered_and_bounded() {
        let src = THREE_FNS_PY;
        let in_alpha = src.find("a = 1").unwrap() + 2;
        let (mut doc, view, loader) = python_doc_at(src, in_alpha);

        let one = Target::Function.resolve_counted(&mut doc, &view, &loader, 1, in_alpha);
        assert_eq!(spans(&one), vec![py_fn_body(src, "alpha")]);

        let two = Target::Function.resolve_counted(&mut doc, &view, &loader, 2, in_alpha);
        let s2 = spans(&two);
        assert_eq!(s2.len(), 2);
        assert_eq!(s2[0], py_fn_body(src, "alpha"));
        assert_eq!(s2[1], py_fn_body(src, "beta"));
        assert!(s2[0].1 <= s2[1].0);

        let many = Target::Function.resolve_counted(&mut doc, &view, &loader, 50, in_alpha);
        assert_eq!(spans(&many).len(), 3);
    }

    #[test]
    fn test_python_function_backward_single_counted_and_bob_boundary() {
        let src = THREE_FNS_PY;
        let in_gamma = src.find("c = 3").unwrap() + 2;
        let (mut doc, view, loader) = python_doc_at(src, in_gamma);

        let one = Target::Function.resolve_counted_dir(
            &mut doc, &view, &loader, 1, in_gamma, Direction::Backward,
        );
        assert_eq!(spans(&one), vec![py_fn_body(src, "beta")]);

        let two = Target::Function.resolve_counted_dir(
            &mut doc, &view, &loader, 2, in_gamma, Direction::Backward,
        );
        let s = spans(&two);
        assert_eq!(s.len(), 2);
        assert_eq!(s[0], py_fn_body(src, "alpha"));
        assert_eq!(s[1], py_fn_body(src, "beta"));

        // BOB boundary: nothing precedes alpha.
        let at_alpha = src.find("a = 1").unwrap() + 2;
        let none = Target::Function.resolve_counted_dir(
            &mut doc, &view, &loader, 3, at_alpha, Direction::Backward,
        );
        assert_eq!(spans(&none), vec![(at_alpha, at_alpha)]);
    }

    #[test]
    fn test_python_function_mutation_sees_current_tree() {
        let src = THREE_FNS_PY;
        let in_alpha = src.find("a = 1").unwrap() + 2;
        let (mut doc, view, loader) = python_doc_at(src, in_alpha);

        // Delete alpha entirely.
        let a_start = src.find("def alpha").unwrap();
        let a_end = src.find("def beta").unwrap();
        let transaction =
            Transaction::change(doc.text(), vec![(a_start, a_end, None)].into_iter());
        doc.apply(&transaction, view.id);

        let new_text = doc.text().slice(..).to_string();
        let resolved =
            Target::Function.resolve_counted(&mut doc, &view, &loader, 1, a_start).primary();
        let frag: String = new_text.chars().skip(resolved.from()).take(resolved.to() - resolved.from()).collect();
        assert!(frag.contains("b = 2"), "sees beta: {frag}");
        assert!(!frag.contains("a = 1"), "never stale alpha: {frag}");
    }

    #[test]
    fn test_python_function_advance_walks_without_duplicates() {
        let src = THREE_FNS_PY;
        let in_alpha = src.find("a = 1").unwrap() + 2;
        let (mut doc, view, loader) = python_doc_at(src, in_alpha);

        let first = Target::Function.resolve(&doc, &view, &loader).primary();
        let second =
            Target::Function.resolve_advance(&mut doc, &view, &loader, 1, first.head).primary();
        assert_ne!((first.from(), first.to()), (second.from(), second.to()));
        assert_eq!((second.from(), second.to()), py_fn_body(src, "beta"));

        let third =
            Target::Function.resolve_advance(&mut doc, &view, &loader, 1, second.head).primary();
        assert_eq!((third.from(), third.to()), py_fn_body(src, "gamma"));

        // Past final target: clean stop.
        let fourth =
            Target::Function.resolve_advance(&mut doc, &view, &loader, 1, third.head);
        assert_eq!(spans(&fourth), vec![(src.len(), src.len())]);
    }

    // ---- Rust class (struct) tests ----

    const THREE_STRUCTS_RS: &str = "struct Alpha {\n    a: i32,\n}\n\nstruct Beta {\n    b: i32,\n}\n\nstruct Gamma {\n    c: i32,\n}\n";

    /// Rust @class.inside for struct_item: the body block including braces.
    fn rs_struct_body(src: &str, name: &str) -> (usize, usize) {
        let sig = src.find(&format!("struct {name}")).unwrap();
        let open = sig + src[sig..].find('{').unwrap();
        // @class.inside captures `body: (_)`, which is the brace-delimited block.
        let close = open + src[open..].find('}').unwrap();
        (open, close + 1)
    }

    #[test]
    fn test_rust_struct_class_forward_resolve_real_treesitter() {
        let src = THREE_STRUCTS_RS;
        let in_beta = src.find("b: i32").unwrap() + 2;
        let (doc, view, loader) = rust_doc_at(src, in_beta);
        let range = Target::Class.resolve(&doc, &view, &loader).primary();
        let expected = rs_struct_body(src, "Beta");
        assert_eq!((range.from(), range.to()), expected);
        let frag = frag_at(src, (range.from(), range.to()));
        assert!(frag.contains("b: i32"), "beta body selected: {frag}");
        assert!(!frag.contains("a: i32"), "alpha excluded: {frag}");
        assert!(!frag.contains("c: i32"), "gamma excluded: {frag}");
    }

    #[test]
    fn test_rust_struct_class_counted_forward_ordered_and_bounded() {
        let src = THREE_STRUCTS_RS;
        let in_alpha = src.find("a: i32").unwrap() + 2;
        let (mut doc, view, loader) = rust_doc_at(src, in_alpha);

        let one = Target::Class.resolve_counted(&mut doc, &view, &loader, 1, in_alpha);
        assert_eq!(spans(&one), vec![rs_struct_body(src, "Alpha")]);

        let two = Target::Class.resolve_counted(&mut doc, &view, &loader, 2, in_alpha);
        let s2 = spans(&two);
        assert_eq!(s2.len(), 2);
        assert_eq!(s2[0], rs_struct_body(src, "Alpha"));
        assert_eq!(s2[1], rs_struct_body(src, "Beta"));

        let many = Target::Class.resolve_counted(&mut doc, &view, &loader, 50, in_alpha);
        assert_eq!(spans(&many).len(), 3);
    }

    #[test]
    fn test_rust_struct_class_backward_single_counted_and_bob_boundary() {
        let src = THREE_STRUCTS_RS;
        let in_gamma = src.find("c: i32").unwrap() + 2;
        let (mut doc, view, loader) = rust_doc_at(src, in_gamma);

        let one = Target::Class.resolve_counted_dir(
            &mut doc, &view, &loader, 1, in_gamma, Direction::Backward,
        );
        assert_eq!(spans(&one), vec![rs_struct_body(src, "Beta")]);

        let two = Target::Class.resolve_counted_dir(
            &mut doc, &view, &loader, 2, in_gamma, Direction::Backward,
        );
        let s = spans(&two);
        assert_eq!(s.len(), 2);
        assert_eq!(s[0], rs_struct_body(src, "Alpha"));
        assert_eq!(s[1], rs_struct_body(src, "Beta"));

        // BOB boundary.
        let at_alpha = src.find("a: i32").unwrap() + 2;
        let none = Target::Class.resolve_counted_dir(
            &mut doc, &view, &loader, 3, at_alpha, Direction::Backward,
        );
        assert_eq!(spans(&none), vec![(at_alpha, at_alpha)]);
    }

    #[test]
    fn test_rust_struct_class_mutation_sees_current_tree() {
        let src = THREE_STRUCTS_RS;
        let in_alpha = src.find("a: i32").unwrap() + 2;
        let (mut doc, view, loader) = rust_doc_at(src, in_alpha);

        // Delete Alpha struct entirely.
        let a_start = src.find("struct Alpha").unwrap();
        let a_end = src.find("struct Beta").unwrap();
        let transaction =
            Transaction::change(doc.text(), vec![(a_start, a_end, None)].into_iter());
        doc.apply(&transaction, view.id);

        let new_text = doc.text().slice(..).to_string();
        let resolved =
            Target::Class.resolve_counted(&mut doc, &view, &loader, 1, a_start).primary();
        let frag: String = new_text.chars().skip(resolved.from()).take(resolved.to() - resolved.from()).collect();
        assert!(frag.contains("b: i32"), "sees Beta: {frag}");
        assert!(!frag.contains("a: i32"), "never stale Alpha: {frag}");
    }

    // ============================================================
    // Phase 39: Target::Parameter — real tree-sitter @parameter.inside
    //
    // 86 grammars define @parameter.inside.  Both Rust and Python
    // have it.  The capture applies to individual parameter nodes
    // within (parameters), (type_parameters), (arguments), etc.
    //
    // These tests prove Parameter resolves through the same
    // tree-sitter mechanism as Function/Class — not via text
    // primitives like Argument's `(` pair matching.
    // ============================================================

    // ---- Python parameter tests ----
    //
    // QUERY REALITY (pinned, not invented): python's textobjects.scm uses
    // `((_) @parameter.inside . ","? ...)` whose wildcard can also match the
    // anonymous `,` separator, so separator tokens are capturable too. The
    // main fixtures therefore use ONE parameter per function to exercise
    // semantic traversal unambiguously; a dedicated test pins the separator
    // behavior explicitly.

    const PY_ONE_PARAM_FNS: &str = "def alpha(x):\n    a = 1\n\n\ndef beta(p):\n    b = 2\n";

    /// Python @parameter.inside: the identifier itself.
    fn py_param(src: &str, fn_name: &str, param_name: &str) -> (usize, usize) {
        let fn_sig = src.find(&format!("def {fn_name}")).unwrap();
        let paren_open = fn_sig + src[fn_sig..].find('(').unwrap();
        let search_start = paren_open + 1;
        let param_offset = src[search_start..].find(param_name).unwrap() + search_start;
        (param_offset, param_offset + param_name.len())
    }

    #[test]
    fn test_python_parameter_forward_resolve_real_treesitter() {
        let src = PY_ONE_PARAM_FNS;
        // Cursor ON 'x' (byte 11 would be the capturable `,`, see separator test).
        let cursor = py_param(src, "alpha", "x").0;
        let (doc, view, loader) = python_doc_at(src, cursor);
        let range = Target::Parameter.resolve(&doc, &view, &loader).primary();
        let expected = py_param(src, "alpha", "x");
        assert_eq!((range.from(), range.to()), expected);
        assert_eq!(frag_at(src, (range.from(), range.to())), "x");
    }

    #[test]
    fn test_python_parameter_counted_forward_crosses_functions() {
        let src = PY_ONE_PARAM_FNS;
        let cursor = py_param(src, "alpha", "x").0;
        let (mut doc, view, loader) = python_doc_at(src, cursor);

        let one = Target::Parameter.resolve_counted(&mut doc, &view, &loader, 1, cursor);
        assert_eq!(spans(&one), vec![py_param(src, "alpha", "x")]);

        // count=2 crosses the function boundary into beta.
        let two = Target::Parameter.resolve_counted(&mut doc, &view, &loader, 2, cursor);
        assert_eq!(
            spans(&two),
            vec![py_param(src, "alpha", "x"), py_param(src, "beta", "p")]
        );

        // Only two parameters exist: bounded termination, no junk.
        let many = Target::Parameter.resolve_counted(&mut doc, &view, &loader, 50, cursor);
        assert_eq!(spans(&many).len(), 2);
    }

    #[test]
    fn test_python_parameter_backward_and_bob_boundary() {
        let src = PY_ONE_PARAM_FNS;
        let at_p = py_param(src, "beta", "p").0;
        let (mut doc, view, loader) = python_doc_at(src, at_p);

        // Backward 1 from beta's parameter -> alpha's.
        let one = Target::Parameter.resolve_counted_dir(
            &mut doc, &view, &loader, 1, at_p, Direction::Backward,
        );
        assert_eq!(spans(&one), vec![py_param(src, "alpha", "x")]);

        // BOB boundary: nothing precedes alpha's parameter.
        let at_x = py_param(src, "alpha", "x").0;
        let none = Target::Parameter.resolve_counted_dir(
            &mut doc, &view, &loader, 3, at_x, Direction::Backward,
        );
        assert_eq!(spans(&none), vec![(at_x, at_x)], "BOB: clean point");
    }

    #[test]
    fn test_python_parameter_mutation_sees_current_tree() {
        let src = PY_ONE_PARAM_FNS;
        let cursor = py_param(src, "alpha", "x").0;
        let (mut doc, view, loader) = python_doc_at(src, cursor);

        // Delete alpha entirely (its parameter goes with it).
        let a_start = src.find("def alpha").unwrap();
        let a_end = src.find("def beta").unwrap();
        let transaction =
            Transaction::change(doc.text(), vec![(a_start, a_end, None)].into_iter());
        doc.apply(&transaction, view.id);

        let new_text = doc.text().slice(..).to_string();
        let resolved =
            Target::Parameter.resolve_counted(&mut doc, &view, &loader, 1, a_start).primary();
        let frag: String = new_text.chars().skip(resolved.from()).take(resolved.to() - resolved.from()).collect();
        assert!(frag.contains("p"), "sees beta's param: {frag}");
        assert!(!frag.contains("x"), "never stale alpha param: {frag}");
    }

    #[test]
    fn test_python_parameter_advance_no_duplicates() {
        let src = PY_ONE_PARAM_FNS;
        let cursor = py_param(src, "alpha", "x").0;
        let (mut doc, view, loader) = python_doc_at(src, cursor);

        let first = Target::Parameter.resolve(&doc, &view, &loader).primary();
        let second =
            Target::Parameter.resolve_advance(&mut doc, &view, &loader, 1, first.head).primary();
        assert_ne!((first.from(), first.to()), (second.from(), second.to()));
        assert_eq!((second.from(), second.to()), py_param(src, "beta", "p"));
    }

    #[test]
    fn test_python_parameter_separator_capture_is_pinned() {
        // Query reality: the wildcard in `((_) @parameter.inside . ","? ...)`
        // matches the anonymous `,` too, so separators are capturable units.
        // Pinned verbatim so future query changes surface immediately.
        let src = "def f(a, b):\n    x = 1\n";
        let comma = src.find(',').unwrap();
        let (doc, view, loader) = python_doc_at(src, comma);
        let range = Target::Parameter.resolve(&doc, &view, &loader).primary();
        assert_eq!((range.from(), range.to()), (comma, comma + 1));
        assert_eq!(frag_at(src, (range.from(), range.to())), ",");
    }

    // ---- Rust parameter tests ----

    const RS_TWO_FNS_PARAMS: &str = "fn alpha(x: i32, y: i32) {\n    let a = 1;\n}\n\nfn beta(p: bool, q: String) {\n    let b = 2;\n}\n";

    /// Rust @parameter.inside: each parameter within (parameters) is a
    /// separate capture.  For `fn alpha(x: i32, y: i32)`, `x: i32`
    /// is one parameter and `y: i32` is another.
    fn rs_param(src: &str, fn_name: &str, param_text: &str) -> (usize, usize) {
        let fn_sig = src.find(&format!("fn {fn_name}")).unwrap();
        let paren_open = fn_sig + src[fn_sig..].find('(').unwrap();
        let search_start = paren_open + 1;
        let param_offset = src[search_start..].find(param_text).unwrap() + search_start;
        (param_offset, param_offset + param_text.len())
    }

    #[test]
    fn test_rust_parameter_forward_resolve_real_treesitter() {
        let src = RS_TWO_FNS_PARAMS;
        // Cursor on 'x: i32' in alpha's parameters.
        let cursor = src.find("fn alpha(").unwrap() + "fn alpha(".len();
        let (doc, view, loader) = rust_doc_at(src, cursor);
        let range = Target::Parameter.resolve(&doc, &view, &loader).primary();
        let expected = rs_param(src, "alpha", "x: i32");
        assert_eq!((range.from(), range.to()), expected);
        let frag = frag_at(src, (range.from(), range.to()));
        assert_eq!(frag, "x: i32");
    }

    #[test]
    fn test_rust_parameter_counted_forward() {
        let src = RS_TWO_FNS_PARAMS;
        let cursor = src.find("fn alpha(").unwrap() + "fn alpha(".len();
        let (mut doc, view, loader) = rust_doc_at(src, cursor);

        let one = Target::Parameter.resolve_counted(&mut doc, &view, &loader, 1, cursor);
        assert_eq!(spans(&one), vec![rs_param(src, "alpha", "x: i32")]);

        let two = Target::Parameter.resolve_counted(&mut doc, &view, &loader, 2, cursor);
        let s2 = spans(&two);
        assert_eq!(s2.len(), 2);
        assert_eq!(s2[0], rs_param(src, "alpha", "x: i32"));
        assert_eq!(s2[1], rs_param(src, "alpha", "y: i32"));

        // count=3 crosses into beta's parameters.
        let three = Target::Parameter.resolve_counted(&mut doc, &view, &loader, 3, cursor);
        let s3 = spans(&three);
        assert_eq!(s3.len(), 3);
        assert_eq!(s3[0], rs_param(src, "alpha", "x: i32"));
        assert_eq!(s3[1], rs_param(src, "alpha", "y: i32"));
        assert_eq!(s3[2], rs_param(src, "beta", "p: bool"));
    }

    #[test]
    fn test_rust_parameter_backward() {
        let src = RS_TWO_FNS_PARAMS;
        let cursor = rs_param(src, "beta", "q: String").0 + 1;
        let (mut doc, view, loader) = rust_doc_at(src, cursor);

        let one = Target::Parameter.resolve_counted_dir(
            &mut doc, &view, &loader, 1, cursor, Direction::Backward,
        );
        assert_eq!(spans(&one), vec![rs_param(src, "beta", "p: bool")]);

        let two = Target::Parameter.resolve_counted_dir(
            &mut doc, &view, &loader, 2, cursor, Direction::Backward,
        );
        let s = spans(&two);
        assert_eq!(s.len(), 2);
        assert_eq!(s[0], rs_param(src, "alpha", "y: i32"));
        assert_eq!(s[1], rs_param(src, "beta", "p: bool"));
    }

    #[test]
    fn test_rust_parameter_mutation_sees_current_tree() {
        let src = RS_TWO_FNS_PARAMS;
        let cursor = src.find("fn alpha(").unwrap() + "fn alpha(".len();
        let (mut doc, view, loader) = rust_doc_at(src, cursor);

        // Delete alpha entirely.
        let a_start = src.find("fn alpha").unwrap();
        let a_end = src.find("fn beta").unwrap();
        let transaction =
            Transaction::change(doc.text(), vec![(a_start, a_end, None)].into_iter());
        doc.apply(&transaction, view.id);

        let new_text = doc.text().slice(..).to_string();
        let resolved =
            Target::Parameter.resolve_counted(&mut doc, &view, &loader, 1, a_start).primary();
        let frag: String = new_text.chars().skip(resolved.from()).take(resolved.to() - resolved.from()).collect();
        assert!(frag.contains("p: bool"), "sees beta's params: {frag}");
        assert!(!frag.contains("x: i32"), "never stale alpha params: {frag}");
    }

    #[test]
    fn test_rust_parameter_advance_no_duplicates() {
        let src = RS_TWO_FNS_PARAMS;
        let cursor = rs_param(src, "alpha", "x: i32").0;
        let (mut doc, view, loader) = rust_doc_at(src, cursor);

        let first = Target::Parameter.resolve(&doc, &view, &loader).primary();
        let second =
            Target::Parameter.resolve_advance(&mut doc, &view, &loader, 1, first.head).primary();
        assert_ne!((first.from(), first.to()), (second.from(), second.to()));
        assert_eq!((second.from(), second.to()), rs_param(src, "alpha", "y: i32"));
    }

    #[test]
    fn test_parameter_no_syntax_falls_back_to_selection() {
        let src = RS_TWO_FNS_PARAMS;
        let loader = test_loader();
        let mut doc = Document::from(
            Rope::from(src),
            None,
            std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(EditorConfig::default())),
            loader.clone(),
        );
        // No language set — syntax unavailable.
        let view = View::new(doc.id(), GutterConfig::default());
        let cursor = src.find("x: i32").unwrap() + 2;
        doc.set_selection(view.id, Selection::point(cursor));
        let range = Target::Parameter.resolve(&doc, &view, &loader).primary();
        assert_eq!((range.from(), range.to()), (cursor, cursor + 1), "fallback to selection");
    }

    #[test]
    fn test_parse_target_spec_parameter_key() {
        // `t n` -> parameter, Forward, count 1.
        assert_eq!(
            parse_target_spec(&['n']),
            Some((1, Direction::Forward, Target::Parameter))
        );
        assert_eq!(
            parse_target_spec(&['3', 'n']),
            Some((3, Direction::Forward, Target::Parameter))
        );
        assert_eq!(
            parse_target_spec(&['-', '2', 'n']),
            Some((2, Direction::Backward, Target::Parameter))
        );
        assert_eq!(
            parse_target_spec(&['-', 'n']),
            Some((1, Direction::Backward, Target::Parameter))
        );
        // Key round-trip.
        assert_eq!(Target::from_name("parameter"), Some(Target::Parameter));
        assert_eq!(Target::Parameter.key(), 'n');
        // Explorer prefix rendering.
        assert_eq!(
            format_tachyon_prefix(Some(3), false, Some(Target::Parameter)),
            "t 3 parameter"
        );
        assert_eq!(
            format_tachyon_prefix(Some(2), true, Some(Target::Parameter)),
            "t -2 parameter"
        );
        // Exactly one help entry; name/key agree.
        let entries: Vec<_> = Target::targets_help()
            .iter()
            .filter(|(name, _, _)| *name == "parameter")
            .collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].1, 'n');
    }

    // ============================================================
    // Phase 40: delimiter-safe Parameter deletion (real trees).
    //
    // `t n d` removes the separator together with the parameter so
    // `foo(alpha, beta, gamma)` stays structurally valid. Scoped strictly
    // to Target::Parameter + Action::Delete; yank/change/indent keep the
    // plain selection.
    // ============================================================

    /// Production-equivalent deletion for a prepared Selection.
    /// Resolve Parameter at `cursor`, run the delimiter-aware adjuster, apply
    /// the deletion through a normal Transaction, and return the new text.
    fn delete_param_at(loader_doc_cursor: (&TestLoader, &str, usize)) -> String {
        let (_loader, src, cursor) = loader_doc_cursor;
        let (mut doc, view, loader) = python_doc_at(src, cursor);
        let resolved = Target::Parameter.resolve(&doc, &view, &loader).primary();
        let del =
            parameter_delete_selection(&doc, &loader, &Selection::single(resolved.from(), resolved.to()));
        let transaction =
            Transaction::change(doc.text(), del.ranges().iter().map(|r| (r.from(), r.to(), None)));
        doc.apply(&transaction, view.id);
        doc.text().slice(..).to_string()
    }

    const PY3_PARAMS: &str = "def f(alpha, beta, gamma):\n    x = 1\n";
    const PY1_PARAM: &str = "def g(only):\n    y = 2\n";
    const RS3_PARAMS: &str = "fn f(alpha: i32, beta: i32, gamma: i32) {\n    let x = 1;\n}\n";
    const RS1_PARAM: &str = "fn g(only: u8) {\n    let y = 2;\n}\n";

    #[test]
    fn test_py_param_delete_first_middle_last_only() {
        // FIRST: trailing separator consumed -> one clean space remains.
        let cur = PY3_PARAMS.find("alpha").unwrap();
        assert_eq!(
            delete_param_at((&test_loader(), PY3_PARAMS, cur)),
            "def f(beta, gamma):\n    x = 1\n"
        );
        // MIDDLE.
        let cur = PY3_PARAMS.find("beta").unwrap();
        assert_eq!(
            delete_param_at((&test_loader(), PY3_PARAMS, cur)),
            "def f(alpha, gamma):\n    x = 1\n"
        );
        // LAST: preceding separator consumed.
        let cur = PY3_PARAMS.find("gamma").unwrap();
        assert_eq!(
            delete_param_at((&test_loader(), PY3_PARAMS, cur)),
            "def f(alpha, beta):\n    x = 1\n"
        );
        // ONLY parameter: empty but valid list.
        let cur = PY1_PARAM.find("only").unwrap();
        assert_eq!(
            delete_param_at((&test_loader(), PY1_PARAM, cur)),
            "def g():\n    y = 2\n"
        );
    }

    #[test]
    fn test_rs_param_delete_first_middle_last_only() {
        let cur = RS3_PARAMS.find("alpha").unwrap();
        assert_eq!(
            {
                let (mut doc, view, loader) = rust_doc_at(RS3_PARAMS, cur);
                let resolved = Target::Parameter.resolve(&doc, &view, &loader).primary();
                let del = parameter_delete_selection(
                    &doc,
                    &loader,
                    &Selection::single(resolved.from(), resolved.to()),
                );
                let transaction = Transaction::change(
                    doc.text(),
                    del.ranges().iter().map(|r| (r.from(), r.to(), None)),
                );
                doc.apply(&transaction, view.id);
                doc.text().slice(..).to_string()
            },
            "fn f(beta: i32, gamma: i32) {\n    let x = 1;\n}\n"
        );

        let cur = RS3_PARAMS.find("beta").unwrap();
        assert_eq!(
            {
                let (mut doc, view, loader) = rust_doc_at(RS3_PARAMS, cur);
                let resolved = Target::Parameter.resolve(&doc, &view, &loader).primary();
                let del = parameter_delete_selection(
                    &doc,
                    &loader,
                    &Selection::single(resolved.from(), resolved.to()),
                );
                let transaction = Transaction::change(
                    doc.text(),
                    del.ranges().iter().map(|r| (r.from(), r.to(), None)),
                );
                doc.apply(&transaction, view.id);
                doc.text().slice(..).to_string()
            },
            "fn f(alpha: i32, gamma: i32) {\n    let x = 1;\n}\n"
        );

        let cur = RS3_PARAMS.find("gamma").unwrap();
        assert_eq!(
            {
                let (mut doc, view, loader) = rust_doc_at(RS3_PARAMS, cur);
                let resolved = Target::Parameter.resolve(&doc, &view, &loader).primary();
                let del = parameter_delete_selection(
                    &doc,
                    &loader,
                    &Selection::single(resolved.from(), resolved.to()),
                );
                let transaction = Transaction::change(
                    doc.text(),
                    del.ranges().iter().map(|r| (r.from(), r.to(), None)),
                );
                doc.apply(&transaction, view.id);
                doc.text().slice(..).to_string()
            },
            "fn f(alpha: i32, beta: i32) {\n    let x = 1;\n}\n"
        );

        let cur = RS1_PARAM.find("only").unwrap();
        assert_eq!(
            {
                let (mut doc, view, loader) = rust_doc_at(RS1_PARAM, cur);
                let resolved = Target::Parameter.resolve(&doc, &view, &loader).primary();
                let del = parameter_delete_selection(
                    &doc,
                    &loader,
                    &Selection::single(resolved.from(), resolved.to()),
                );
                let transaction = Transaction::change(
                    doc.text(),
                    del.ranges().iter().map(|r| (r.from(), r.to(), None)),
                );
                doc.apply(&transaction, view.id);
                doc.text().slice(..).to_string()
            },
            "fn g() {\n    let y = 2;\n}\n"
        );
    }

    #[test]
    fn test_py_param_counted_two_deletion_merges_region() {
        // t 2 n d on (alpha, beta, gamma): both parameters AND their shared
        // separators go in ONE merged span -> exactly `gamma` remains.
        let src = PY3_PARAMS;
        let cur = src.find("alpha").unwrap();
        let (mut doc, view, loader) = python_doc_at(src, cur);
        let sel = Target::Parameter.resolve_counted_dir(
            &mut doc, &view, &loader, 2, cur, Direction::Forward,
        );
        let del = parameter_delete_selection(&doc, &loader, &sel);
        assert_eq!(del.ranges().len(), 1, "adjacent expansions merge into one span");
        let transaction =
            Transaction::change(doc.text(), del.ranges().iter().map(|r| (r.from(), r.to(), None)));
        doc.apply(&transaction, view.id);
        assert_eq!(doc.text().slice(..).to_string(), "def f(gamma):\n    x = 1\n");
    }

    // ============================================================
    // Phase 41: counted BACKWARD delimiter-aware deletion.
    //
    // Engine contract pinned empirically against the real grammar:
    //   - the object CONTAINING the scan position is skipped; backward
    //     collects objects strictly before it, document-ordered;
    //   - separator tokens are never collected by traversal in either
    //     direction;
    //   - when nothing qualifies, the engine returns a zero-width point,
    //     which the adjuster passes through untouched (inert).
    // The Phase 40 adjuster + merge logic is reused unchanged.
    // ============================================================

    const PY4_PARAMS: &str = "def f(alpha, beta, gamma, delta):\n    x = 1\n";

    /// Engine-shaped backward deletion: resolve `count` params Backward from
    /// `pos`, run the delimiter-aware adjuster, apply ONE Transaction.
    fn py_backward_delete(src: &str, pos: usize, count: usize) -> String {
        let (mut doc, view, loader) = python_doc_at(src, pos);
        let sel = Target::Parameter.resolve_counted_dir(
            &mut doc, &view, &loader, count, pos, Direction::Backward,
        );
        let del = parameter_delete_selection(&doc, &loader, &sel);
        let transaction =
            Transaction::change(doc.text(), del.ranges().iter().map(|r| (r.from(), r.to(), None)));
        doc.apply(&transaction, view.id);
        doc.text().slice(..).to_string()
    }

    #[test]
    fn test_py_param_backward_single_from_after_last() {
        let pos = PY4_PARAMS.find("):").unwrap() + 1; // right after delta
        assert_eq!(
            py_backward_delete(PY4_PARAMS, pos, 1),
            "def f(alpha, beta, gamma):\n    x = 1\n"
        );
    }

    #[test]
    fn test_py_param_backward_two_from_after_last_merges_tail() {
        let pos = PY4_PARAMS.find("):").unwrap() + 1;
        assert_eq!(
            py_backward_delete(PY4_PARAMS, pos, 2),
            "def f(alpha, beta):\n    x = 1\n"
        );
    }

    #[test]
    fn test_py_param_backward_middle_two_skips_containing_object() {
        // Cursor inside delta: delta itself is skipped -> beta+gamma go,
        // leaving alpha and delta with exactly one healthy separator.
        let pos = PY4_PARAMS.find("delta").unwrap() + 2;
        assert_eq!(
            py_backward_delete(PY4_PARAMS, pos, 2),
            "def f(alpha, delta):\n    x = 1\n"
        );
    }

    #[test]
    fn test_py_param_backward_first_two_from_inside_third() {
        let pos = PY4_PARAMS.find("gamma").unwrap();
        assert_eq!(
            py_backward_delete(PY4_PARAMS, pos, 2),
            "def f(gamma, delta):\n    x = 1\n"
        );
    }

    #[test]
    fn test_py_param_backward_overcount_at_first_param_is_inert() {
        // Nothing exists strictly before alpha: engine yields a zero-width
        // point ON alpha's capture. The adjuster must NOT expand it into a
        // fabricated deletion — document stays byte-identical.
        let pos = PY4_PARAMS.find("alpha").unwrap();
        assert_eq!(py_backward_delete(PY4_PARAMS, pos, 50), PY4_PARAMS);
    }

    #[test]
    fn test_py_param_single_list_backward_still_valid_and_inert_cases() {
        let src = "def g(only):\n    y = 2\n";
        // After `only`: deletes it -> valid empty list.
        let pos = src.find("):").unwrap() + 1;
        assert_eq!(py_backward_delete(src, pos, 1), "def g():\n    y = 2\n");
        // Inside `only` at list start: nothing strictly before -> inert.
        let pos = src.find("only").unwrap();
        assert_eq!(py_backward_delete(src, pos, 3), src);
    }

    #[test]
    fn test_rs_param_backward_counted_merge() {
        let src = RS3_PARAMS;
        let pos = src.find(") {").unwrap(); // after gamma
        let (mut doc, view, loader) = rust_doc_at(src, pos);
        let sel = Target::Parameter.resolve_counted_dir(
            &mut doc, &view, &loader, 2, pos, Direction::Backward,
        );
        let del = parameter_delete_selection(&doc, &loader, &sel);
        let transaction =
            Transaction::change(doc.text(), del.ranges().iter().map(|r| (r.from(), r.to(), None)));
        doc.apply(&transaction, view.id);
        assert_eq!(
            doc.text().slice(..).to_string(),
            "fn f(alpha: i32) {\n    let x = 1;\n}\n"
        );
    }

    #[test]
    fn test_py_param_backward_deleted_document_reparses() {
        let result = py_backward_delete(
            PY4_PARAMS,
            PY4_PARAMS.find("):").unwrap() + 1,
            2,
        );
        assert_eq!(result, "def f(alpha, beta):\n    x = 1\n");

        let loader = test_loader();
        let mut doc = Document::from(
            Rope::from(result.clone()),
            None,
            std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(EditorConfig::default())),
            loader.clone(),
        );
        doc.set_language_by_language_id("python", &loader.load()).unwrap();
        assert!(doc.syntax().is_some(), "backward-deleted source reparses");
        let view = View::new(doc.id(), GutterConfig::default());
        let probe = result.find("alpha").unwrap();
        doc.set_selection(view.id, Selection::point(probe));
        let remaining =
            Target::Parameter.resolve_counted_dir(&mut doc, &view, &loader, 50, probe, Direction::Forward);
        let s = spans(&remaining);
        assert_eq!(s.len(), 2, "exactly two parameters survive");
        let frags: Vec<String> = s
            .iter()
            .map(|&(a, b)| result[a..b].to_string())
            .collect();
        assert_eq!(frags, vec!["alpha".to_string(), "beta".to_string()]);
    }

    #[test]
    fn test_py_param_backward_repeat_resolves_current_tree() {
        // Production-shaped `t -n d .` chain: each round re-resolves against
        // the mutated document; no geometry is carried between rounds — only
        // the next cursor offset derived from what THIS round deleted.
        let mut text = PY4_PARAMS.to_string();
        let mut cursor = text.find("):").unwrap() + 1;

        for expected in [
            "def f(alpha, beta, gamma):\n    x = 1\n",
            "def f(alpha, beta):\n    x = 1\n",
            "def f(alpha):\n    x = 1\n",
        ] {
            let loader = test_loader();
            let mut doc = Document::from(
                Rope::from(text.clone()),
                None,
                std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(EditorConfig::default())),
                loader.clone(),
            );
            doc.set_language_by_language_id("python", &loader.load()).unwrap();
            let view = View::new(doc.id(), GutterConfig::default());
            let probe_pos = cursor.min(text.len());
            doc.set_selection(view.id, Selection::point(probe_pos));
            let sel = Target::Parameter.resolve_counted_dir(
                &mut doc, &view, &loader, 1, probe_pos, Direction::Backward,
            );
            let del = parameter_delete_selection(&doc, &loader, &sel);
            let removed_from = del.primary().from();
            let transaction = Transaction::change(
                doc.text(),
                del.ranges().iter().map(|r| (r.from(), r.to(), None)),
            );
            doc.apply(&transaction, view.id);
            text = doc.text().slice(..).to_string();
            assert_eq!(text, expected);
            // Scope-removing action: continue from where this round bit.
            cursor = removed_from.min(text.len());
        }
    }

    // ============================================================
    // Phase 42: MULTILINE signatures — the newline contract.
    //
    // Locked against the real Rust grammar (trailing-comma list shape).
    // Invariant: separator cleanup consumes horizontal whitespace ONLY;
    // a newline is consumed solely when it structurally belongs to the
    // deleted parameter's own line (left-anchored span). The operation is
    // NOT a formatter: whatever layout survives is left verbatim.
    // ============================================================

    const RS_MULTILINE: &str =
        "fn example(\n    alpha: i32,\n    beta: i32,\n    gamma: i32,\n) {\n}\n";

    /// Resolve one Parameter at `pos` and run the production-shaped deletion.
    fn rs_multiline_delete_at(pos: usize) -> String {
        let (mut doc, view, loader) = rust_doc_at(RS_MULTILINE, pos);
        let sel = Target::Parameter.resolve_counted_dir(
            &mut doc, &view, &loader, 1, pos, Direction::Forward,
        );
        let del = parameter_delete_selection(&doc, &loader, &sel);
        let transaction =
            Transaction::change(doc.text(), del.ranges().iter().map(|r| (r.from(), r.to(), None)));
        doc.apply(&transaction, view.id);
        doc.text().slice(..).to_string()
    }

    // ============================================================
    // Phase 43: Python MULTILINE signatures — grammar-independence.
    //
    // Same fixture shape as the Rust Phase 42 block, resolved through the
    // real Python grammar (whose wildcard query also captures `,` tokens —
    // see the Phase 39 separator pin). The Phase 42 whitespace/newline
    // contract must hold unchanged: capture-driven edges, horizontal-only
    // free whitespace, own-line newlines move with their parameter.
    // ============================================================

    const PY_MULTILINE: &str = "def example(\n    alpha,\n    beta,\n    gamma,\n):\n    pass\n";

    /// Resolve one Parameter at `pos` and run the production-shaped deletion.
    fn py_multiline_delete_at(pos: usize) -> String {
        let (mut doc, view, loader) = python_doc_at(PY_MULTILINE, pos);
        let sel = Target::Parameter.resolve_counted_dir(
            &mut doc, &view, &loader, 1, pos, Direction::Forward,
        );
        let del = parameter_delete_selection(&doc, &loader, &sel);
        let transaction =
            Transaction::change(doc.text(), del.ranges().iter().map(|r| (r.from(), r.to(), None)));
        doc.apply(&transaction, view.id);
        doc.text().slice(..).to_string()
    }

    #[test]
    fn test_py_multiline_middle_parameter_delete_keeps_layout() {
        let pos = PY_MULTILINE.find("beta").unwrap();
        assert_eq!(
            py_multiline_delete_at(pos),
            "def example(\n    alpha,\n    gamma,\n):\n    pass\n"
        );
    }

    #[test]
    fn test_py_multiline_first_parameter_delete_leaves_no_orphan_indent() {
        let pos = PY_MULTILINE.find("alpha").unwrap();
        let result = py_multiline_delete_at(pos);
        assert_eq!(
            result,
            "def example(\n    beta,\n    gamma,\n):\n    pass\n",
            "alpha's line incl. indent and terminator removed as a unit"
        );
        assert!(!result.contains(" \n"), "no orphaned indentation");
    }

    #[test]
    fn test_py_multiline_last_parameter_delete_keeps_trailing_comma_and_paren() {
        let pos = PY_MULTILINE.find("gamma").unwrap();
        assert_eq!(
            py_multiline_delete_at(pos),
            "def example(\n    alpha,\n    beta,\n):\n    pass\n",
            "closing paren and beta's trailing comma untouched"
        );
    }

    #[test]
    fn test_py_multiline_counted_two_delete_and_reparse() {
        let pos = PY_MULTILINE.find("alpha").unwrap();
        let (mut doc, view, loader) = python_doc_at(PY_MULTILINE, pos);
        let sel = Target::Parameter.resolve_counted_dir(
            &mut doc, &view, &loader, 2, pos, Direction::Forward,
        );
        let del = parameter_delete_selection(&doc, &loader, &sel);
        assert_eq!(del.ranges().len(), 1, "contiguous group merges");
        let transaction =
            Transaction::change(doc.text(), del.ranges().iter().map(|r| (r.from(), r.to(), None)));
        doc.apply(&transaction, view.id);
        let result = doc.text().slice(..).to_string();
        assert_eq!(result, "def example(\n    gamma,\n):\n    pass\n");

        // Reparse with the REAL Python grammar; exactly gamma survives.
        let loader = test_loader();
        let mut doc = Document::from(
            Rope::from(result.clone()),
            None,
            std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(EditorConfig::default())),
            loader.clone(),
        );
        doc.set_language_by_language_id("python", &loader.load()).unwrap();
        assert!(doc.syntax().is_some(), "python multiline-deleted source reparses");
        let view = View::new(doc.id(), GutterConfig::default());
        let probe = result.find("gamma").unwrap();
        doc.set_selection(view.id, Selection::point(probe));
        let remaining = Target::Parameter.resolve_counted_dir(
            &mut doc, &view, &loader, 50, probe, Direction::Forward,
        );
        let s = spans(&remaining);
        assert_eq!(s.len(), 1, "exactly one parameter survives");
        assert_eq!(&result[s[0].0..s[0].1], "gamma");
    }

    // ============================================================
    // Phase 44: CRLF contract — regression tests for a real defect.
    //
    // Before the fix, head-parameter deletion on CRLF sources left an
    // orphaned "\r\n" blank line: the own-line terminator check matched
    // only '\n'. The line-ending extension now accepts "\r?\n". LF
    // behavior is byte-identical to Phase 42/43 (guarded by those suites).
    // ============================================================

    const RS_CRLF: &str =
        "fn example(\r\n    alpha: i32,\r\n    beta: i32,\r\n    gamma: i32,\r\n) {\r\n}\r\n";

    fn rs_crlf_delete_at(pos: usize) -> String {
        let (mut doc, view, loader) = rust_doc_at(RS_CRLF, pos);
        let sel = Target::Parameter.resolve_counted_dir(
            &mut doc, &view, &loader, 1, pos, Direction::Forward,
        );
        let del = parameter_delete_selection(&doc, &loader, &sel);
        let transaction =
            Transaction::change(doc.text(), del.ranges().iter().map(|r| (r.from(), r.to(), None)));
        doc.apply(&transaction, view.id);
        doc.text().slice(..).to_string()
    }

    #[test]
    fn test_rs_crlf_middle_delete_preserves_line_endings() {
        let pos = RS_CRLF.find("beta").unwrap();
        assert_eq!(
            rs_crlf_delete_at(pos),
            "fn example(\r\n    alpha: i32,\r\n    gamma: i32,\r\n) {\r\n}\r\n"
        );
    }

    #[test]
    fn test_rs_crlf_first_delete_leaves_no_orphan_cr() {
        let pos = RS_CRLF.find("alpha").unwrap();
        let result = rs_crlf_delete_at(pos);
        assert_eq!(
            result,
            "fn example(\r\n    beta: i32,\r\n    gamma: i32,\r\n) {\r\n}\r\n",
            "own-line \\r\\n consumed; no blank line"
        );
        assert!(!result.contains("\r\n\r\n"), "no orphan CRLF blank line");
        assert!(!result.contains(" \r"), "no stray indent before CR");
    }

    #[test]
    fn test_rs_crlf_last_delete_keeps_trailing_comma_and_endings() {
        let pos = RS_CRLF.find("gamma").unwrap();
        let result = rs_crlf_delete_at(pos);
        assert_eq!(
            result,
            "fn example(\r\n    alpha: i32,\r\n    beta: i32,\r\n) {\r\n}\r\n"
        );
        assert_eq!(result.matches("\r\n").count(), 5, "all CRLF endings intact");
    }

    #[test]
    fn test_rs_crlf_counted_two_delete_and_reparse() {
        let pos = RS_CRLF.find("alpha").unwrap();
        let (mut doc, view, loader) = rust_doc_at(RS_CRLF, pos);
        let sel = Target::Parameter.resolve_counted_dir(
            &mut doc, &view, &loader, 2, pos, Direction::Forward,
        );
        let del = parameter_delete_selection(&doc, &loader, &sel);
        assert_eq!(del.ranges().len(), 1);
        let transaction =
            Transaction::change(doc.text(), del.ranges().iter().map(|r| (r.from(), r.to(), None)));
        doc.apply(&transaction, view.id);
        let result = doc.text().slice(..).to_string();
        assert_eq!(result, "fn example(\r\n    gamma: i32,\r\n) {\r\n}\r\n");
        assert!(!result.contains("\r\n\r\n"));

        // Reparse with the real Rust grammar; exactly gamma survives.
        let loader = test_loader();
        let mut doc = Document::from(
            Rope::from(result.clone()),
            None,
            std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(EditorConfig::default())),
            loader.clone(),
        );
        doc.set_language_by_language_id("rust", &loader.load()).unwrap();
        assert!(doc.syntax().is_some());
        let view = View::new(doc.id(), GutterConfig::default());
        let probe = result.find("gamma").unwrap();
        doc.set_selection(view.id, Selection::point(probe));
        let remaining = Target::Parameter.resolve_counted_dir(
            &mut doc, &view, &loader, 50, probe, Direction::Forward,
        );
        let s = spans(&remaining);
        assert_eq!(s.len(), 1);
        assert_eq!(&result[s[0].0..s[0].1], "gamma: i32");
    }

    // ============================================================
    // Phase 45: Target::Argument audit pins.
    //
    // QUERY REALITY: zero bundled grammars define @argument.{inside,outer,
    // around} (verified by repository search). Helix's corpus convention
    // folds CALL-SITE arguments into @parameter.inside instead — e.g. Rust
    // captures `(arguments)` children, Python captures `argument_list`
    // children. These tests pin both facts so the text-based Argument
    // target's scope stays an informed decision.
    // ============================================================

    const RS_CALLSITE: &str = "fn main() {\n    foo(alpha, beta, gamma);\n}\n";

    #[test]
    fn test_callsite_arguments_resolve_through_parameter_captures() {
        // Cursor inside the middle call argument: Target::Parameter already
        // reaches call-site semantics via the bundled @parameter.inside.
        let src = RS_CALLSITE;
        let pos = src.find("beta").unwrap();
        let (mut doc, view, loader) = rust_doc_at(src, pos);
        let sel =
            Target::Parameter.resolve_counted_dir(&mut doc, &view, &loader, 2, pos, Direction::Forward);
        let s = spans(&sel);
        assert_eq!(s.len(), 2);
        assert_eq!(&src[s[0].0..s[0].1], "beta");
        assert_eq!(&src[s[1].0..s[1].1], "gamma");
    }

    // ============================================================
    // Phase 46: cross-grammar matrix — REAL Go grammar.
    //
    // Bundled go/textobjects.scm provides all three high-value captures:
    //   @function.inside  <- function/method/literal BODY BLOCK (braces in)
    //   @class.inside     <- struct field-list / interface methods contents
    //   @parameter.inside <- parameter_list / type_parameter_list / argument_list
    // Go groups same-type params into ONE parameter_declaration ("a, b int"),
    // so fixtures use distinct types to force per-parameter nodes.
    // ============================================================

    const THREE_GOFNS: &str =
        "package main\n\nfunc alpha() {\n\tx := 1\n}\n\nfunc beta() {\n\ty := 2\n}\n\nfunc gamma() {\n\tz := 3\n}\n";

    /// Body-block span of `func <name>` inclusive of braces (measured contract).
    fn go_fn_body(src: &str, name: &str) -> (usize, usize) {
        let sig = src.find(&format!("func {name}")).unwrap();
        let open = sig + src[sig..].find('{').unwrap();
        let close = open + src[open..].find('}').unwrap();
        (open, close + 1)
    }

    #[test]
    fn test_go_function_forward_counted_and_backward() {
        let src = THREE_GOFNS;
        let in_alpha = src.find("x := 1").unwrap();
        let (mut doc, view, loader) = go_doc_at(src, in_alpha);

        // Single forward: alpha's body block.
        let one =
            Target::Function.resolve_counted_dir(&mut doc, &view, &loader, 1, in_alpha, Direction::Forward);
        assert_eq!(spans(&one), vec![go_fn_body(src, "alpha")]);

        // Counted forward: ordered, non-overlapping.
        let two =
            Target::Function.resolve_counted_dir(&mut doc, &view, &loader, 2, in_alpha, Direction::Forward);
        assert_eq!(spans(&two), vec![go_fn_body(src, "alpha"), go_fn_body(src, "beta")]);

        // Over-count terminates with what exists.
        let many =
            Target::Function.resolve_counted_dir(&mut doc, &view, &loader, 50, in_alpha, Direction::Forward);
        assert_eq!(spans(&many).len(), 3);

        // Backward from gamma: document-ordered even though discovery ran back.
        let in_gamma = src.find("z := 3").unwrap();
        let back =
            Target::Function.resolve_counted_dir(&mut doc, &view, &loader, 2, in_gamma, Direction::Backward);
        let s = spans(&back);
        assert_eq!(s.len(), 2);
        assert_eq!(s[0], go_fn_body(src, "alpha"));
        assert_eq!(s[1], go_fn_body(src, "beta"));
    }

    #[test]
    fn test_go_function_advance_walks_and_mutation_is_current() {
        let src = THREE_GOFNS;
        let in_alpha = src.find("x := 1").unwrap();
        let (mut doc, view, loader) = go_doc_at(src, in_alpha);

        // Advance chain without duplicates.
        let first = Target::Function.resolve(&doc, &view, &loader).primary();
        let second =
            Target::Function.resolve_advance(&mut doc, &view, &loader, 1, first.head).primary();
        assert_eq!((second.from(), second.to()), go_fn_body(src, "beta"));

        // Mutation: delete alpha entirely; resolver must see the CURRENT tree.
        let a_start = src.find("func alpha").unwrap();
        let a_end = src.find("func beta").unwrap();
        let transaction =
            Transaction::change(doc.text(), vec![(a_start, a_end, None)].into_iter());
        doc.apply(&transaction, view.id);
        let new_text = doc.text().slice(..).to_string();
        let resolved =
            Target::Function.resolve_counted(&mut doc, &view, &loader, 1, a_start).primary();
        let frag: String =
            new_text.chars().skip(resolved.from()).take(resolved.to() - resolved.from()).collect();
        assert!(frag.contains("y := 2"), "sees beta after mutation: {frag}");
        assert!(!frag.contains("x := 1"), "never stale alpha: {frag}");
    }

    const GO_PARAMS: &str =
        "package main\n\nfunc calc(width int, height int, depth int) int {\n\treturn width * depth\n}\n";

    fn go_param(src: &str, name: &str) -> (usize, usize) {
        let sig = src.find("func calc(").unwrap() + "func calc(".len();
        let off = src[sig..].find(name).unwrap() + sig;
        // Measured Go contract: @parameter.inside captures the WHOLE
        // parameter_declaration node ("height int"), like Rust's "x: i32".
        let decl = format!("{name} int");
        (off, off + decl.len())
    }

    #[test]
    fn test_go_parameter_resolution_counted_and_backward() {
        let src = GO_PARAMS;
        let pos = go_param(src, "height").0;
        let (doc, view, loader) = go_doc_at(src, pos);
        let r = Target::Parameter.resolve(&doc, &view, &loader).primary();
        assert_eq!((r.from(), r.to()), go_param(src, "height"));

        let (mut doc, view, loader) = go_doc_at(src, go_param(src, "width").0);
        let two =
            Target::Parameter.resolve_counted_dir(&mut doc, &view, &loader, 2, go_param(src, "width").0, Direction::Forward);
        assert_eq!(
            spans(&two),
            vec![go_param(src, "width"), go_param(src, "height")],
            "distinct types force one declaration each"
        );
        let many =
            Target::Parameter.resolve_counted_dir(&mut doc, &view, &loader, 50, go_param(src, "width").0, Direction::Forward);
        assert_eq!(spans(&many).len(), 3);

        // Backward from depth -> height; BOB-safe from width.
        let (mut doc, view, loader) = go_doc_at(src, go_param(src, "depth").0);
        let back = Target::Parameter.resolve_counted_dir(
            &mut doc, &view, &loader, 1, go_param(src, "depth").0, Direction::Backward,
        );
        assert_eq!(spans(&back), vec![go_param(src, "height")]);
        let bob = Target::Parameter.resolve_counted_dir(
            &mut doc, &view, &loader, 3, go_param(src, "width").0, Direction::Backward,
        );
        let w = go_param(src, "width");
        assert_eq!(spans(&bob), vec![(w.0, w.0)], "BOB: inert zero-width point");
    }

    #[test]
    fn test_go_parameter_delimiter_aware_delete_and_reparse() {
        // t n d on the MIDDLE parameter repairs the separator (Phase 40-44
        // machinery is capture-driven, so it transfers to Go untouched).
        let src = GO_PARAMS;
        let pos = go_param(src, "height").0;
        let (mut doc, view, loader) = go_doc_at(src, pos);
        let sel = Target::Parameter.resolve_counted_dir(
            &mut doc, &view, &loader, 1, pos, Direction::Forward,
        );
        let del = parameter_delete_selection(&doc, &loader, &sel);
        let transaction =
            Transaction::change(doc.text(), del.ranges().iter().map(|r| (r.from(), r.to(), None)));
        doc.apply(&transaction, view.id);
        let result = doc.text().slice(..).to_string();
        assert_eq!(
            result,
            "package main\n\nfunc calc(width int, depth int) int {\n\treturn width * depth\n}\n"
        );

        // Reparse with the real Go grammar; two params remain.
        let loader = test_loader();
        let mut doc = Document::from(
            Rope::from(result.clone()),
            None,
            std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(EditorConfig::default())),
            loader.clone(),
        );
        doc.set_language_by_language_id("go", &loader.load()).unwrap();
        assert!(doc.syntax().is_some());
        let view = View::new(doc.id(), GutterConfig::default());
        let probe = result.find("width").unwrap();
        doc.set_selection(view.id, Selection::point(probe));
        let remaining = Target::Parameter.resolve_counted_dir(
            &mut doc, &view, &loader, 50, probe, Direction::Forward,
        );
        assert_eq!(remaining.ranges().len(), 2);
    }

    // ============================================================
    // Phase 47: cross-grammar matrix — REAL C grammar.
    //
    // MEASURED QUERY REALITY (runtime/queries/c/textobjects.scm): C DOES
    // define @class.inside — struct/enum/union bodies — so this is a
    // POSITIVE capture pin, not the anticipated fallback-only contract.
    // The global no-fabrication property for genuinely missing captures
    // remains pinned by the Python Statement tests.
    // ============================================================

    const C_FN_AND_STRUCT: &str =
        "int alpha(int x, int y) {\n    return x;\n}\n\nstruct Point {\n    int px;\n    int py;\n};\n";

    #[test]
    fn test_c_function_and_parameter_positive_captures() {
        let src = C_FN_AND_STRUCT;
        // Function: body block incl. braces (shared cross-grammar contract).
        let fpos = src.find("return").unwrap();
        let (mut doc, view, loader) = c_doc_at(src, fpos);
        let one = Target::Function.resolve_counted_dir(
            &mut doc, &view, &loader, 1, fpos, Direction::Forward,
        );
        let s = spans(&one);
        assert_eq!(s.len(), 1);
        assert_eq!(frag_at(src, s[0]), "{\n    return x;\n}");

        // Parameter: whole declaration node ("int y").
        let ppos = src.find("int y").unwrap();
        let (doc, view, loader) = c_doc_at(src, ppos);
        let r = Target::Parameter.resolve(&doc, &view, &loader).primary();
        assert_eq!(frag_at(src, (r.from(), r.to())), "int y");
    }

    #[test]
    fn test_c_struct_class_positive_capture_and_bounded_count() {
        let src = C_FN_AND_STRUCT;
        let spos = src.find("int py").unwrap();
        let (mut doc, view, loader) = c_doc_at(src, spos);
        let r = Target::Class.resolve(&doc, &view, &loader).primary();
        let frag = frag_at(src, (r.from(), r.to()));
        assert_eq!(frag, "{\n    int px;\n    int py;\n}", "struct body incl. braces");

        // Over-count terminates with exactly what exists — no junk collection.
        let many =
            Target::Class.resolve_counted_dir(&mut doc, &view, &loader, 50, spos, Direction::Forward);
        assert_eq!(spans(&many), vec![(r.from(), r.to())]);
    }

    // ============================================================
    // Phase 51 RELEASE-CONTRACT PINS
    //
    // Closes the last verification gaps in the public semantic surface:
    //   - Expression / Block have NO bundled captures anywhere (audited),
    //     so they must behave exactly like the pinned Statement contract:
    //     documented width-1 fallback, zero junk collection under counted
    //     traversal in either direction.
    //   - Target::All is the whole-document primitive.
    // ============================================================

    #[test]
    fn test_expression_and_block_fallback_contract_on_real_tree() {
        let src = THREE_FNS;
        let cursor = src.find("let b").unwrap() + 2;
        let (mut doc, view, loader) = rust_doc_at(src, cursor);

        for target in [Target::Expression, Target::Block] {
            // Fresh document per target: counted traversal below mutates the
            // doc's scratch selection, and plain resolution falls back to it.
            let (mut doc, view, loader) = rust_doc_at(src, cursor);
            assert!(target.is_structural(), "{target:?} is structural by design");
            // Plain resolution: documented width-1 selection fallback.
            let range = target.resolve(&doc, &view, &loader).primary();
            assert_eq!(
                (range.from(), range.to()),
                (cursor, cursor + 1),
                "{target:?}: no invented semantic range"
            );
            // Counted traversal in BOTH directions collects no fallback junk.
            for dir in [Direction::Forward, Direction::Backward] {
                let sel = target.resolve_counted_dir(&mut doc, &view, &loader, 5, cursor, dir);
                assert_eq!(
                    spans(&sel),
                    vec![(cursor, cursor)],
                    "{target:?} {dir:?}: bounded inert point, no junk"
                );
            }
        }
    }

    #[test]
    fn test_all_target_is_whole_document_primitive() {
        let src = THREE_FNS;
        let cursor = src.find("let b").unwrap() + 2;
        let (doc, view, loader) = rust_doc_at(src, cursor);
        let range = Target::All.resolve(&doc, &view, &loader).primary();
        assert_eq!((range.from(), range.to()), (0, src.len()));
        assert_eq!(Target::All.is_structural(), false);
    }

    // ============================================================
    // Phase 52 release hardening: Yank's DATA contract.
    //
    // The register write itself needs a live Editor (TUI); what CAN be
    // pinned without it is the exact payload the Yank arm collects:
    // Selection::fragments over the resolved counted selection -> one
    // string per target, document order.
    // ============================================================

    #[test]
    fn test_yank_payload_fragments_are_one_per_target_in_document_order() {
        let src = THREE_FNS;
        let pos = src.find("let c").unwrap(); // inside gamma
        let (mut doc, view, loader) = rust_doc_at(src, pos);
        let sel = Target::Function.resolve_counted_dir(
            &mut doc, &view, &loader, 2, pos, Direction::Backward,
        );
        doc.set_selection(view.id, sel);
        let text = doc.text().slice(..);
        let values: Vec<String> = doc
            .selection(view.id)
            .fragments(text)
            .map(std::borrow::Cow::into_owned)
            .collect();
        assert_eq!(values.len(), 2);
        // @function.inside = body block; signature excluded by grammar contract.
        assert!(values[0].contains("let a"), "alpha body: {:?}", values[0]);
        assert!(values[1].contains("let b"), "beta body: {:?}", values[1]);
        assert!(!values.iter().any(|v| v.contains("gamma")), "containing object excluded");
    }

    // ============================================================
    // Phase 48: NON-PARAMETER semantic repeat across mutations.
    //
    // Contract under test (`repeat_target_action`, scope-preserving branch):
    //   state = (Target, Action, count, Direction, MultiSelectIntent) only;
    //   every repeat recomputes the current scope at the CURRENT cursor,
    //   steps past its trailing edge (Backward: min(from)-1), then resolves
    //   fresh against the CURRENT document. No Selection/Range survives.
    //
    // These tests mirror that exact branch against real Rust trees, like
    // the Parameter repeat precedents (Phases 40/41).
    // ============================================================

    const FOUR_FNS: &str = "fn alpha() {\n    let a = 1;\n}\n\nfn beta() {\n    let b = 2;\n}\n\nfn gamma() {\n    let c = 3;\n}\n\nfn delta() {\n    let d = 4;\n}\n";

    /// Production-shaped scope-preserving backward repeat step: identical
    /// logic to `repeat_target_action`'s `advance_past_current` branch.
    fn repeat_backward_yank(
        doc: &mut Document,
        view: &View,
        loader: &TestLoader,
        target: Target,
        count: usize,
        cursor: usize,
    ) -> Selection {
        let current_scope =
            target.resolve_counted_dir(doc, view, loader, count, cursor, Direction::Backward);
        let edge = current_scope
            .ranges()
            .iter()
            .map(|r| r.from())
            .min()
            .unwrap_or(cursor)
            .saturating_sub(1);
        target.resolve_counted_dir(doc, view, loader, count, edge, Direction::Backward)
    }

    #[test]
    fn test_function_backward_repeat_baseline_without_mutation() {
        let src = FOUR_FNS;
        let in_gamma = src.find("let c").unwrap();
        let (mut doc, view, loader) = rust_doc_at(src, in_gamma);

        // Initial `t -f y` from inside gamma: containing object skipped -> beta.
        let initial =
            Target::Function.resolve_counted_dir(&mut doc, &view, &loader, 1, in_gamma, Direction::Backward);
        let b = spans(&initial)[0];
        assert!(frag_at(src, b).contains("let b"), "initial reaches beta");
        assert!(!frag_at(src, b).contains("let c"), "containing gamma excluded");

        // Repeat `.`: production yank sets the document selection to the
        // yanked range; the repeat cursor is its head (inside beta).
        let cursor = initial.primary().head;
        let repeated = repeat_backward_yank(&mut doc, &view, &loader, Target::Function, 1, cursor);
        let s = spans(&repeated);
        assert_eq!(s.len(), 1);
        let frag = frag_at(src, s[0]);
        assert!(frag.contains("let a"), "repeat reaches alpha: {frag}");
        assert!(!frag.contains("let b") && !frag.contains("let c"));
    }

    #[test]
    fn test_function_backward_repeat_after_mutation_uses_current_tree() {
        let src = FOUR_FNS;
        let in_delta = src.find("let d").unwrap();
        let (mut doc, view, loader) = rust_doc_at(src, in_delta);

        // Round 0: `t -f y` from delta selects gamma.
        let initial =
            Target::Function.resolve_counted_dir(&mut doc, &view, &loader, 1, in_delta, Direction::Backward);
        let g = spans(&initial)[0];
        assert!(frag_at(src, g).contains("let c"));

        // MUTATION: delete beta entirely (normal Helix transaction).
        let b_start = src.find("fn beta").unwrap();
        let b_end = src.find("fn gamma").unwrap();
        let transaction =
            Transaction::change(doc.text(), vec![(b_start, b_end, None)].into_iter());
        doc.apply(&transaction, view.id);

        let new_text = doc.text().slice(..).to_string();
        // Production maps the live selection through the change; emulate by
        // re-deriving gamma's head in the mutated document.
        let new_cursor = new_text.find("let c").unwrap();

        // MEASURED contract: the scope-preserving branch recomputes the
        // current scope (now alpha, since beta vanished), steps past ITS
        // edge, and finds nothing further — an inert zero-width point.
        // Crucially this proves CURRENT-tree behavior: a stale replay of
        // the pre-mutation answer would have returned dead-beta geometry.
        let repeated =
            repeat_backward_yank(&mut doc, &view, &loader, Target::Function, 1, new_cursor);
        let s = spans(&repeated);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].0, s[0].1, "inert exhaustion against the CURRENT tree");
        assert!(s[0].0 <= new_cursor, "never past the scan origin");
        // Nothing stale was manufactured or applied.
        let frag = frag_at(&new_text, s[0]);
        assert!(frag.is_empty());
        assert!(!frag.contains("let b"), "no stale beta replay");
    }

    #[test]
    fn test_function_counted_backward_repeat_after_mutation_bounded_inert() {
        let src = FOUR_FNS;
        let in_delta = src.find("let d").unwrap();
        let (mut doc, view, loader) = rust_doc_at(src, in_delta);

        // Round 0: `t -2 f y` from delta -> [beta, gamma].
        let initial =
            Target::Function.resolve_counted_dir(&mut doc, &view, &loader, 2, in_delta, Direction::Backward);
        let s0 = spans(&initial);
        assert_eq!(s0.len(), 2);
        assert!(frag_at(src, s0[0]).contains("let b"));

        // MUTATION: delete alpha — everything before the pair is gone.
        let a_start = src.find("fn alpha").unwrap();
        let a_end = src.find("fn beta").unwrap();
        let transaction =
            Transaction::change(doc.text(), vec![(a_start, a_end, None)].into_iter());
        doc.apply(&transaction, view.id);

        let new_text = doc.text().slice(..).to_string();
        let new_cursor = new_text.find("let b").unwrap();

        // Counted repeat: scope-preserving branch recomputes [.. ] then steps
        // past the edge. With nothing preceding beta, it must terminate as an
        // inert zero-width point — bounded, panic-free, no stale ranges.
        let repeated =
            repeat_backward_yank(&mut doc, &view, &loader, Target::Function, 2, new_cursor);
        let s = spans(&repeated);
        assert_eq!(s.len(), 1, "single fallback range");
        assert_eq!(s[0].0, s[0].1, "zero-width inert termination");
        assert!(s[0].0 <= new_cursor, "never points past the scan origin");
    }

    /// Mirrors apply_target_selection's Indent/Outdent branch exactly.
    fn mirror_indent(doc: &mut Document, view: &View, selection: Selection, indent: bool) -> String {
        let indent_str = doc.indent_style.as_str().to_owned();
        let tab_width = doc.tab_width();
        let indent_width = doc.indent_width();
        doc.set_selection(view.id, selection);
        let current_selection = doc.selection(view.id).clone();
        if indent {
            // Mirrors the deduped production Indent branch (Phase 49).
            let mut seen_lines = std::collections::HashSet::new();
            let transaction = Transaction::change(
                doc.text(),
                current_selection.ranges().iter().filter_map(|range| {
                    let text = doc.text().slice(..);
                    let line = range.cursor_line(text);
                    if !seen_lines.insert(line) {
                        return None;
                    }
                    let pos = text.line_to_char(line);
                    let is_blank = text.line(line).chunks().all(|s| s.trim().is_empty());
                    if is_blank {
                        return None;
                    }
                    Some((pos, pos, Some(helix_core::Tendril::from(indent_str.clone()))))
                }),
            );
            doc.apply(&transaction, view.id);
        } else {
            let mut changes = Vec::new();
            for range in current_selection.ranges() {
                let text = doc.text().slice(..);
                let line_idx = range.cursor_line(text);
                let line = text.line(line_idx);
                let mut width = 0;
                let mut pos = 0;
                for ch in line.chars() {
                    match ch {
                        ' ' => width += 1,
                        '\t' => width = (width / tab_width + 1) * tab_width,
                        _ => break,
                    }
                    pos += 1;
                    if width >= indent_width {
                        break;
                    }
                }
                if pos > 0 {
                    let line_start = text.line_to_char(line_idx);
                    changes.push((line_start, line_start + pos, None));
                }
            }
            if !changes.is_empty() {
                let transaction = Transaction::change(doc.text(), changes.into_iter());
                doc.apply(&transaction, view.id);
            }
        }
        doc.text().slice(..).to_string()
    }

    // ============================================================
    // Phase 49: semantic INDENT/OUTDENT — composing existing Helix actions
    // with semantic selections.
    //
    // Audited reality: Indent/Outdent already live inside the single funnel
    // (apply_target_selection) and operate per-range, line-based. Counted
    // and directional scope are inherited automatically from
    // resolve_counted_dir. Measured contract (default EditorConfig):
    //   - indent unit is TAB; one unit inserted at each unique range line,
    //   - a multi-line target indents its CURSOR-side line only (existing
    //     Action semantics; recorded, not redesigned),
    //   - same-line ranges indent ONCE (Phase 49 dedupe regression below).
    // ============================================================

    #[test]
    fn test_indent_function_indents_cursor_line_once() {
        let src = "fn alpha() {\n    let a = 1;\n}\n";
        let pos = src.find("let a").unwrap();
        let (mut doc, view, loader) = rust_doc_at(src, pos);
        let sel = Target::Function.resolve_counted_dir(
            &mut doc, &view, &loader, 1, pos, Direction::Forward,
        );
        let out = mirror_indent(&mut doc, &view, sel, true);
        assert_eq!(out, "fn alpha() {\n    let a = 1;\n\t}\n");
    }

    #[test]
    fn test_indent_same_line_multi_parameter_is_deduped() {
        // REGRESSION: before the Phase 49 dedupe, two ranges on one line
        // stacked TWO tabs for a single `t 2 n >`.
        let src = "fn f(alpha: i32, beta: i32) {\n    let x = 1;\n}\n";
        let pos = src.find("alpha").unwrap();
        let (mut doc, view, loader) = rust_doc_at(src, pos);
        let sel = Target::Parameter.resolve_counted(&mut doc, &view, &loader, 2, pos);
        assert_eq!(spans(&sel).len(), 2);
        let out = mirror_indent(&mut doc, &view, sel, true);
        assert_eq!(
            out, "\tfn f(alpha: i32, beta: i32) {\n    let x = 1;\n}\n",
            "exactly ONE indent unit for the shared line"
        );
    }

    #[test]
    fn test_indent_distinct_line_parameters_indent_each() {
        let src = "fn f(\n    alpha: i32,\n    beta: i32,\n) {\n}\n";
        let pos = src.find("alpha").unwrap();
        let (mut doc, view, loader) = rust_doc_at(src, pos);
        let sel = Target::Parameter.resolve_counted_dir(
            &mut doc, &view, &loader, 2, pos, Direction::Forward,
        );
        let s = spans(&sel);
        assert_eq!(s.len(), 2);
        assert_ne!(
            doc.text().line_to_char(doc.text().char_to_line(s[0].0)),
            doc.text().line_to_char(doc.text().char_to_line(s[1].0)),
            "fixture sanity: params on distinct lines"
        );
        let out = mirror_indent(&mut doc, &view, sel, true);
        assert_eq!(
            out, "fn f(\n\t    alpha: i32,\n\t    beta: i32,\n) {\n}\n",
            "each distinct parameter line gets one tab"
        );
    }

    #[test]
    fn test_outdent_removes_one_indent_width_from_parameter_lines() {
        let src = "fn f(\n\t    alpha: i32,\n) {\n}\n";
        let pos = src.find("alpha").unwrap();
        let (mut doc, view, loader) = rust_doc_at(src, pos);
        let sel = Target::Parameter.resolve_counted_dir(
            &mut doc, &view, &loader, 1, pos, Direction::Forward,
        );
        let out = mirror_indent(&mut doc, &view, sel, false);
        assert_eq!(
            out, "fn f(\n    alpha: i32,\n) {\n}\n",
            "one tab-width removed"
        );
    }

    /// Mirrors apply_target_selection's Change arm exactly and returns the
    /// resulting text plus post-change cursor count (part of the contract).
    fn mirror_change(
        doc: &mut Document,
        view: &View,
        selection: Selection,
    ) -> (String, usize) {
        doc.set_selection(view.id, selection);
        let transaction =
            Transaction::delete_by_selection(doc.text(), doc.selection(view.id), |range| {
                (range.from(), range.to())
            });
        doc.apply(&transaction, view.id);
        (
            doc.text().slice(..).to_string(),
            doc.selection(view.id).ranges().len(),
        )
    }

    // ============================================================
    // Phase 50: counted Change through the standard pipeline.
    //
    // Contract: resolve N disjoint targets via resolve_counted_dir, apply
    // Helix's native multi-selection change; result is one insertion
    // cursor per target. Measured on real trees — no merging, no second
    // execution path.
    // ============================================================

    const THREE_FNS_C: &str = "fn alpha() {\n    let a = 1;\n}\n\nfn beta() {\n    let b = 2;\n}\n\nfn gamma() {\n    let c = 3;\n}\n";

    #[test]
    fn test_counted_change_two_functions_disjoint_and_ordered() {
        let src = THREE_FNS_C;
        let pos = src.find("let a").unwrap();
        let (mut doc, view, loader) = rust_doc_at(src, pos);
        let sel = Target::Function.resolve_counted_dir(
            &mut doc, &view, &loader, 2, pos, Direction::Forward,
        );
        let s = spans(&sel);
        assert_eq!(s.len(), 2);
        assert!(s[0].1 <= s[1].0, "document-ordered disjoint ranges");

        let (out, cursors) = mirror_change(&mut doc, &view, sel);
        assert_eq!(
            out, "fn alpha() \n\nfn beta() \n\nfn gamma() {\n    let c = 3;\n}\n",
            "both bodies cleared in ONE transaction"
        );
        assert_eq!(cursors, 2, "one insertion cursor per changed target");
    }

    #[test]
    fn test_counted_change_same_line_parameters() {
        let src = "fn f(alpha: i32, beta: i32) {\n    let x = 1;\n}\n";
        let pos = src.find("alpha").unwrap();
        let (mut doc, view, loader) = rust_doc_at(src, pos);
        let sel = Target::Parameter.resolve_counted(&mut doc, &view, &loader, 2, pos);
        let (out, cursors) = mirror_change(&mut doc, &view, sel);
        assert_eq!(out, "fn f(, ) {\n    let x = 1;\n}\n");
        assert_eq!(cursors, 2);
    }

    #[test]
    fn test_counted_change_backward_two_functions() {
        let src = THREE_FNS_C;
        let pos = src.find("let c").unwrap(); // inside gamma
        let (mut doc, view, loader) = rust_doc_at(src, pos);
        let sel = Target::Function.resolve_counted_dir(
            &mut doc, &view, &loader, 2, pos, Direction::Backward,
        );
        let s = spans(&sel);
        assert_eq!(s.len(), 2);
        assert!(frag_at(src, s[0]).contains("let a"), "alpha first");
        assert!(frag_at(src, s[1]).contains("let b"), "beta second");
        let (out, cursors) = mirror_change(&mut doc, &view, sel);
        assert_eq!(out, "fn alpha() \n\nfn beta() \n\nfn gamma() {\n    let c = 3;\n}\n");
        assert_eq!(cursors, 2);
    }

    #[test]
    fn test_counted_change_overcount_bounded() {
        let src = THREE_FNS_C;
        let pos = src.find("let a").unwrap();
        let (mut doc, view, loader) = rust_doc_at(src, pos);
        let sel = Target::Function.resolve_counted_dir(
            &mut doc, &view, &loader, 50, pos, Direction::Forward,
        );
        let s = spans(&sel);
        assert_eq!(s.len(), 3, "bounded by available targets");
        let (_, cursors) = mirror_change(&mut doc, &view, sel);
        assert_eq!(cursors, 3);
    }

    #[test]
    fn test_go_struct_class_resolve_real_treesitter() {
        let src = "package main\n\ntype Point struct {\n\tx int\n\ty int\n}\n";
        let pos = src.find("y int").unwrap();
        let (doc, view, loader) = go_doc_at(src, pos);
        let r = Target::Class.resolve(&doc, &view, &loader).primary();
        // Measured contract: struct body block incl. braces.
        let frag = frag_at(src, (r.from(), r.to()));
        assert!(frag.contains("x int") && frag.contains("y int"), "{frag}");
        assert!(frag.starts_with('{') && frag.ends_with('}'));
    }

    #[test]
    fn test_argument_object_has_no_bundled_capture_falls_back() {
        // "argument" resolves through textobject_treesitter like any object,
        // but NO grammar defines it -> documented selection fallback.
        let src = RS_CALLSITE;
        let pos = src.find("beta").unwrap();
        let (doc, view, loader) = rust_doc_at(src, pos);
        let range = Target::Argument.resolve(&doc, &view, &loader).primary();
        // Current Argument is TEXT-BASED (pair-surround on '(', Inside): it
        // must NOT fall back to width-1; it selects inside the paren region,
        // delimiters excluded.
        let frag = frag_at(src, (range.from(), range.to()));
        assert!(
            frag.contains("alpha") && frag.contains("gamma"),
            "whole call-argument content: {frag}"
        );
        assert!(!frag.contains('(') && !frag.contains(')'), "Inside: delimiters excluded");
    }

    #[test]
    fn test_rs_multiline_middle_parameter_delete_keeps_layout() {
        // Left-anchored removal of ",\n    beta: i32" — the newline that goes
        // belongs to beta's own line; surrounding layout survives untouched.
        let pos = RS_MULTILINE.find("beta").unwrap();
        assert_eq!(
            rs_multiline_delete_at(pos),
            "fn example(\n    alpha: i32,\n    gamma: i32,\n) {\n}\n"
        );
    }

    #[test]
    fn test_rs_multiline_first_parameter_delete_spares_newline() {
        // Head parameter: trailing "," consumed; the newline after it is NOT
        // horizontal whitespace and must survive.
        let pos = RS_MULTILINE.find("alpha").unwrap();
        let result = rs_multiline_delete_at(pos);
        assert_eq!(
            result,
            "fn example(\n    beta: i32,\n    gamma: i32,\n) {\n}\n"
        );
        assert!(result.contains("\n    beta"), "newline after separator preserved");
    }

    #[test]
    fn test_rs_multiline_last_parameter_delete_keeps_trailing_comma() {
        // Last member with trailing comma: left-anchored span stops at the
        // capture end, so gamma's own trailing "," stays — still valid Rust.
        let pos = RS_MULTILINE.find("gamma").unwrap();
        assert_eq!(
            rs_multiline_delete_at(pos),
            "fn example(\n    alpha: i32,\n    beta: i32,\n) {\n}\n"
        );
    }

    #[test]
    fn test_rs_multiline_counted_two_delete_and_reparse() {
        // t 2 n d across a multiline head pair: one merged region closed on
        // beta's outer boundary separator. Layout beyond it is untouched
        // (the blank first line is honest minimal behavior, not formatting).
        let pos = RS_MULTILINE.find("alpha").unwrap();
        let (mut doc, view, loader) = rust_doc_at(RS_MULTILINE, pos);
        let sel = Target::Parameter.resolve_counted_dir(
            &mut doc, &view, &loader, 2, pos, Direction::Forward,
        );
        let del = parameter_delete_selection(&doc, &loader, &sel);
        assert_eq!(del.ranges().len(), 1, "contiguous group merges");
        let transaction =
            Transaction::change(doc.text(), del.ranges().iter().map(|r| (r.from(), r.to(), None)));
        doc.apply(&transaction, view.id);
        let result = doc.text().slice(..).to_string();
        assert_eq!(
            result,
            "fn example(\n    gamma: i32,\n) {\n}\n",
            "group removes its members' lines incl. own-line newlines; layout beyond is verbatim"
        );

        // Reparse the RESULT and prove the semantic structure: only gamma.
        let loader = test_loader();
        let mut doc = Document::from(
            Rope::from(result.clone()),
            None,
            std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(EditorConfig::default())),
            loader.clone(),
        );
        doc.set_language_by_language_id("rust", &loader.load()).unwrap();
        assert!(doc.syntax().is_some(), "multiline-deleted source reparses");
        let view = View::new(doc.id(), GutterConfig::default());
        let probe = result.find("gamma").unwrap();
        doc.set_selection(view.id, Selection::point(probe));
        let remaining = Target::Parameter.resolve_counted_dir(
            &mut doc, &view, &loader, 50, probe, Direction::Forward,
        );
        let s = spans(&remaining);
        assert_eq!(s.len(), 1, "exactly one parameter survives");
        assert_eq!(&result[s[0].0..s[0].1], "gamma: i32");
    }

    #[test]
    fn test_py_param_repeat_deletes_next_from_current_tree() {
        // Engine-level repeat resolves against the CURRENT document; prove the
        // chain manually with two production-shaped steps.
        let src = "def h(kaa, bee, cee, dee):\n    z = 0\n";
        let mut cursor = src.find("kaa").unwrap();

        let (mut doc, view, loader) = python_doc_at(src, cursor);
        for expected in [
            "def h(bee, cee, dee):\n    z = 0\n",
            "def h(cee, dee):\n    z = 0\n",
        ] {
            let resolved = Target::Parameter.resolve(&doc, &view, &loader).primary();
            let del = parameter_delete_selection(
                &doc,
                &loader,
                &Selection::single(resolved.from(), resolved.to()),
            );
            let transaction = Transaction::change(
                doc.text(),
                del.ranges().iter().map(|r| (r.from(), r.to(), None)),
            );
            doc.apply(&transaction, view.id);
            assert_eq!(doc.text().slice(..).to_string(), expected);
            // Scope-removing action: the next parameter now occupies the cursor.
            cursor = resolved.from().min(doc.text().len_chars());
        }
        let _ = cursor;
    }

    #[test]
    fn test_py_param_deleted_document_still_parses_with_fewer_params() {
        // Reparse the RESULT with the same grammar and verify the semantic
        // structure: valid tree, and exactly two parameters remain resolvable.
        let src = PY3_PARAMS;
        let cur = src.find("beta").unwrap(); // middle
        let result = delete_param_at((&test_loader(), src, cur));
        assert_eq!(result, "def f(alpha, gamma):\n    x = 1\n");

        let loader = test_loader();
        let mut doc = Document::from(
            Rope::from(result.clone()),
            None,
            std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(EditorConfig::default())),
            loader.clone(),
        );
        doc.set_language_by_language_id("python", &loader.load()).unwrap();
        assert!(doc.syntax().is_some(), "result reparses");
        let view = View::new(doc.id(), GutterConfig::default());
        let probe = result.find("alpha").unwrap();
        doc.set_selection(view.id, Selection::point(probe));
        let remaining =
            Target::Parameter.resolve_counted(&mut doc, &view, &loader, 50, probe);
        assert_eq!(remaining.ranges().len(), 2, "exactly two params survive");
    }

    #[test]
    fn test_param_deletion_without_syntax_is_inert_and_safe() {
        // No language: resolution falls back to the width-1 selection and the
        // adjuster must pass it through untouched (no panic, no expansion).
        let src = RS3_PARAMS;
        let cursor = src.find("beta").unwrap();
        let loader = test_loader();
        let mut doc = Document::from(
            Rope::from(src),
            None,
            std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(EditorConfig::default())),
            loader.clone(),
        );
        let view = View::new(doc.id(), GutterConfig::default());
        doc.set_selection(view.id, Selection::point(cursor));
        let resolved = Target::Parameter.resolve(&doc, &view, &loader).primary();
        let del = parameter_delete_selection(
            &doc,
            &loader,
            &Selection::single(resolved.from(), resolved.to()),
        );
        assert_eq!(del.ranges().len(), 1);
        assert_eq!((del.primary().from(), del.primary().to()), (cursor, cursor + 1));
    }
}
