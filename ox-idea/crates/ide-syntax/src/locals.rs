//! Scopes and locals from `queries/<grammar>/locals.scm` (nvim-treesitter capture
//! convention: `@local.scope`, `@local.definition[.<kind>]`, `@local.reference`,
//! `(#set! definition.<kind>.scope "parent")`).
//!
//! Resolution is lexical: a reference binds to the innermost enclosing scope
//! that declares its name. Only function-level scopes count as local (blocks,
//! functions, lambdas, loops, catch clauses…); a name declared at file, class
//! or module level, or in member position, is reported as not local.

use crate::kinds::{Kinds, Redeclaration, locals_source};
use crate::types::{LocalKind, LocalSymbol, Locals, NotLocalReason};
use crate::{Ctx, StructureError, StructureResult};
use ide_core::Pos;
use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};
use tree_sitter::{Language, Node, Query, QueryCursor, StreamingIterator};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DefKind {
    Local(LocalKind),
    /// Fields, methods, namespaces, enum members: never local.
    Member,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Role {
    Scope,
    Def(DefKind),
    Ref,
    Ignore,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ScopeHint {
    Own,
    Parent,
    Global,
}

enum Predicate {
    HasAncestor { capture: u32, kinds: Vec<String>, negate: bool },
    HasParent { capture: u32, kinds: Vec<String>, negate: bool },
}

pub(crate) struct LocalsQuery {
    query: Query,
    roles: Vec<Role>,
    /// Per capture: the definition kind as spelled in the query (`var`, `function`…).
    kind_names: Vec<String>,
    /// Per pattern: `(definition kind, where it lives)` from `#set! definition.<kind>.scope`.
    hints: Vec<Vec<(String, ScopeHint)>>,
    predicates: Vec<Vec<Predicate>>,
}

fn role(name: &str) -> Role {
    let Some(rest) = name.strip_prefix("local.") else { return Role::Ignore };
    if rest == "scope" {
        return Role::Scope;
    }
    if rest == "reference" {
        return Role::Ref;
    }
    let Some(def) = rest.strip_prefix("definition") else { return Role::Ignore };
    let kind = def.strip_prefix('.').unwrap_or("");
    Role::Def(match kind {
        "" => DefKind::Local(LocalKind::Other),
        "var" | "constant" => DefKind::Local(LocalKind::Var),
        "parameter" => DefKind::Local(LocalKind::Parameter),
        "function" => DefKind::Local(LocalKind::Function),
        "type" => DefKind::Local(LocalKind::Type),
        "import" => DefKind::Local(LocalKind::Import),
        "macro" => DefKind::Local(LocalKind::Macro),
        "label" => DefKind::Local(LocalKind::Label),
        "field" | "method" | "namespace" | "enum" | "associated" | "member" | "property" => DefKind::Member,
        _ => DefKind::Local(LocalKind::Other),
    })
}

impl LocalsQuery {
    fn new(language: &Language, source: &str) -> Result<Self, tree_sitter::QueryError> {
        let mut query = Query::new(language, source)?;
        let roles: Vec<Role> = query.capture_names().iter().map(|n| role(n)).collect();
        let kind_names = query
            .capture_names()
            .iter()
            .map(|n| n.strip_prefix("local.definition").unwrap_or("").trim_start_matches('.').to_owned())
            .collect();
        let mut hints = Vec::with_capacity(query.pattern_count());
        let mut predicates = Vec::with_capacity(query.pattern_count());
        let mut disabled = Vec::new();
        for p in 0..query.pattern_count() {
            let hint = query
                .property_settings(p)
                .iter()
                .filter_map(|s| {
                    let kind = s.key.strip_prefix("definition.")?.strip_suffix(".scope")?;
                    let hint = match s.value.as_deref() {
                        Some("parent") => ScopeHint::Parent,
                        Some("global") => ScopeHint::Global,
                        _ => return None,
                    };
                    Some((kind.to_owned(), hint))
                })
                .collect();
            hints.push(hint);
            let mut preds = Vec::new();
            for pred in query.general_predicates(p) {
                let negate = pred.operator.starts_with("not-");
                let op = pred.operator.trim_start_matches("not-");
                let capture = match pred.args.first() {
                    Some(tree_sitter::QueryPredicateArg::Capture(c)) => *c,
                    _ => {
                        disabled.push(p);
                        continue;
                    }
                };
                let kinds: Vec<String> = pred
                    .args
                    .iter()
                    .skip(1)
                    .filter_map(|a| match a {
                        tree_sitter::QueryPredicateArg::String(s) => Some(s.to_string()),
                        _ => None,
                    })
                    .collect();
                match op {
                    "has-ancestor?" => preds.push(Predicate::HasAncestor { capture, kinds, negate }),
                    "has-parent?" => preds.push(Predicate::HasParent { capture, kinds, negate }),
                    other => {
                        tracing::debug!("locals: unsupported predicate #{other} disables pattern {p}");
                        disabled.push(p);
                    }
                }
            }
            predicates.push(preds);
        }
        for p in disabled {
            query.disable_pattern(p);
        }
        Ok(Self { query, roles, kind_names, hints, predicates })
    }

    fn hint(&self, pattern: usize, capture: u32) -> ScopeHint {
        let kind = &self.kind_names[capture as usize];
        self.hints[pattern].iter().find(|(k, _)| k == kind || kind.is_empty()).map_or(ScopeHint::Own, |(_, h)| *h)
    }

    fn accepts(&self, pattern: usize, captures: &[tree_sitter::QueryCapture]) -> bool {
        self.predicates[pattern].iter().all(|pred| {
            let (capture, kinds, negate, ancestors) = match pred {
                Predicate::HasAncestor { capture, kinds, negate } => (*capture, kinds, *negate, true),
                Predicate::HasParent { capture, kinds, negate } => (*capture, kinds, *negate, false),
            };
            let Some(c) = captures.iter().find(|c| c.index == capture) else { return true };
            let mut cur = c.node.parent();
            let mut found = false;
            while let Some(n) = cur {
                if kinds.iter().any(|k| k == n.kind()) {
                    found = true;
                    break;
                }
                if !ancestors {
                    break;
                }
                cur = n.parent();
            }
            found != negate
        })
    }

    pub(crate) fn for_grammar(grammar: &'static str, language: &Language) -> Option<&'static LocalsQuery> {
        static CACHE: OnceLock<Mutex<HashMap<&'static str, Option<&'static LocalsQuery>>>> = OnceLock::new();
        let mut map = CACHE.get_or_init(Mutex::default).lock().unwrap_or_else(|e| e.into_inner());
        *map.entry(grammar).or_insert_with(|| {
            let source = locals_source(grammar)?;
            match LocalsQuery::new(language, source) {
                Ok(q) => Some(Box::leak(Box::new(q))),
                Err(e) => {
                    tracing::error!("locals query for {grammar} is invalid: {e}");
                    None
                }
            }
        })
    }
}

/// Validates the bundled query of `grammar` (tests).
#[cfg(test)]
pub(crate) fn compile(grammar: &str, language: &Language) -> Option<Result<(), String>> {
    let source = locals_source(grammar)?;
    Some(LocalsQuery::new(language, source).map(|_| ()).map_err(|e| format!("{e}")))
}

#[derive(Clone, Copy)]
struct Def<'t> {
    node: Node<'t>,
    kind: DefKind,
    hint: ScopeHint,
    pattern: usize,
}

/// Everything the query reports inside one byte range.
struct Captures<'t> {
    scopes: HashSet<usize>,
    defs: HashMap<usize, Def<'t>>,
    refs: HashMap<usize, Node<'t>>,
}

fn collect<'t>(
    cx: &Ctx<'t>,
    q: &LocalsQuery,
    range: std::ops::Range<usize>,
    name: Option<&str>,
) -> StructureResult<Captures<'t>> {
    let text = cx.text();
    let mut cursor = QueryCursor::new();
    cursor.set_byte_range(range);
    let mut out = Captures { scopes: HashSet::new(), defs: HashMap::new(), refs: HashMap::new() };
    let mut matches = cursor.matches(&q.query, cx.tree.root_node(), text.as_bytes());
    let mut n = 0usize;
    while let Some(m) = matches.next() {
        n += 1;
        if n.is_multiple_of(2048) {
            cx.check()?;
        }
        if !q.accepts(m.pattern_index, m.captures) {
            continue;
        }
        for c in m.captures {
            let node = c.node;
            let named = || name.is_none_or(|name| &text[node.byte_range()] == name);
            match q.roles[c.index as usize] {
                Role::Scope => {
                    out.scopes.insert(node.id());
                }
                Role::Def(DefKind::Member) if !cx.kinds.table.implicit_members => {}
                Role::Def(kind) if named() => {
                    let def = Def { node, kind, hint: q.hint(m.pattern_index, c.index), pattern: m.pattern_index };
                    // The earliest pattern wins when several capture the same node.
                    out.defs
                        .entry(node.id())
                        .and_modify(|d| {
                            if def.pattern < d.pattern {
                                *d = def;
                            }
                        })
                        .or_insert(def);
                }
                Role::Ref if named() => {
                    out.refs.insert(node.id(), node);
                }
                _ => {}
            }
        }
    }
    Ok(out)
}

