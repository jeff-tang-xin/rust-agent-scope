; Adapted from nvim-treesitter queries/kotlin/locals.scm (Apache-2.0) for
; tree-sitter-kotlin-ng (`identifier` instead of `simple_identifier`).

; Imports
(package_header
  (qualified_identifier) @local.definition.namespace)

(import
  (qualified_identifier
    (identifier) @local.definition.import .)
  .)

(import
  (identifier) @local.definition.import .)

; Functions
(function_declaration
  name: (identifier) @local.definition.function
  (#set! definition.function.scope "parent"))

(class_body
  (function_declaration
    name: (identifier) @local.definition.method)
  (#set! definition.method.scope "parent"))

; Variables
(function_declaration
  (function_value_parameters
    (parameter
      (identifier) @local.definition.parameter)))

(secondary_constructor
  (function_value_parameters
    (parameter
      (identifier) @local.definition.parameter)))

(lambda_literal
  (lambda_parameters
    (variable_declaration
      (identifier) @local.definition.parameter)))

(lambda_literal
  (lambda_parameters
    (multi_variable_declaration
      (variable_declaration
        (identifier) @local.definition.parameter))))

(class_body
  (property_declaration
    (variable_declaration
      (identifier) @local.definition.field)))

(class_parameter
  (identifier) @local.definition.field)

(enum_class_body
  (enum_entry
    (identifier) @local.definition.field))

(catch_block
  (identifier) @local.definition.var)

(variable_declaration
  (identifier) @local.definition.var)

; Types
(class_declaration
  name: (identifier) @local.definition.type
  (#set! definition.type.scope "parent"))

(object_declaration
  name: (identifier) @local.definition.type
  (#set! definition.type.scope "parent"))

(type_alias
  (identifier) @local.definition.type
  (#set! definition.type.scope "parent"))

; Scopes
[
  (block)
  (if_expression)
  (when_expression)
  (when_entry)
  (for_statement)
  (while_statement)
  (do_while_statement)
  (lambda_literal)
  (function_declaration)
  (anonymous_function)
  (primary_constructor)
  (secondary_constructor)
  (anonymous_initializer)
  (catch_block)
  (class_declaration)
  (object_declaration)
  (object_literal)
  (enum_class_body)
  (enum_entry)
] @local.scope

; References
(identifier) @local.reference
