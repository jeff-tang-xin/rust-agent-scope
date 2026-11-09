//! IPC types of the structure service (UTF-16 columns, 0-based lines).

use ide_core::Range;
use serde::{Deserialize, Serialize};
use specta::Type;

/// One tree-sitter node.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct SyntaxNode {
    /// Grammar node kind, e.g. `method_invocation`, `identifier`, `;`.
    pub kind: String,
    pub range: Range,
    /// Field name of this node in its parent (`name`, `body`, `condition`…).
    pub field: Option<String>,
    /// Named node (anonymous tokens such as `;` or `(` are not named).
    pub named: bool,
    /// The node is an `ERROR` node.
    pub error: bool,
    /// The parser inserted the node to recover (e.g. a missing `;`); its range is empty.
    pub missing: bool,
    /// The node or a descendant is an error / missing node.
    pub has_error: bool,
}

/// Result of `node_at`: the smallest named node covering a range, with its ancestors.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct NodeAt {
    pub node: SyntaxNode,
    /// Parent first, the root last.
    pub ancestors: Vec<SyntaxNode>,
}

/// Result of `expression_at` (Extract Variable / Introduce … fallback).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ExpressionAt {
    /// The expression exactly matching the (whitespace-trimmed) selection, else
    /// the smallest expression enclosing it; for an empty selection the
    /// innermost expression at the caret. `None` when the selection is not
    /// inside an expression.
    pub expression: Option<SyntaxNode>,
    /// `expression` spans exactly the trimmed selection.
    pub exact: bool,
    /// Expressions from `expression` outwards up to the statement, innermost
    /// first (IDEA's "choose expression" popup for a caret selection).
    pub candidates: Vec<SyntaxNode>,
    /// The statement (direct child of a block / statement list) containing the expression.
    pub statement: Option<SyntaxNode>,
    /// The block / statement list that contains `statement`.
    pub block: Option<SyntaxNode>,
    /// Innermost lambda / nested function between the expression and the
    /// statement: inserting a declaration before `statement` would move the
    /// expression out of that function, so callers must not do it blindly.
    pub function_boundary: Option<SyntaxNode>,
    /// The statement (or the expression when there is none) contains syntax errors.
    pub has_error: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum SyntaxErrorKind {
    /// Text the parser could not fit into the grammar (`ERROR` node).
    Unexpected,
    /// A token / node the parser had to assume (e.g. a missing `;`); empty range.
    Missing,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct SyntaxError {
    pub kind: SyntaxErrorKind,
    pub range: Range,
    /// For `missing`: the expected node kind or token text, e.g. `;`, `)`, `identifier`.
    pub expected: Option<String>,
    /// Short human message: ``missing `;` ``, ``unexpected `foo` ``.
    pub message: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct SyntaxErrors {
    /// In document order.
    pub errors: Vec<SyntaxError>,
    /// More errors exist than were returned (see `MAX_SYNTAX_ERRORS`).
    pub truncated: bool,
}

/// What a local definition declares (nvim-treesitter `@local.definition.<kind>`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum LocalKind {
    Var,
    Parameter,
    Function,
    Type,
    Import,
    Macro,
    Label,
    /// Plain `@local.definition` or a kind without a better mapping.
    Other,
}

/// A variable / parameter / nested function defined in a function-level scope.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct LocalSymbol {
    pub name: String,
    pub kind: LocalKind,
    /// The declaring identifier.
    pub definition: Range,
    /// The scope the definition belongs to (block, function, lambda, loop…).
    pub scope: Range,
    /// Every other occurrence that resolves to this symbol (reads, writes and,
    /// in languages where assignment declares — Python, Ruby, PHP, Bash —
    /// re-assignments), document order, excluding `definition`.
    pub references: Vec<Range>,
    /// The resolution is unambiguous. `false` when the scope declares the name
    /// more than once in a language where that is not plain shadowing (JS `var`
    /// re-declaration, broken Java…): references were split by position and
    /// callers should confirm before renaming.
    pub certain: bool,
    /// The other declarations of the same name in `scope` (empty when `certain`).
    pub conflicts: Vec<Range>,
}

/// Why an identifier is not treated as a local.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum NotLocalReason {
    /// Member position (`obj.name`, `name:` labels, `foo()` method names in Java…).
    Member,
    /// Declared at file / class / module level (fields, methods, top-level
    /// functions, imports): visible beyond the local scope.
    TopLevel,
    /// No definition in an enclosing local scope: global, import, inherited
    /// member, or declared in a way the query does not cover.
    Unresolved,
}

/// Result of `locals(pos)`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(tag = "status", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum Locals {
    /// No identifier at the position.
    NoIdentifier,
    NotLocal {
        name: String,
        range: Range,
        reason: NotLocalReason,
    },
    Local {
        symbol: LocalSymbol,
    },
}

/// An identifier-like token whose text equals the searched name.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct IdentifierOccurrence {
    pub range: Range,
    /// Node kind, e.g. `identifier`, `type_identifier`, `property_identifier`.
    pub kind: String,
    /// Member / label position (`obj.name`, `name: value` labels): refers to
    /// a member, not to a variable in scope.
    pub qualified: bool,
}

/// Structural ranges around a position (selection, move / complete statement).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct StructuralRanges {
    /// Extend Selection chain, innermost first (shrink = walk it backwards).
    pub selection: Vec<Range>,
    /// Innermost statement (child of a block / statement list) at the position.
    pub statement: Option<SyntaxNode>,
    pub prev_statement: Option<SyntaxNode>,
    pub next_statement: Option<SyntaxNode>,
    /// The block / statement list containing `statement`.
    pub block: Option<SyntaxNode>,
    /// Innermost declaration that is a direct member of a class body or of the file.
    pub declaration: Option<SyntaxNode>,
    pub prev_declaration: Option<SyntaxNode>,
    pub next_declaration: Option<SyntaxNode>,
}

/// What the structure service can do for a language.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct StructureSupport {
    /// A tree-sitter grammar is bundled: every query but `locals` works.
    pub tree: bool,
    /// A `locals.scm` query is bundled.
    pub locals: bool,
}

/// Identifier occurrences in one file (`identifiers_in_files`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct FileIdentifiers {
    pub path: ide_core::IpcPath,
    pub occurrences: Vec<IdentifierOccurrence>,
}
