; Adapted from nvim-treesitter queries/swift/locals.scm (Apache-2.0): adds
; parameter / binding definitions and references for tree-sitter-swift 0.7.

(import_declaration
  (identifier) @local.definition.import)

(function_declaration
  name: (simple_identifier) @local.definition.function
  (#set! definition.function.scope "parent"))

(class_body
  (function_declaration
    name: (simple_identifier) @local.definition.method))

(class_body
  (property_declaration
    name: (pattern
      bound_identifier: (simple_identifier) @local.definition.field)))

(parameter
  name: (simple_identifier) @local.definition.parameter)

(lambda_parameter
  name: (simple_identifier) @local.definition.parameter)

(property_declaration
  name: (pattern
    bound_identifier: (simple_identifier) @local.definition.var))

(for_statement
  item: (pattern
    bound_identifier: (simple_identifier) @local.definition.var))

(if_statement
  (value_binding_pattern)
  (pattern
    bound_identifier: (simple_identifier) @local.definition.var))

(guard_statement
  (value_binding_pattern)
  (pattern
    bound_identifier: (simple_identifier) @local.definition.var))

(catch_block
  (pattern
    bound_identifier: (simple_identifier) @local.definition.var))

; References
(simple_identifier) @local.reference

; Scopes
[
  (statements)
  (for_statement)
  (while_statement)
  (repeat_while_statement)
  (do_statement)
  (if_statement)
  (guard_statement)
  (switch_statement)
  (switch_entry)
  (catch_block)
  (lambda_literal)
  (function_declaration)
  (init_declaration)
  (computed_property)
  (class_declaration)
  (protocol_declaration)
] @local.scope
