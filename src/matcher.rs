use std::collections::HashMap;
use tree_sitter::{
    Language, Node, Parser, Point, Query, QueryCursor, StreamingIterator, TreeCursor,
};

#[derive(Debug, Clone)]
pub struct Match {
    pub start_byte: usize,
    pub end_byte: usize,
    pub start_point: Point,
    #[allow(dead_code)]
    pub end_point: Point,
    /// Placeholder captures from snippet matching.
    /// Maps `$name` → the source text it matched.
    pub captures: HashMap<String, String>,
}

/// Distinguishes repetition syntax styles.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum RepetitionKind {
    /// `$($name)sep*` — only applies at the last child position
    LastChild,
    /// `$$$name` or `$$$` — can span multiple consecutive children at any position
    MultiChild,
}

/// Metadata about a `$($name)sep*` or `$$$name` repetition placeholder.
#[derive(Debug, Clone)]
pub(crate) struct RepetitionInfo {
    /// Repetition operator: "*", "+", or "?"
    op: String,
    pub(crate) kind: RepetitionKind,
    /// Sub-pattern sentinel order for multi-item repetition groups.
    /// E.g. for `$($a + $b),*`, `sub_sentinels = ["_pw_a", "_pw_b"]`.
    /// Empty for single-item `$($name)sep*` (backwards compatible).
    sub_sentinels: Vec<String>,
}

/// Pre-defined special placeholder tokens that match by node role
/// rather than by structure alone.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum SpecialKind {
    /// `$BODY` — zero or more statements inside a block.
    /// Statement-aware: peeks through tree-sitter's `expression_statement`
    /// wrappers so it works where `$$$name` doesn't.
    Body,
    /// `$STMT` — a single statement of any kind.
    Stmt,
    /// `$EXPR` — a single expression.
    Expr,
}

fn is_root_wrapper(kind: &str) -> bool {
    matches!(kind, "program" | "module")
}

/// Extract the meaningful pattern node from a parsed snippet tree.
fn extract_pattern(root: Node) -> Option<Node> {
    if is_root_wrapper(root.kind()) && root.named_child_count() == 1 {
        root.named_child(0)
    } else {
        Some(root)
    }
}

/// Check if a node's subtree contains an ERROR node.
fn has_error(node: Node) -> bool {
    if node.kind() == "ERROR" {
        return true;
    }
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            if has_error(cursor.node()) {
                return true;
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    false
}

/// Check if a named leaf node matches by text content.
/// Named leaf nodes require exact text match by default.
fn leaf_matches(source_text: &str, pattern_text: &str, s_node: Node, p_node: Node) -> bool {
    let p = &pattern_text[p_node.start_byte()..p_node.end_byte()];
    let s = &source_text[s_node.start_byte()..s_node.end_byte()];
    p == s
}

/// Build reverse map from sentinel identifier to placeholder name.
/// `{"msg": "_pw_1"}` → `{"_pw_1": "msg"}`
fn reverse_placeholder_map(placeholders: &HashMap<String, String>) -> HashMap<String, String> {
    placeholders
        .iter()
        .map(|(k, v)| (v.clone(), k.clone()))
        .collect()
}

/// Check if a pattern node is a `$` placeholder (a bare `_pw_N` identifier).
fn is_placeholder(pattern_node: Node, pattern_text: &str) -> bool {
    if !pattern_node.is_named() || pattern_node.named_child_count() != 0 {
        return false;
    }
    let text = &pattern_text[pattern_node.start_byte()..pattern_node.end_byte()];
    text.starts_with("_pw_")
}

/// Check if the last named child of `parent` is a repetition placeholder.
/// Returns `Some((sentinel, info))` if so.
fn last_child_repetition(
    parent: Node,
    pattern_text: &str,
    repetitions: &HashMap<String, RepetitionInfo>,
) -> Option<(String, RepetitionInfo)> {
    let count = parent.named_child_count();
    if count == 0 {
        return None;
    }
    let last = parent.named_child(count as u32 - 1)?;
    if !is_placeholder(last, pattern_text) {
        return None;
    }
    let sentinel = pattern_text[last.start_byte()..last.end_byte()].to_string();
    match repetitions.get(&sentinel) {
        Some(info) if info.kind == RepetitionKind::LastChild => {
            Some((sentinel, info.clone()))
        }
        _ => None,
    }
}

/// Shared read-only context passed through match functions.
struct MatchCtx<'a> {
    source_text: &'a str,
    pattern_text: &'a str,
    reverse_placeholders: &'a HashMap<String, String>,
    repetitions: &'a HashMap<String, RepetitionInfo>,
    specials: &'a HashMap<String, SpecialKind>,
    type_constraints: &'a HashMap<String, String>,
}

/// Check if two nodes are structurally equivalent.
///
/// Named leaf nodes (identifiers, literals) match by text content by
/// default. Use `$name` in snippet patterns to match any single node
/// at that position. Use `$($name)sep*` / `$($name)sep+` to match
/// zero or more / one or more repetitions as the last child.
fn structurally_matches(
    source_node: Node,
    pattern_node: Node,
    captures: &mut HashMap<String, String>,
    ctx: &MatchCtx,
) -> bool {
    if is_placeholder(pattern_node, ctx.pattern_text) {
        let sentinel = &ctx.pattern_text[pattern_node.start_byte()..pattern_node.end_byte()];
        if let Some(name) = ctx.reverse_placeholders.get(sentinel) {
            let matched = &ctx.source_text[source_node.start_byte()..source_node.end_byte()];
            captures.insert(name.clone(), matched.to_string());
        }
        // Check type constraint: $name:Kind restricts to a specific AST kind
        if let Some(constrained_kind) = ctx.type_constraints.get(sentinel) {
            if source_node.kind() != constrained_kind.as_str() {
                return false;
            }
        }
        return true;
    }

    if source_node.kind() != pattern_node.kind() {
        if let Some(special) = wraps_special(pattern_node, ctx.pattern_text, ctx.specials) {
            let matched_kind = match special {
                SpecialKind::Stmt => is_statement_kind(source_node.kind()),
                SpecialKind::Expr => !is_statement_kind(source_node.kind())
                    && source_node.kind() != "program"
                    && source_node.kind() != "module",
                SpecialKind::Body => is_statement_kind(source_node.kind()),
            };
            if matched_kind {
                if let Some(sentinel) = inner_sentinel(pattern_node, ctx.pattern_text) {
                    if let Some(name) = ctx.reverse_placeholders.get(&sentinel) {
                        let matched =
                            &ctx.source_text[source_node.start_byte()..source_node.end_byte()];
                        captures.insert(name.clone(), matched.to_string());
                    }
                }
                return true;
            }
        }
        return false;
    }

    let pattern_named = pattern_node.named_child_count();
    let source_named = source_node.named_child_count();

    if let Some((sentinel, rep)) =
        last_child_repetition(pattern_node, ctx.pattern_text, ctx.repetitions)
    {
        let fixed = pattern_named - 1;
        if source_named < fixed {
            return false;
        }
        let matched_count = source_named - fixed;

        if rep.op == "+" && matched_count == 0 {
            return false;
        }
        if rep.op == "?" && matched_count > 1 {
            return false;
        }

        for i in 0..fixed {
            let Some(p_child) = pattern_node.named_child(i as u32) else {
                return false;
            };
            let Some(s_child) = source_node.named_child(i as u32) else {
                return false;
            };
            if !structurally_matches(s_child, p_child, captures, ctx) {
                return false;
            }
        }

        if rep.sub_sentinels.is_empty() {
            // Single-item: existing behavior
            if matched_count > 0 {
                if let Some(name) = ctx.reverse_placeholders.get(&sentinel) {
                    let first = source_node.named_child(fixed as u32).unwrap();
                    let last = source_node.named_child((source_named - 1) as u32).unwrap();
                    let text = &ctx.source_text[first.start_byte()..last.end_byte()];
                    captures.insert(name.clone(), text.to_string());
                }
            } else if let Some(name) = ctx.reverse_placeholders.get(&sentinel) {
                captures.insert(name.clone(), String::new());
            }
        } else {
            // Multi-item: group source children by sub-pattern size
            let group_size = rep.sub_sentinels.len();
            if !matched_count.is_multiple_of(group_size) {
                return false;
            }
            let num_groups = matched_count / group_size;
            if num_groups == 0 {
                // Zero matches: capture empty strings for all placeholders
                for sub_sentinel in &rep.sub_sentinels {
                    if let Some(name) = ctx.reverse_placeholders.get(sub_sentinel) {
                        captures.insert(name.clone(), String::new());
                    }
                }
            } else {
                let mut accum: HashMap<String, Vec<(usize, usize)>> = HashMap::new();
                for g in 0..num_groups {
                    for (j, sub_sentinel) in rep.sub_sentinels.iter().enumerate() {
                        let s_idx = fixed + g * group_size + j;
                        let s_child = source_node.named_child(s_idx as u32).unwrap();
                        accum.entry(sub_sentinel.clone())
                            .or_default()
                            .push((s_child.start_byte(), s_child.end_byte()));
                    }
                }
                for (sub_sentinel, ranges) in &accum {
                    if let Some(name) = ctx.reverse_placeholders.get(sub_sentinel) {
                        let text: String = ranges.iter()
                            .map(|&(s, e)| &ctx.source_text[s..e])
                            .collect::<Vec<_>>()
                            .concat();
                        captures.insert(name.clone(), text);
                    }
                }
            }
        }

        return true;
    }

    let p_children = named_children(pattern_node);
    let has_multi = p_children.iter().any(|c| {
        is_multi_placeholder(*c, ctx.pattern_text, ctx.repetitions)
            || matches!(
                wraps_special(*c, ctx.pattern_text, ctx.specials),
                Some(SpecialKind::Body)
            )
    });

    if !has_multi {
        if pattern_named != source_named {
            return false;
        }

        if pattern_named == 0 {
            if pattern_node.is_named() {
                return leaf_matches(
                    ctx.source_text,
                    ctx.pattern_text,
                    source_node,
                    pattern_node,
                );
            }
            return true;
        }
    }

    let s_children = named_children(source_node);
    let p_children = named_children(pattern_node);

    match_children_suffix(&s_children, &p_children, 0, 0, captures, ctx)
}