pub(crate) fn locals<'t>(cx: &Ctx<'t>, pos: Pos) -> StructureResult<Locals> {
    let q = LocalsQuery::for_grammar(cx.grammar, cx.language).ok_or(StructureError::Unsupported)?;
    let text = cx.text();
    let b = cx.lines.byte(pos);
    let root = cx.tree.root_node();

    // 1. The identifier at the caret and the scopes around it.
    let around = collect(cx, q, b.saturating_sub(1)..(b + 1).min(text.len()), None)?;
    let covers = |n: &Node| n.start_byte() <= b && b <= n.end_byte() && n.start_byte() < n.end_byte();
    let mut hits: Vec<Node<'t>> =
        around.defs.values().map(|d| d.node).chain(around.refs.values().copied()).filter(|n| covers(n)).collect();
    // Prefer a token the caret is inside over one it merely touches, then the smallest.
    hits.sort_by_key(|n| (n.end_byte() == b && n.start_byte() < b, n.end_byte() - n.start_byte()));
    let Some(target) = hits.first().copied() else { return Ok(uncaptured(cx, b)) };
    let name = text[target.byte_range()].to_owned();
    let range = cx.range(&target);
    let not_local = |reason| Ok(Locals::NotLocal { name: name.clone(), range, reason });
    if cx.kinds.is_qualified(&target) || around.defs.get(&target.id()).is_some_and(|d| d.kind == DefKind::Member) {
        return not_local(NotLocalReason::Member);
    }

    // 2. The local region: the outermost scope around the caret below the
    //    file / class level. Nothing outside it can declare a local.
    let mut region = None;
    let mut cur = target.parent();
    while let Some(n) = cur {
        if around.scopes.contains(&n.id()) && n.parent().is_some() && !cx.kinds.is_member_scope(&n) {
            region = Some(n);
        }
        cur = n.parent();
    }
    let Some(region) = region else {
        let reason =
            if around.defs.contains_key(&target.id()) { NotLocalReason::TopLevel } else { NotLocalReason::Unresolved };
        return not_local(reason);
    };

    // 3. Every declaration and reference of the name inside the region.
    let caps = collect(cx, q, region.byte_range(), Some(&name))?;
    let scope_of = |node: Node<'t>| -> Option<Node<'t>> {
        let mut cur = node.parent();
        while let Some(n) = cur {
            if caps.scopes.contains(&n.id()) || n.parent().is_none() {
                return Some(n);
            }
            cur = n.parent();
        }
        None
    };
    let in_region = |n: &Node| region.start_byte() <= n.start_byte() && n.end_byte() <= region.end_byte();
    // Owning scope id -> (scope, declarations sorted by position with their visibility start).
    let mut by_scope: HashMap<usize, (Node<'t>, Vec<(Def<'t>, usize)>)> = HashMap::new();
    let mut owner_of: HashMap<usize, usize> = HashMap::new();
    for def in caps.defs.values() {
        if cx.kinds.is_qualified(&def.node) {
            continue;
        }
        let owner = match def.hint {
            // A function / type naming its own scope node lives in the enclosing one.
            ScopeHint::Own
                if matches!(def.kind, DefKind::Local(LocalKind::Function | LocalKind::Type))
                    && def.node.parent().is_some_and(|p| {
                        caps.scopes.contains(&p.id()) && crate::kinds::field_name(&def.node) == Some("name")
                    }) =>
            {
                scope_of(def.node).and_then(scope_of)
            }
            ScopeHint::Own => scope_of(def.node),
            ScopeHint::Parent => scope_of(def.node).and_then(scope_of),
            ScopeHint::Global => Some(root),
        };
        let Some(owner) = owner.filter(|o| in_region(o)) else { continue };
        let visible = visible_from(cx.kinds, def.node, owner);
        by_scope.entry(owner.id()).or_insert_with(|| (owner, Vec::new())).1.push((*def, visible));
        owner_of.insert(def.node.id(), owner.id());
    }
    for (_, defs) in by_scope.values_mut() {
        defs.sort_by_key(|(d, _)| d.node.start_byte());
    }
    let redecl = cx.kinds.redeclaration();
    // Occurrence -> (owning scope, declaration, the scope declares the name more than once).
    let resolve = |node: Node<'t>| -> Option<(Node<'t>, Def<'t>, bool)> {
        if let Some(owner) = owner_of.get(&node.id()) {
            let (scope, defs) = &by_scope[owner];
            let own = defs.iter().find(|(d, _)| d.node == node).map(|(d, _)| *d)?;
            let def = if redecl == Redeclaration::Merge { defs[0].0 } else { own };
            return Some((*scope, def, defs.len() > 1));
        }
        let mut cur = node.parent();
        while let Some(n) = cur {
            if !in_region(&n) {
                return None;
            }
            if let Some((scope, defs)) = by_scope.get(&n.id()) {
                let visible = defs.iter().rev().find(|(_, v)| *v <= node.start_byte()).map(|(d, _)| *d);
                let pick = match redecl {
                    Redeclaration::Merge => Some(defs[0].0),
                    Redeclaration::Shadow => visible,
                    Redeclaration::Conflict => Some(visible.unwrap_or(defs[0].0)),
                };
                if let Some(def) = pick {
                    return Some((*scope, def, defs.len() > 1));
                }
            }
            cur = n.parent();
        }
        None
    };

    let Some((scope, def, _)) = resolve(target) else {
        let reason =
            if caps.defs.contains_key(&target.id()) { NotLocalReason::TopLevel } else { NotLocalReason::Unresolved };
        return not_local(reason);
    };
    let DefKind::Local(kind) = def.kind else { return not_local(NotLocalReason::Member) };
    if cx.kinds.is_member_scope(&scope) {
        return not_local(NotLocalReason::TopLevel);
    }

    let same_scope = &by_scope[&scope.id()].1;
    let captured: HashSet<usize> =
        caps.refs.values().chain(caps.defs.values().map(|d| &d.node)).map(|n| n.start_byte()).collect();
    let certain = (same_scope.len() == 1 || redecl != Redeclaration::Conflict)
        && !hidden_occurrences(text, scope, &name, &captured);
    let mut references: Vec<Node<'t>> = caps
        .refs
        .values()
        .chain(caps.defs.values().map(|d| &d.node))
        .copied()
        .filter(|n| *n != def.node && !cx.kinds.is_qualified(n))
        .filter(|n| resolve(*n).is_some_and(|(s, d, _)| s == scope && d.node == def.node))
        .collect();
    references.sort_by_key(|n| n.start_byte());
    references.dedup_by_key(|n| n.id());
    cx.check()?;
    let conflicts = if certain {
        Vec::new()
    } else {
        same_scope.iter().filter(|(d, _)| d.node != def.node).map(|(d, _)| cx.range(&d.node)).collect()
    };
    Ok(Locals::Local {
        symbol: LocalSymbol {
            name,
            kind,
            definition: cx.range(&def.node),
            scope: cx.range(&scope),
            references: references.iter().map(|n| cx.range(n)).collect(),
            certain,
            conflicts,
        },
    })
}

