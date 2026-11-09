; Adapted from nvim-treesitter queries/c_sharp/locals.scm (Apache-2.0): adds
; function / lambda / loop / catch scopes, lambda parameters and member kinds.

; Definitions
(field_declaration
  (variable_declaration
    (variable_declarator
      .
      (identifier) @local.definition.field)))

(variable_declarator
  .
  (identifier) @local.definition.var)

(variable_declarator
  (tuple_pattern
    (identifier) @local.definition.var))

(declaration_expression
  name: (identifier) @local.definition.var)

(foreach_statement
  left: (identifier) @local.definition.var)

(foreach_statement
  left: (tuple_pattern
    (identifier) @local.definition.var))

(catch_declaration
  name: (identifier) @local.definition.var)

(parameter
  name: (identifier) @local.definition.parameter)

(lambda_expression
  parameters: (implicit_parameter) @local.definition.parameter)

(method_declaration
  name: (identifier) @local.definition.method)

(local_function_statement
  name: (identifier) @local.definition.function
  (#set! definition.function.scope "parent"))

(property_declaration
  name: (identifier) @local.definition.field)

(type_parameter
  (identifier) @local.definition.type)

(class_declaration
  name: (identifier) @local.definition.type
  (#set! definition.type.scope "parent"))

; References
(identifier) @local.reference

(implicit_parameter) @local.reference

; Scope
[
  (block)
  (method_declaration)
  (constructor_declaration)
  (local_function_statement)
  (lambda_expression)
  (anonymous_method_expression)
  (for_statement)
  (foreach_statement)
  (using_statement)
  (catch_clause)
  (switch_section)
] @local.scope