/// Check if a pattern node is a multi-child placeholder (`$$$name` or `$$$`).
/// Only matches leaf nodes. For statement-level repetition (where tree-sitter
/// wraps the sentinel in `expression_statement`), use `$($name)*` instead.
fn is_multi_placeholder(
    node: Node,
    pattern_text: &str,
    repetitions: &HashMap<String, RepetitionInfo>,
) -> bool {
    if !node.is_named() || node.named_child_count() != 0 {
        return false;
    }
    let text = &pattern_text[node.start_byte()..node.end_byte()];
    matches!(repetitions.get(text), Some(info) if info.kind == RepetitionKind::MultiChild)
}

/// Check if a source node kind represents a statement.
fn is_statement_kind(kind: &str) -> bool {
    kind.ends_with("_statement")
        || kind == "block"
        || kind == "local_variable_declaration"
        || kind == "switch_block_statement_group"
        || kind == "labeled_statement"
}

/// Check if a pattern node wraps a special placeholder sentinel — i.e.,
/// it's a single-child wrapper (like `expression_statement`) whose only
/// named child is a `$BODY`, `$STMT`, or `$EXPR` sentinel.
fn wraps_special(
    node: Node,
    pattern_text: &str,
    specials: &HashMap<String, SpecialKind>,
) -> Option<SpecialKind> {
    if node.named_child_count() == 1 {
        if let Some(child) = node.named_child(0) {
            if child.named_child_count() == 0 {
                let text = &pattern_text[child.start_byte()..child.end_byte()];
                return specials.get(text).cloned();
            }
        }
    }
    None
}

/// Extract the inner sentinel text from a single-child wrapper node.
fn inner_sentinel(node: Node, pattern_text: &str) -> Option<String> {
    if node.named_child_count() == 1 {
        if let Some(child) = node.named_child(0) {
            if child.named_child_count() == 0 {
                return Some(pattern_text[child.start_byte()..child.end_byte()].to_string());
            }
        }
    }
    None
}

/// Recursively match a child pattern node against a single source node.
/// Thin wrapper that delegates to `structurally_matches`.
fn match_single_child(
    s_child: Node,
    p_child: Node,
    captures: &mut HashMap<String, String>,
    ctx: &MatchCtx,
) -> bool {
    structurally_matches(s_child, p_child, captures, ctx)
}

/// Match a suffix of pattern children against a suffix of source children,
/// handling `$$$name` multi-child placeholders with backtracking.
///
/// Returns true if the remaining children match. On success, `captures`
/// includes placeholders from the matched suffix.
fn match_children_suffix(
    s_children: &[Node<'_>],
    p_children: &[Node<'_>],
    si: usize,
    pi: usize,
    captures: &mut HashMap<String, String>,
    ctx: &MatchCtx,
) -> bool {
    if pi >= p_children.len() {
        return si >= s_children.len();
    }

    let p_child = p_children[pi];

    if let Some(SpecialKind::Body) = wraps_special(p_child, ctx.pattern_text, ctx.specials) {
        let sentinel = inner_sentinel(p_child, ctx.pattern_text).unwrap_or_default();
        let min = 0;
        let max = s_children
            .len()
            .saturating_sub(si)
            .saturating_sub(p_children.len().saturating_sub(pi + 1));

        for n in min..=max {
            let saved = captures.clone();
            let end = si + n;

            if let Some(name) = ctx.reverse_placeholders.get(&sentinel) {
                if n > 0 {
                    let first = s_children[si];
                    let last = s_children[end - 1];
                    let text = &ctx.source_text[first.start_byte()..last.end_byte()];
                    captures.insert(name.clone(), text.to_string());
                } else {
                    captures.insert(name.clone(), String::new());
                }
            }

            if match_children_suffix(s_children, p_children, end, pi + 1, captures, ctx) {
                return true;
            }

            *captures = saved;
        }

        return false;
    }

    if is_multi_placeholder(p_child, ctx.pattern_text, ctx.repetitions) {
        let sentinel = ctx.pattern_text[p_child.start_byte()..p_child.end_byte()].to_string();
        let rep = ctx.repetitions.get(&sentinel).unwrap();
        let min = if rep.op == "+" { 1 } else { 0 };
        let max = match rep.op.as_str() {
            "?" => 1,
            _ => s_children.len().saturating_sub(si).saturating_sub(
                p_children.len().saturating_sub(pi + 1),
            ),
        };

        for n in min..=max {
            let saved = captures.clone();
            let end = si + n;

            if let Some(name) = ctx.reverse_placeholders.get(&sentinel) {
                if n > 0 {
                    let first = s_children[si];
                    let last = s_children[end - 1];
                    let text = &ctx.source_text[first.start_byte()..last.end_byte()];
                    captures.insert(name.clone(), text.to_string());
                } else {
                    captures.insert(name.clone(), String::new());
                }
            }

            if match_children_suffix(s_children, p_children, end, pi + 1, captures, ctx) {
                return true;
            }

            *captures = saved;
        }

        return false;
    }

    if si >= s_children.len() {
        return false;
    }

    if !match_single_child(s_children[si], p_child, captures, ctx) {
        return false;
    }

    match_children_suffix(s_children, p_children, si + 1, pi + 1, captures, ctx)
}

/// Collect named children of `node` into a vector.
fn named_children<'a>(node: Node<'a>) -> Vec<Node<'a>> {
    let mut result = Vec::new();
    for i in 0..node.named_child_count() {
        if let Some(child) = node.named_child(i as u32) {
            result.push(child);
        }
    }
    result
}