/// An identifier the query does not capture (e.g. a JS `property_identifier`).
fn uncaptured(cx: &Ctx, b: usize) -> Locals {
    let text = cx.text();
    let root = cx.tree.root_node();
    let probe = |at: usize| {
        let end = (at + 1).min(text.len());
        root.descendant_for_byte_range(at, end)
            .filter(|n| n.is_named() && n.child_count() == 0 && n.start_byte() <= at && at < n.end_byte())
    };
    let leaf = probe(b).or_else(|| b.checked_sub(1).and_then(probe));
    match leaf {
        Some(n) if n.kind().contains("identifier") || n.kind() == "name" => Locals::NotLocal {
            name: text[n.byte_range()].to_owned(),
            range: cx.range(&n),
            reason: if cx.kinds.is_qualified(&n) { NotLocalReason::Member } else { NotLocalReason::Unresolved },
        },
        _ => Locals::NoIdentifier,
    }
}

/// Spellings of `name` in `scope` the query could not see, so the reference
/// list may be incomplete: inside `ERROR` nodes, or as a `$name` string
/// template the grammar keeps as plain text (Kotlin `"$x"`).
fn hidden_occurrences(text: &str, scope: Node, name: &str, captured: &HashSet<usize>) -> bool {
    let is_word = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_');
    let templated = format!("${name}");
    let base = scope.start_byte();
    for (i, _) in text[scope.byte_range()].match_indices(&templated) {
        let at = base + i + 1;
        if !is_word(text[at + name.len()..].chars().next()) && !captured.contains(&at) {
            return true;
        }
    }
    if !scope.has_error() {
        return false;
    }
    let mut stack = vec![scope];
    let mut cursor = scope.walk();
    while let Some(n) = stack.pop() {
        if n.is_error() {
            let base = n.start_byte();
            let s = &text[n.byte_range()];
            for (i, _) in s.match_indices(name) {
                let at = base + i;
                let before = text[..at].chars().next_back();
                let after = text[at + name.len()..].chars().next();
                if !is_word(before) && !is_word(after) && !captured.contains(&at) {
                    return true;
                }
            }
        } else if n.has_error() {
            stack.extend(n.children(&mut cursor));
        }
    }
    false
}

/// Where a declaration becomes visible: after the statement that declares it
/// (`let x = x + 1` reads the previous `x`), or at the name for parameters
/// and other declarations that are not inside a statement of their scope.
fn visible_from(kinds: &Kinds, def: Node, owner: Node) -> usize {
    let mut cur = def;
    while let Some(parent) = cur.parent() {
        if parent == owner {
            break;
        }
        if kinds.is_block(&parent) && cur.is_named() {
            return cur.end_byte();
        }
        cur = parent;
    }
    if cur.parent() == Some(owner) && kinds.is_block(&owner) && cur != def {
        return cur.end_byte();
    }
    def.start_byte()
}
