; Adapted from nvim-treesitter queries/dart/locals.scm (Apache-2.0) for
; tree-sitter-dart 0.2 (`class_declaration`, `function_expression`, patterns).

; Definitions
(class_body
  (class_member
    (method_declaration
      (method_signature
        (function_signature
          name: (identifier) @local.definition.method)))))

(class_body
  (class_member
    (declaration
      (initialized_identifier_list
        (initialized_identifier
          name: (identifier) @local.definition.field)))))

(function_signature
  name: (identifier) @local.definition.function
  (#set! definition.function.scope "parent"))

(formal_parameter
  name: (identifier) @local.definition.parameter)

(constructor_param
  (identifier) @local.definition.parameter)

(catch_clause
  exception: (identifier) @local.definition.var)

(catch_clause
  stack_trace: (identifier) @local.definition.var)

(for_statement
  name: (identifier) @local.definition.var)

(initialized_variable_definition
  name: (identifier) @local.definition.var)

(initialized_identifier
  (identifier) @local.definition.var)

(static_final_declaration
  (identifier) @local.definition.var)

(variable_pattern
  name: (identifier) @local.definition.var)

(class_declaration
  name: (identifier) @local.definition.type
  (#set! definition.type.scope "parent"))

(enum_declaration
  name: (identifier) @local.definition.type
  (#set! definition.type.scope "parent"))

(import_specification
  alias: (identifier) @local.definition.import)

; References
(identifier) @local.reference

(identifier_dollar_escaped) @local.reference

; Scopes
(class_declaration
  body: (_) @local.scope)

[
  (function_body)
  (function_expression)
  (method_declaration)
  (block)
  (if_statement)
  (for_statement)
  (while_statement)
  (try_statement)
  (catch_clause)
  (finally_clause)
  (switch_statement_case)
] @local.scope