/// If `pattern` is a single-child wrapper node (like `expression_statement`
/// wrapping an expression), return the inner child. This enables matching
/// through statement boundaries.
///
/// Skips unwrapping when the inner child is a bare placeholder or a leaf
/// (0 named children), both of which would match too broadly.
fn try_unwrap_pattern<'a>(pattern: Node<'a>, pattern_text: &'a str) -> Option<Node<'a>> {
    let count = pattern.named_child_count();
    if count == 1 {
        if let Some(child) = pattern.named_child(0) {
            if child.named_child_count() > 0 && !is_placeholder(child, pattern_text) {
                return Some(child);
            }
        }
    }
    None
}

/// Check if `node`'s byte range is entirely inside any match in `matches`.
/// Used to avoid duplicate matches from statement unwrapping.
fn overlaps_matched_range(node: Node, matches: &[Match]) -> bool {
    let start = node.start_byte();
    let end = node.end_byte();
    matches.iter().any(|m| {
        m.start_byte <= start && end <= m.end_byte && (m.start_byte < end || start < m.end_byte)
    })
}

/// Recursively traverse source tree collecting structural matches.
fn collect_matches(
    cursor: &mut TreeCursor,
    pattern: Node,
    matches: &mut Vec<Match>,
    ctx: &MatchCtx,
) {
    let node = cursor.node();

    // Try full pattern match
    let mut captures = HashMap::new();
    if structurally_matches(node, pattern, &mut captures, ctx) {
        matches.push(Match {
            start_byte: node.start_byte(),
            end_byte: node.end_byte(),
            start_point: node.start_position(),
            end_point: node.end_position(),
            captures,
        });
    } else if !overlaps_matched_range(node, matches) {
        // Try unwrapped pattern — strip statement-level wrappers
        // to match deeper (e.g., method calls inside return_statements).
        // Skips bare placeholders to avoid overly broad matches.
        if let Some(inner) = try_unwrap_pattern(pattern, ctx.pattern_text) {
            let mut captures = HashMap::new();
            if structurally_matches(node, inner, &mut captures, ctx) {
                matches.push(Match {
                    start_byte: node.start_byte(),
                    end_byte: node.end_byte(),
                    start_point: node.start_position(),
                    end_point: node.end_position(),
                    captures,
                });
            }
        }
    }

    if cursor.goto_first_child() {
        loop {
            collect_matches(cursor, pattern, matches, ctx);
            if !cursor.goto_next_sibling() {
                break;
            }
        }
        cursor.goto_parent();
    }
}

struct ProcessedSnippet {
    pattern_text: String,
    placeholders: HashMap<String, String>,
    repetitions: HashMap<String, RepetitionInfo>,
    specials: HashMap<String, SpecialKind>,
    /// Maps sentinel → tree-sitter AST kind name that the placeholder
    /// is constrained to match. E.g. `$x:identifier` stores sentinel → "identifier".
    type_constraints: HashMap<String, String>,
}

/// Preprocess a snippet, replacing `$name` placeholders with unique
/// sentinel identifiers so tree-sitter can parse them.
///
/// Also handles Rust-style repetition syntax:
/// - `$($name),*` — zero or more comma-separated
/// - `$($name),+` — one or more comma-separated
///
/// Returns the processed snippet text, a map of placeholder names,
/// and a map of sentinels with repetition info.
fn preprocess_snippet(
    snippet: &str,
) -> ProcessedSnippet {
    let mut result = String::with_capacity(snippet.len());
    let mut placeholders: HashMap<String, String> = HashMap::new();
    let mut repetitions: HashMap<String, RepetitionInfo> = HashMap::new();
    let mut specials: HashMap<String, SpecialKind> = HashMap::new();
    let mut type_constraints: HashMap<String, String> = HashMap::new();
    let mut counter = 0usize;
    let mut chars = snippet.char_indices().peekable();

    while let Some((_, c)) = chars.next() {
        if c == '$' {
            if let Some(&(_, next)) = chars.peek() {
                if next == '(' {
                    // Rust-style repetition: $($name)sep*  or  $($name)sep+
                    chars.next(); // consume '('

                    // Read inner content until matching ')'
                    let mut inner = String::new();
                    let mut depth = 1;
                    let mut balanced = false;
                    while let Some(&(_, ic)) = chars.peek() {
                        if ic == '(' {
                            depth += 1;
                            inner.push(ic);
                            chars.next();
                        } else if ic == ')' {
                            depth -= 1;
                            chars.next();
                            if depth == 0 {
                                balanced = true;
                                break;
                            }
                            inner.push(ic);
                        } else {
                            inner.push(ic);
                            chars.next();
                        }
                    }

                    if !balanced {
                        // Unmatched parens — treat as literal
                        result.push_str("$(");
                        result.push_str(&inner);
                        continue;
                    }

                    // Read separator (anything between ')' and operator)
                    let mut sep = String::new();
                    while let Some(&(_, sc)) = chars.peek() {
                        if sc == '*' || sc == '+' || sc == '?' {
                            break;
                        }
                        sep.push(sc);
                        chars.next();
                    }

                    // Read operator
                    let op = match chars.next() {
                        Some((_, '*')) => "*".to_string(),
                        Some((_, '+')) => "+".to_string(),
                        Some((_, '?')) => "?".to_string(),
                        _ => {
                            // Invalid repetition syntax — treat as literal
                            result.push_str("$(");
                            result.push_str(&inner);
                            result.push_str(&sep);
                            continue;
                        }
                    };

                    // Extract ALL $name placeholders from the inner content
                    // (supports multi-item repetitions like `$($a + $b),*`)
                    let mut sub_sentinels: Vec<String> = Vec::new();
                    let mut search_pos = 0;
                    while let Some(pos) = inner[search_pos..].find('$') {
                        let abs_pos = search_pos + pos;
                        let after_dollar = &inner[abs_pos + 1..];
                        let name_len = after_dollar.chars()
                            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                            .count();
                        if name_len > 0 {
                            let sub_name: String = after_dollar[..name_len].to_string();
                            let sentinel = if let Some(existing) = placeholders.get(&sub_name) {
                                existing.clone()
                            } else {
                                counter += 1;
                                let s = format!("_pw_{}", counter);
                                placeholders.insert(sub_name.clone(), s.clone());
                                s
                            };
                            sub_sentinels.push(sentinel);
                        }
                        search_pos = abs_pos + 1 + name_len;
                    }

                    if sub_sentinels.is_empty() {
                        // Not valid repetition syntax — treat as literal
                        result.push_str("$(");
                        result.push_str(&inner);
                        result.push_str(&sep);
                        result.push_str(&op);
                        continue;
                    }

                    if sub_sentinels.len() == 1 {
                        // Single-item: use the placeholder's own sentinel
                        let sentinel = &sub_sentinels[0];
                        result.push_str(sentinel);
                        repetitions.insert(sentinel.clone(), RepetitionInfo {
                            op,
                            kind: RepetitionKind::LastChild,
                            sub_sentinels: Vec::new(),
                        });
                    } else {
                        // Multi-item: create a group sentinel
                        counter += 1;
                        let group_sentinel = format!("_pw_{}", counter);
                        repetitions.insert(group_sentinel.clone(), RepetitionInfo {
                            op,
                            kind: RepetitionKind::LastChild,
                            sub_sentinels,
                        });
                        result.push_str(&group_sentinel);
                    }
                    continue;
                } else if next == '$' {
                    // Check for $$$ — multi-child placeholder
                    let mut lookahead = chars.clone();
                    lookahead.next(); // consume second $
                    if let Some(&(_, c3)) = lookahead.peek() {
                        if c3 == '$' {
                            // $$$ found — consume all three $ signs
                            chars.next(); // second $
                            chars.next(); // third $
                            // Read optional identifier name
                            let mut multi_name = String::new();
                            while let Some(&(_, mc)) = chars.peek() {
                                if mc.is_ascii_alphanumeric() || mc == '_' {
                                    multi_name.push(mc);
                                    chars.next();
                                } else {
                                    break;
                                }
                            }
                            counter += 1;
                            let sentinel = format!("_pw_multi_{}", counter);
                            if !multi_name.is_empty() {
                                placeholders.insert(multi_name, sentinel.clone());
                            }
                            repetitions.insert(sentinel.clone(), RepetitionInfo {
                                op: "*".to_string(),
                                kind: RepetitionKind::MultiChild,
                                sub_sentinels: Vec::new(),
                            });
                            result.push_str(&sentinel);
                            continue;
                        }
                    }
                    // Not $$$ — treat $ as literal, fall through
                } else if next.is_ascii_alphabetic() || next == '_' {
                    let mut name = String::new();
                    while let Some(&(_, c)) = chars.peek() {
                        if c.is_ascii_alphanumeric() || c == '_' {
                            name.push(c);
                            chars.next();
                        } else {
                            break;
                        }
                    }
                    // Check for pre-defined special tokens: $BODY, $STMT, $EXPR
                    let special_kind = match name.as_str() {
                        "BODY" => Some(SpecialKind::Body),
                        "STMT" => Some(SpecialKind::Stmt),
                        "EXPR" => Some(SpecialKind::Expr),
                        _ => None,
                    };
                    if let Some(kind) = special_kind {
                        counter += 1;
                        let sentinel = match kind {
                            SpecialKind::Body => format!("_pw_body_{}", counter),
                            SpecialKind::Stmt => format!("_pw_stmt_{}", counter),
                            SpecialKind::Expr => format!("_pw_expr_{}", counter),
                        };
                        placeholders.insert(name, sentinel.clone());
                        if kind == SpecialKind::Body {
                            repetitions.insert(sentinel.clone(), RepetitionInfo {
                                op: "*".to_string(),
                                kind: RepetitionKind::MultiChild,
                                sub_sentinels: Vec::new(),
                            });
                        }
                        // $BODY and $STMT need a semicolon to be valid
                        // statements in tree-sitter; $EXPR does not.
                        result.push_str(&sentinel);
                        if kind != SpecialKind::Expr {
                            result.push(';');
                        }
                        specials.insert(sentinel, kind);
                        continue;
                    }
                    if let Some(existing) = placeholders.get(&name) {
                        result.push_str(existing);
                    } else {
                        counter += 1;
                        let sentinel = format!("_pw_{}", counter);
                        placeholders.insert(name.clone(), sentinel.clone());
                        result.push_str(&sentinel);
                    }
                    // Check for Rust-macro-style type constraint: $name:Kind
                    if let Some(&(_, next_c)) = chars.peek() {
                        if next_c == ':' {
                            chars.next(); // consume ':'
                            let mut kind_name = String::new();
                            while let Some(&(_, c)) = chars.peek() {
                                if c.is_ascii_alphanumeric() || c == '_' {
                                    kind_name.push(c);
                                    chars.next();
                                } else {
                                    break;
                                }
                            }
                            if !kind_name.is_empty() {
                                if let Some(sentinel) = placeholders.get(&name) {
                                    type_constraints.entry(sentinel.clone()).or_insert(kind_name);
                                }
                            }
                        }
                    }
                    continue;
                }
            }
        }
        result.push(c);
    }

    ProcessedSnippet { pattern_text: result, placeholders, repetitions, specials, type_constraints }
}

/// Find all snippet matches in source text.
///
/// Named nodes (identifiers, literals) are matched by text content by
/// default. Prefix an identifier with `$` (e.g. `$x`) to match any
/// value at that position.
///
/// Use `$($name)sep*` / `$($name)sep+` (Rust macro repetition syntax)
/// for zero-or-more (`*`), one-or-more (`+`), or optional (`?`) matching
/// in the last child position.
///
/// Use `$name:Kind` to constrain a placeholder to a specific tree-sitter
/// AST kind (e.g. `$x:identifier` matches only identifiers).
pub fn find_snippet_matches(
    source: &str,
    snippet: &str,
    lang: &Language,
) -> Result<Vec<Match>, String> {
    let processed = preprocess_snippet(snippet);
    let reverse_ph = reverse_placeholder_map(&processed.placeholders);

    let mut parser = Parser::new();
    parser
        .set_language(lang)
        .map_err(|e| format!("Failed to set language: {}", e))?;

    let snippet_tree = parser
        .parse(&processed.pattern_text, None)
        .ok_or("Failed to parse snippet")?;
    let pattern = extract_pattern(snippet_tree.root_node())
        .ok_or("Could not extract pattern from snippet")?;

    if has_error(pattern) {
        return Err("Snippet contains syntax errors".to_string());
    }

    let source_tree = parser.parse(source, None).ok_or("Failed to parse source")?;

    let ctx = MatchCtx {
        source_text: source,
        pattern_text: &processed.pattern_text,
        reverse_placeholders: &reverse_ph,
        repetitions: &processed.repetitions,
        specials: &processed.specials,
        type_constraints: &processed.type_constraints,
    };

    let mut matches = Vec::new();
    let mut cursor = source_tree.root_node().walk();
    collect_matches(&mut cursor, pattern, &mut matches, &ctx);
    Ok(matches)
}

/// Find matches using a tree-sitter S-expression query.
pub fn find_query_matches(
    source: &str,
    query_str: &str,
    lang: &Language,
) -> Result<Vec<Match>, String> {
    let query = Query::new(lang, query_str)
        .map_err(|e| format!("Query error at {}:{}: {}", e.row, e.column, e.message))?;

    let mut parser = Parser::new();
    parser
        .set_language(lang)
        .map_err(|_| "Failed to set language".to_string())?;
    let source_tree = parser.parse(source, None).ok_or("Failed to parse source")?;

    let mut qc = QueryCursor::new();
    let mut query_matches = qc.matches(&query, source_tree.root_node(), source.as_bytes());

    let capture_names: Vec<&str> = query.capture_names().iter().map(|s| s.as_ref()).collect();
    let mut results = Vec::new();

    while let Some(qm) = query_matches.next() {
        if qm.captures.is_empty() {
            continue;
        }

        let target = qm
            .captures
            .iter()
            .find(|c| {
                capture_names
                    .get(c.index as usize)
                    .map(|n| *n == "matched" || *n == "target")
                    .unwrap_or(false)
            })
            .unwrap_or(&qm.captures[0]);

        let node = target.node;
        results.push(Match {
            start_byte: node.start_byte(),
            end_byte: node.end_byte(),
            start_point: node.start_position(),
            end_point: node.end_position(),
            captures: HashMap::new(),
        });
    }

    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn java_lang() -> Language {
        tree_sitter_java::LANGUAGE.into()
    }

    fn python_lang() -> Language {
        tree_sitter_python::LANGUAGE.into()
    }

    fn js_lang() -> Language {
        tree_sitter_javascript::LANGUAGE.into()
    }

    // ——— by default, leaf nodes match by name ———

    #[test]
    fn test_exact_name_match() {
        // Identifiers must match exactly by default
        let source = "class A { void f() { x = 1; } }";
        let matches = find_snippet_matches(source, "x = 1;", &java_lang()).unwrap();
        assert_eq!(matches.len(), 1);
    }

    #[test]
    fn test_identifier_name_mismatch() {
        // Different identifier name, no match
        let source = "class A { void f() { y = 1; } }";
        let matches = find_snippet_matches(source, "x = 1;", &java_lang()).unwrap();
        assert_eq!(matches.len(), 0);
    }

    #[test]
    fn test_literal_value_mismatch() {
        // Different integer value, no match
        let source = "class A { void f() { return 42; } }";
        let matches = find_snippet_matches(source, "return 1;", &java_lang()).unwrap();
        assert_eq!(matches.len(), 0);
    }

    #[test]
    fn test_string_literal_mismatch() {
        let source = "class A { void f() { s = \"hello\"; } }";
        let matches = find_snippet_matches(source, "s = \"world\";", &java_lang()).unwrap();
        assert_eq!(matches.len(), 0);
    }

    // ——— $ placeholder wildcard matching ———

    #[test]
    fn test_dollar_placeholder_matches_any_identifier() {
        let source = "class A { void f() { println(42); } }";
        let matches = find_snippet_matches(source, "$method(42);", &java_lang()).unwrap();
        assert_eq!(matches.len(), 1);
    }

    #[test]
    fn test_dollar_placeholder_matches_any_literal() {
        let source = "class A { void f() { return 42; } }";
        let matches = find_snippet_matches(source, "return $n;", &java_lang()).unwrap();
        assert_eq!(matches.len(), 1);
    }

    #[test]
    fn test_dollar_placeholder_matches_any_string() {
        let source = "class A { void f() { s = \"hello\"; } }";
        let matches = find_snippet_matches(source, "s = $s;", &java_lang()).unwrap();
        assert_eq!(matches.len(), 1);
    }

    #[test]
    fn test_dollar_multiple_placeholders() {
        let source = "class A { void f() { foo(1, 2); } }";
        let matches = find_snippet_matches(source, "$f($a, $b);", &java_lang()).unwrap();
        assert_eq!(matches.len(), 1);
    }

    #[test]
    fn test_multiple_matches_with_placeholder() {
        let source = r#"
class A {
    void f() { return 1; }
    void g() { return 2; }
    void h() { return 3; }
}"#;
        let matches = find_snippet_matches(source, "return $n;", &java_lang()).unwrap();
        assert_eq!(matches.len(), 3);
    }

    // ——— existing test cases adapted for name-matched default ———

    #[test]
    fn test_same_kind_leaves_match() {
        let source = "class A { void f() { return 1; } }";
        let matches = find_snippet_matches(source, "return 1;", &java_lang()).unwrap();
        assert_eq!(matches.len(), 1);
    }

    #[test]
    fn test_different_structure_no_match() {
        let source = "class A { void f() { if (true) {} } }";
        let matches = find_snippet_matches(source, "return 1;", &java_lang()).unwrap();
        assert!(matches.is_empty());
    }

    #[test]
    fn test_snippet_collects_matches() {
        let source = r#"
class A {
    void f() {
        System.out.println("hello");
        System.out.println("world");
    }
}"#;
        let matches =
            find_snippet_matches(source, r#"System.out.println("hello");"#, &java_lang()).unwrap();
        assert_eq!(matches.len(), 1);
    }

    #[test]
    fn test_no_match_for_absent_code() {
        let source = "class A { void f() { return 1; } }";
        let matches =
            find_snippet_matches(source, "System.out.println(\"x\");", &java_lang()).unwrap();
        assert!(matches.is_empty());
    }

    #[test]
    fn test_empty_source_no_match() {
        let matches = find_snippet_matches("", "return 1;", &java_lang()).unwrap();
        assert!(matches.is_empty());
    }

    #[test]
    fn test_deeply_nested_match() {
        let source = r#"
class Outer {
    class Inner {
        void f() {
            if (true) {
                return 1;
            }
        }
    }
}"#;
        let matches = find_snippet_matches(source, "return 1;", &java_lang()).unwrap();
        assert_eq!(matches.len(), 1);
    }

    #[test]
    fn test_match_byte_ranges_correct() {
        let source = "class A { int x; void f() { return 1; } }";
        let matches = find_snippet_matches(source, "return 1;", &java_lang()).unwrap();
        assert_eq!(matches.len(), 1);
        let m = &matches[0];
        assert_eq!(&source[m.start_byte..m.end_byte], "return 1;");
    }

    #[test]
    fn test_snippet_error_returns_err() {
        let result = find_snippet_matches("class A {}", "return ", &java_lang());
        assert!(result.is_err());
    }

    // ——— Python snippet matching ———

    #[test]
    fn test_python_snippet_match() {
        let source = "x = 42\n";
        let matches = find_snippet_matches(source, "x = 42", &python_lang()).unwrap();
        assert_eq!(matches.len(), 1);
    }

    #[test]
    fn test_python_multiple_matches() {
        let source = "a = 1\nb = 2\n";
        let matches = find_snippet_matches(source, "$x = $n", &python_lang()).unwrap();
        assert_eq!(matches.len(), 2);
    }

    #[test]
    fn test_python_no_match() {
        let source = "def f():\n    pass\n";
        let matches = find_snippet_matches(source, "return 1", &python_lang()).unwrap();
        assert!(matches.is_empty());
    }

    // ——— JavaScript snippet matching ———

    #[test]
    fn test_js_snippet_match() {
        let source = "function f() { return 42; }";
        let matches = find_snippet_matches(source, "return 42;", &js_lang()).unwrap();
        assert_eq!(matches.len(), 1);
    }

    #[test]
    fn test_js_no_match() {
        let source = "const x = 1;";
        let matches = find_snippet_matches(source, "return 1;", &js_lang()).unwrap();
        assert!(matches.is_empty());
    }

    // ——— query matching ———

    #[test]
    fn test_query_find() {
        let source = "class A { void f() { return 42; } }";
        let matches =
            find_query_matches(source, "(return_statement) @matched", &java_lang()).unwrap();
        assert_eq!(matches.len(), 1);
    }

    #[test]
    fn test_query_no_matches() {
        let source = "class A { void f() { return 42; } }";
        let matches = find_query_matches(source, "(if_statement) @matched", &java_lang()).unwrap();
        assert!(matches.is_empty());
    }

    #[test]
    fn test_query_prefers_matched_capture() {
        let source = "class A { void f() { return 42; } }";
        let matches = find_query_matches(
            source,
            "(method_declaration name: (identifier) @matched)",
            &java_lang(),
        )
        .unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(&source[matches[0].start_byte..matches[0].end_byte], "f");
    }

    #[test]
    fn test_query_syntax_error() {
        let result = find_query_matches("class A {}", "((", &java_lang());
        assert!(result.is_err());
    }

    #[test]
    fn test_python_query() {
        let source = "def f():\n    return 42\n";
        let matches =
            find_query_matches(source, "(return_statement) @matched", &python_lang()).unwrap();
        assert_eq!(matches.len(), 1);
    }

    // ——— structurally_matches ———

    #[test]
    fn test_structurally_matches_fails_on_kind_mismatch() {
        let source = "class A { void f() { return 1; } }";
        let matches = find_snippet_matches(source, "if (true) {}", &java_lang()).unwrap();
        assert!(matches.is_empty());
    }

    // ——— preprocess_snippet ———

    #[test]
    fn test_preprocess_replaces_dollar() {
        let ps = preprocess_snippet("$x = $y;");
        assert_eq!(ps.pattern_text, "_pw_1 = _pw_2;");
    }

    #[test]
    fn test_preprocess_no_dollar() {
        let ps = preprocess_snippet("x = y;");
        assert_eq!(ps.pattern_text, "x = y;");
    }

    // ——— repetition syntax ———

    #[test]
    fn test_preprocess_repetition_star() {
        let ps = preprocess_snippet("$f($($arg,)*)");
        // Should produce: _pw_1(_pw_2) with _pw_2 marked as repetition "*"
        assert_eq!(ps.pattern_text, "_pw_1(_pw_2)");
        assert_eq!(ps.placeholders.get("f").unwrap(), "_pw_1");
        assert_eq!(ps.placeholders.get("arg").unwrap(), "_pw_2");
        assert_eq!(ps.repetitions.get("_pw_2").unwrap().op, "*");
    }

    #[test]
    fn test_preprocess_repetition_plus() {
        let ps = preprocess_snippet("$($arg,)+");
        assert!(ps.repetitions.get("_pw_1").is_some());
        assert_eq!(ps.repetitions.get("_pw_1").unwrap().op, "+");
    }

    #[test]
    fn test_repetition_matches_zero_args() {
        // $f($($arg,)*) should match f() (zero args)
        let source = "class A { void m() { f(); } }";
        let matches = find_snippet_matches(source, "$f($($arg,)*);", &java_lang()).unwrap();
        assert_eq!(matches.len(), 1);
        let m = &matches[0];
        // $arg should capture empty string
        assert_eq!(m.captures.get("arg").map(|s| s.as_str()), Some(""));
    }

    #[test]
    fn test_repetition_matches_one_arg() {
        // $f($($arg,)*) should match f(x) (one arg)
        let source = "class A { void m() { f(x); } }";
        let matches = find_snippet_matches(source, "$f($($arg,)*);", &java_lang()).unwrap();
        assert_eq!(matches.len(), 1);
        let m = &matches[0];
        assert_eq!(m.captures.get("arg").map(|s| s.as_str()), Some("x"));
    }

    #[test]
    fn test_repetition_matches_multiple_args() {
        // $f($($arg,)*) should match f(x, y, z) (three args)
        let source = "class A { void m() { f(x, y, z); } }";
        let matches = find_snippet_matches(source, "$f($($arg,)*);", &java_lang()).unwrap();
        assert_eq!(matches.len(), 1);
        let m = &matches[0];
        assert_eq!(m.captures.get("f").unwrap(), "f");
        // Captured text spans all args including separators
        assert!(m.captures.get("arg").unwrap().len() >= 3);
        assert!(m.captures.get("arg").unwrap().contains('x'));
        assert!(m.captures.get("arg").unwrap().contains('y'));
        assert!(m.captures.get("arg").unwrap().contains('z'));
    }

    #[test]
    fn test_repetition_plus_requires_at_least_one() {
        // $f($($arg,)+) should NOT match f() (zero args)
        let source = "class A { void m() { f(); } }";
        let matches = find_snippet_matches(source, "$f($($arg,)+);", &java_lang()).unwrap();
        assert!(matches.is_empty());
    }

    #[test]
    fn test_repetition_plus_matches_one() {
        // $f($($arg,)+) should match f(x) (one arg)
        let source = "class A { void m() { f(x); } }";
        let matches = find_snippet_matches(source, "$f($($arg,)+);", &java_lang()).unwrap();
        assert_eq!(matches.len(), 1);
    }

    // ——— statement unwrapping ———

    #[test]
    fn test_unwrap_expression_in_return_statement() {
        // Pattern `users.$method()` (expression_statement) should match
        // a method call inside a return statement via unwrapping
        let source = "class A { void m() { return users.size(); } }";
        // With repetition: $($arg,)* so () matches zero args
        let matches =
            find_snippet_matches(source, "users.$method($($arg,)*);", &java_lang()).unwrap();
        assert_eq!(
            matches.len(),
            1,
            "should match return users.size() via unwrapping"
        );
        let m = &matches[0];
        assert_eq!(m.captures.get("method").unwrap(), "size");
    }

    #[test]
    fn test_unwrap_expression_in_variable_declaration() {
        // Pattern `users.$method()` should match
        // a method call inside a variable declaration
        let source = "class A { void m() { String s = users.get(id); } }";
        let matches =
            find_snippet_matches(source, "users.$method($($arg,)*);", &java_lang()).unwrap();
        assert_eq!(
            matches.len(),
            1,
            "should match users.get(id) in variable decl"
        );
        assert_eq!(matches[0].captures.get("method").unwrap(), "get");
    }

    #[test]
    fn test_unwrap_no_duplicate_with_direct_match() {
        // When the pattern directly matches an expression_statement,
        // we should NOT also get a duplicate inner match
        let source = "class A { void m() { users.size(); } }";
        let matches =
            find_snippet_matches(source, "users.$method($($arg,)*);", &java_lang()).unwrap();
        // Should get exactly 1 match (the expression_statement)
        assert_eq!(matches.len(), 1);
    }

    // ——— combined: $method() matching on UserService.java ———

    #[test]
    fn test_method_with_any_args_matches_all_forms() {
        // The original failing case: users.$method() should match
        // all method calls on users regardless of arguments
        let source = r#"
class A {
    void f() {
        users.size();
        String s = users.get(id);
        return users.remove(id);
    }
}"#;
        let matches =
            find_snippet_matches(source, "users.$method($($arg,)*);", &java_lang()).unwrap();
        // Should match all three: users.size(), users.get(id), users.remove(id)
        assert_eq!(matches.len(), 3, "should match all three forms");
    }

    // ——— optional repetition (?) ———

    #[test]
    fn test_repetition_optional_matches_zero() {
        // $f($($arg,)?) should match f() (zero args)
        let source = "class A { void m() { f(); } }";
        let matches = find_snippet_matches(source, "$f($($arg,)?);", &java_lang()).unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].captures.get("arg").map(|s| s.as_str()), Some(""));
    }

    #[test]
    fn test_repetition_optional_matches_one() {
        // $f($($arg,)?) should match f(x) (one arg)
        let source = "class A { void m() { f(x); } }";
        let matches = find_snippet_matches(source, "$f($($arg,)?);", &java_lang()).unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].captures.get("arg").map(|s| s.as_str()), Some("x"));
    }

    #[test]
    fn test_repetition_optional_rejects_two() {
        // $f($($arg,)?) should NOT match f(x, y) (two args)
        let source = "class A { void m() { f(x, y); } }";
        let matches = find_snippet_matches(source, "$f($($arg,)?);", &java_lang()).unwrap();
        assert!(matches.is_empty());
    }

    // ——— $$$ multi-child placeholder ———

    #[test]
    fn test_triple_dollar_matches_zero_args() {
        // $$$args should match f() (zero args)
        let source = "class A { void m() { f(); } }";
        let matches =
            find_snippet_matches(source, "$f($$$args);", &java_lang()).unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(
            matches[0].captures.get("args").map(|s| s.as_str()),
            Some("")
        );
    }

    #[test]
    fn test_triple_dollar_matches_one_arg() {
        // $$$args should match f(x) (one arg)
        let source = "class A { void m() { f(x); } }";
        let matches =
            find_snippet_matches(source, "$f($$$args);", &java_lang()).unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(
            matches[0].captures.get("args").map(|s| s.as_str()),
            Some("x")
        );
    }

    #[test]
    fn test_triple_dollar_matches_multiple_args() {
        // $$$args should match f(x, y, z) (multiple args)
        let source = "class A { void m() { f(x, y, z); } }";
        let matches =
            find_snippet_matches(source, "$f($$$args);", &java_lang()).unwrap();
        assert_eq!(matches.len(), 1);
        let cap = matches[0].captures.get("args").unwrap();
        assert!(cap.contains('x'), "should contain x: {}", cap);
        assert!(cap.contains('y'), "should contain y: {}", cap);
        assert!(cap.contains('z'), "should contain z: {}", cap);
    }

    #[test]
    fn test_triple_dollar_unnamed() {
        // Bare $$$ should match but not capture
        let source = "class A { void m() { f(a, b, c); } }";
        let matches =
            find_snippet_matches(source, "$f($$$);", &java_lang()).unwrap();
        assert_eq!(matches.len(), 1);
    }

    #[test]
    fn test_triple_dollar_matches_any_args() {
        // $$$args matches any number of arguments in function calls
        let source = "class A { void m() { f(); g(x); h(a, b, c); } }";
        // Match any method call (single-identifier name) with any args
        let matches =
            find_snippet_matches(source, "$fn($$$args);", &java_lang()).unwrap();
        assert_eq!(matches.len(), 3, "should match f(), g(x), h(a,b,c)");
    }

    #[test]
    fn test_triple_dollar_combined_with_single() {
        // $$$ and $ work together
        let source = "class A { void f() { log(msg); } }";
        let matches =
            find_snippet_matches(source, "$fn($$$args);", &java_lang()).unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(
            matches[0].captures.get("fn").map(|s| s.as_str()),
            Some("log")
        );
        assert_eq!(
            matches[0].captures.get("args").map(|s| s.as_str()),
            Some("msg")
        );
    }

    #[test]
    fn test_triple_dollar_matches_all_method_calls() {
        // $$$ matches any method name on users, $($arg,)* handles args
        let source = r#"
class A {
    void f() {
        users.size();
        String s = users.get(id);
        return users.remove(id);
    }
}"#;
        let matches =
            find_snippet_matches(source, "users.$$$($($arg,)*);", &java_lang()).unwrap();
        assert_eq!(matches.len(), 3, "should match all three forms");
    }

    #[test]
    fn test_triple_dollar_preprocess() {
        let ps = preprocess_snippet("$$$args");
        assert!(ps.pattern_text.starts_with("_pw_multi_"), "got: {}", ps.pattern_text);
        assert_eq!(ps.placeholders.get("args").map(|s| s.as_str()), Some(ps.pattern_text.as_str()));
        let rep = ps.repetitions.get(ps.pattern_text.as_str()).unwrap();
        assert_eq!(rep.op, "*");
        assert_eq!(rep.kind, RepetitionKind::MultiChild);
    }

    #[test]
    fn test_triple_dollar_preprocess_unnamed() {
        let ps = preprocess_snippet("$$$");
        assert!(ps.pattern_text.starts_with("_pw_multi_"), "got: {}", ps.pattern_text);
        // No placeholder name registered for anonymous $$$
        assert!(!ps.placeholders.contains_key(""));
        let rep = ps.repetitions.get(ps.pattern_text.as_str()).unwrap();
        assert_eq!(rep.kind, RepetitionKind::MultiChild);
    }

    // ——— special tokens: $BODY, $STMT, $EXPR ———

    #[test]
    fn test_body_matches_zero_statements() {
        // if (true) { $BODY } should match an empty block
        let source = "class A { void m() { if (true) {} } }";
        let matches =
            find_snippet_matches(source, "if (true) { $BODY }", &java_lang()).unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].captures.get("BODY").map(|s| s.as_str()), Some(""));
    }

    #[test]
    fn test_body_matches_single_statement() {
        let source = "class A { void m() { if (true) { return 42; } } }";
        let matches =
            find_snippet_matches(source, "if (true) { $BODY }", &java_lang()).unwrap();
        assert_eq!(matches.len(), 1);
        let cap = matches[0].captures.get("BODY").unwrap();
        assert!(cap.contains("return"), "should contain return: {}", cap);
    }

    #[test]
    fn test_body_matches_multiple_statements() {
        let source = "class A { void m() { if (true) { debug(x); return 42; } } }";
        let matches =
            find_snippet_matches(source, "if (true) { $BODY }", &java_lang()).unwrap();
        assert_eq!(matches.len(), 1);
        let cap = matches[0].captures.get("BODY").unwrap();
        assert!(cap.contains("debug"), "should contain debug: {}", cap);
        assert!(cap.contains("return"), "should contain return: {}", cap);
    }

    #[test]
    fn test_body_preprocess() {
        let ps = preprocess_snippet("$BODY");
        assert!(ps.pattern_text.starts_with("_pw_body_"), "got: {}", ps.pattern_text);
        // $BODY appends semicolon for valid statement parsing
        let sentinel = ps.pattern_text.trim_end_matches(';');
        assert_eq!(ps.placeholders.get("BODY").map(|s| s.as_str()), Some(sentinel));
        let rep = ps.repetitions.get(sentinel).unwrap();
        assert_eq!(rep.kind, RepetitionKind::MultiChild);
        assert_eq!(rep.op, "*");
        assert_eq!(ps.specials.get(sentinel), Some(&SpecialKind::Body));
    }

    #[test]
    fn test_stmt_at_statement_level() {
        // $STMT alone matches any single statement, including blocks.
        // Block and return_statement both qualify.
        let source = "class A { void m() { return 42; } }";
        let matches = find_snippet_matches(source, "$STMT", &java_lang()).unwrap();
        assert!(matches.len() >= 1);
        let has_return = matches.iter().any(|m| {
            m.captures.get("STMT").map(|s| s.as_str()) == Some("return 42;")
        });
        assert!(has_return, "should match return statement");
    }

    #[test]
    fn test_stmt_matches_if_statement() {
        let source = "class A { void m() { if (true) { return 1; } } }";
        let matches = find_snippet_matches(source, "$STMT", &java_lang()).unwrap();
        let has_if = matches.iter().any(|m| {
            m.captures.get("STMT").map(|s| s.as_str()) == Some("if (true) { return 1; }")
        });
        assert!(has_if, "should match if statement");
    }

    #[test]
    fn test_stmt_preprocess() {
        let ps = preprocess_snippet("$STMT");
        assert!(ps.pattern_text.starts_with("_pw_stmt_"), "got: {}", ps.pattern_text);
        let sentinel = ps.pattern_text.trim_end_matches(';');
        assert_eq!(ps.placeholders.get("STMT").map(|s| s.as_str()), Some(sentinel));
        assert_eq!(ps.specials.get(sentinel), Some(&SpecialKind::Stmt));
    }

    #[test]
    fn test_expr_at_expression_level() {
        // $EXPR as a leaf should match any expression (like $name)
        let source = "class A { void m() { debug(x); } }";
        let matches =
            find_snippet_matches(source, "debug($EXPR);", &java_lang()).unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].captures.get("EXPR").map(|s| s.as_str()), Some("x"));
    }

    #[test]
    fn test_expr_preprocess() {
        let ps = preprocess_snippet("$EXPR");
        assert!(ps.pattern_text.starts_with("_pw_expr_"), "got: {}", ps.pattern_text);
        assert_eq!(ps.placeholders.get("EXPR").map(|s| s.as_str()), Some(ps.pattern_text.as_str()));
        assert_eq!(ps.specials.get(ps.pattern_text.as_str()), Some(&SpecialKind::Expr));
    }

    #[test]
    fn test_body_combined_with_condition() {
        // if ($EXPR) { $BODY } matches any if statement
        let source = "class A { void m() { if (x > 0) { return x; } } }";
        let matches =
            find_snippet_matches(source, "if ($EXPR) { $BODY }", &java_lang()).unwrap();
        assert_eq!(matches.len(), 1);
        assert!(matches[0].captures.get("EXPR").unwrap().contains("x > 0"));
        assert!(matches[0].captures.get("BODY").unwrap().contains("return"));
    }

    // ——— type-constrained placeholders: $name:Kind ———

    #[test]
    fn test_type_constraint_identifier() {
        let source = "class A { void f() { x = 42; } }";
        let matches = find_snippet_matches(source, "$x:identifier = 42;", &java_lang()).unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].captures.get("x").map(|s| s.as_str()), Some("x"));
    }

    #[test]
    fn test_type_constraint_identifier_rejects_literal() {
        let source = "class A { void f() { return 42; } }";
        let matches = find_snippet_matches(source, "$x:identifier = 42;", &java_lang()).unwrap();
        assert!(matches.is_empty());
    }

    #[test]
    fn test_type_constraint_string_literal() {
        let source = r#"class A { void f() { log("hello"); } }"#;
        let matches =
            find_snippet_matches(source, r#"log($msg:string_literal);"#, &java_lang()).unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(
            matches[0].captures.get("msg").map(|s| s.as_str()),
            Some(r#""hello""#)
        );
    }

    #[test]
    fn test_type_constraint_string_literal_rejects_identifier() {
        let source = "class A { void f() { log(x); } }";
        let matches =
            find_snippet_matches(source, r#"log($msg:string_literal);"#, &java_lang()).unwrap();
        assert!(matches.is_empty());
    }

    #[test]
    fn test_type_constraint_number_literal() {
        let source = "class A { void f() { return 42; } }";
        let matches =
            find_snippet_matches(source, "return $n:decimal_integer_literal;", &java_lang()).unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].captures.get("n").map(|s| s.as_str()), Some("42"));
    }

    #[test]
    fn test_type_constraint_number_literal_rejects_identifier() {
        let source = "class A { void f() { return x; } }";
        let matches =
            find_snippet_matches(source, "return $n:decimal_integer_literal;", &java_lang()).unwrap();
        assert!(matches.is_empty());
    }

    #[test]
    fn test_type_constraint_multiple_placeholders() {
        let source = r#"class A { void f() { log("msg", 42); } }"#;
        let matches = find_snippet_matches(
            source,
            r#"log($msg:string_literal, $n:decimal_integer_literal);"#,
            &java_lang(),
        )
        .unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(
            matches[0].captures.get("msg").map(|s| s.as_str()),
            Some(r#""msg""#)
        );
        assert_eq!(matches[0].captures.get("n").map(|s| s.as_str()), Some("42"));
    }

    #[test]
    fn test_type_constraint_first_occurrence_wins_on_reuse() {
        let source = "class A { void f() { x = y; } }";
        let matches = find_snippet_matches(source, "$x:identifier = $x;", &java_lang()).unwrap();
        assert_eq!(matches.len(), 1);
    }

    #[test]
    fn test_type_constraint_no_trailing_colon_ambiguity() {
        let result = find_snippet_matches("class A { }", "$x:", &java_lang());
        assert!(result.is_err());
    }

    #[test]
    fn test_type_constraint_preprocess() {
        let ps = preprocess_snippet("$x:identifier");
        assert_eq!(ps.type_constraints.len(), 1);
        let sentinel = ps.placeholders.get("x").unwrap();
        assert_eq!(
            ps.type_constraints.get(sentinel).map(|s| s.as_str()),
            Some("identifier")
        );
    }

    #[test]
    fn test_type_constraint_preprocess_unnamed() {
        let ps = preprocess_snippet("$x:");
        assert!(ps.type_constraints.is_empty());
    }

    #[test]
    fn test_type_constraint_rejects_on_kind_mismatch() {
        let source = r#"class A { void f() { log(42); } }"#;
        let matches =
            find_snippet_matches(source, r#"log($x:string_literal);"#, &java_lang()).unwrap();
        assert!(matches.is_empty());
    }

    // ——— multi-item repetition groups ———

    #[test]
    fn test_multi_item_matches_all_groups() {
        // $($key, $val),* matches key-value pairs in flat argument list
        let source = "class A { void m() { f(x, 1, y, 2, z, 3); } }";
        let matches =
            find_snippet_matches(source, "$f($($key, $val),*);", &java_lang()).unwrap();
        assert_eq!(matches.len(), 1, "should match the call with 6 args");
        let cap_key = matches[0].captures.get("key").unwrap();
        let cap_val = matches[0].captures.get("val").unwrap();
        assert!(
            cap_key.contains('x') && cap_key.contains('y') && cap_key.contains('z'),
            "key should contain x, y, z: {}",
            cap_key
        );
        assert!(
            cap_val.contains('1') && cap_val.contains('2') && cap_val.contains('3'),
            "val should contain 1, 2, 3: {}",
            cap_val
        );
    }

    #[test]
    fn test_multi_item_single_group() {
        // Single group of multi-item repetition should match
        let source = "class A { void m() { f(a, 1); } }";
        let matches =
            find_snippet_matches(source, "$f($($key, $val),*);", &java_lang()).unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].captures.get("key").map(|s| s.as_str()), Some("a"));
        assert_eq!(matches[0].captures.get("val").map(|s| s.as_str()), Some("1"));
    }

    #[test]
    fn test_multi_item_zero_matches_allowed_by_star() {
        // $($key, $val),* should match f() with zero args
        let source = "class A { void m() { f(); } }";
        let matches =
            find_snippet_matches(source, "$f($($key, $val),*);", &java_lang()).unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].captures.get("key").map(|s| s.as_str()), Some(""));
        assert_eq!(matches[0].captures.get("val").map(|s| s.as_str()), Some(""));
    }

    #[test]
    fn test_multi_item_plus_requires_at_least_one() {
        // $($key, $val),+ should NOT match f() with zero args
        let source = "class A { void m() { f(); } }";
        let matches =
            find_snippet_matches(source, "$f($($key, $val),+);", &java_lang()).unwrap();
        assert!(matches.is_empty(), "plus quantifier requires at least one group");
    }

    #[test]
    fn test_multi_item_op_mismatch() {
        // $($key, $val),? should not match more than one group
        let source = "class A { void m() { f(a, 1, b, 2); } }";
        let matches =
            find_snippet_matches(source, "$f($($key, $val),?);", &java_lang()).unwrap();
        assert!(matches.is_empty(), "optional quantifier allows at most one group");
    }

    #[test]
    fn test_multi_item_combined_with_single_placeholder() {
        // Mix multi-item repetition with a regular placeholder before it
        let source = "class A { void m() { f(name, x, 1, y, 2); } }";
        let matches = find_snippet_matches(
            source,
            "$f($prefix, $($key, $val),*);",
            &java_lang(),
        )
        .unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].captures.get("prefix").map(|s| s.as_str()), Some("name"));
        assert!(matches[0].captures.get("key").unwrap().contains('x'));
        assert!(matches[0].captures.get("val").unwrap().contains('1'));
    }

    #[test]
    fn test_multi_item_preprocess() {
        let ps = preprocess_snippet("$($a, $b),*");
        // Should have both placeholders registered
        assert!(ps.placeholders.contains_key("a"));
        assert!(ps.placeholders.contains_key("b"));
        // Should have a repetition entry (group sentinel)
        assert_eq!(ps.repetitions.len(), 1);
        let (sentinel, info) = ps.repetitions.iter().next().unwrap();
        // sub_sentinels should be non-empty for multi-item
        assert_eq!(info.sub_sentinels.len(), 2);
        assert_eq!(info.sub_sentinels[0], *ps.placeholders.get("a").unwrap());
        assert_eq!(info.sub_sentinels[1], *ps.placeholders.get("b").unwrap());
        assert_eq!(info.kind, RepetitionKind::LastChild);
        // Pattern text should contain the group sentinel
        assert!(ps.pattern_text.contains(sentinel.as_str()));
    }

    #[test]
    fn test_multi_item_single_item_still_works() {
        // Existing single-item $($arg,)* should still work with backwards compat
        let source = "class A { void m() { f(a, b, c); } }";
        let matches =
            find_snippet_matches(source, "$f($($arg,)*);", &java_lang()).unwrap();
        assert_eq!(matches.len(), 1);
        let cap = matches[0].captures.get("arg").unwrap();
        assert!(cap.contains('a'));
        assert!(cap.contains('b'));
        assert!(cap.contains('c'));
        // Verify single-item uses empty sub_sentinels
        let ps = preprocess_snippet("$($arg,)*");
        let info = ps.repetitions.values().next().unwrap();
        assert!(info.sub_sentinels.is_empty());
    }

}
