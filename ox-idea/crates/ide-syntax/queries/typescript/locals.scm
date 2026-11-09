; From nvim-treesitter queries/ecma + queries/typescript locals.scm (Apache-2.0),
; flattened; members are `.field` / `.method`; adds loop, catch, class and
; destructuring definitions.

; Scopes
;-------
(statement_block) @local.scope

(class_body) @local.scope

(function_expression) @local.scope

(arrow_function) @local.scope

(function_declaration) @local.scope

(method_definition) @local.scope

(for_statement) @local.scope

(for_in_statement) @local.scope

(catch_clause) @local.scope

; Definitions
;------------
(variable_declarator
  name: (identifier) @local.definition.var)

(variable_declarator
  name: (object_pattern
    (shorthand_property_identifier_pattern) @local.definition.var))

(variable_declarator
  (object_pattern
    (pair_pattern
      (identifier) @local.definition.var)))

(import_specifier
  (identifier) @local.definition.import)

(namespace_import
  (identifier) @local.definition.import)

(function_declaration
  (identifier) @local.definition.function
  (#set! definition.function.scope parent))

(generator_function_declaration
  name: (identifier) @local.definition.function
  (#set! definition.function.scope parent))

(class_declaration
  name: (_) @local.definition.type
  (#set! definition.type.scope parent))

(variable_declarator
  name: (array_pattern
    (identifier) @local.definition.var))

(for_in_statement
  left: (identifier) @local.definition.var)

(for_in_statement
  left: (object_pattern
    (shorthand_property_identifier_pattern) @local.definition.var))

(for_in_statement
  left: (array_pattern
    (identifier) @local.definition.var))

(catch_clause
  parameter: (identifier) @local.definition.var)

(method_definition
  (property_identifier) @local.definition.method)

; References
;------------
(identifier) @local.reference

(shorthand_property_identifier) @local.reference

(required_parameter
  (identifier) @local.definition)

(optional_parameter
  (identifier) @local.definition)

; x => x
(arrow_function
  parameter: (identifier) @local.definition.parameter)

; ({ a }) => null
(required_parameter
  (object_pattern
    (shorthand_property_identifier_pattern) @local.definition.parameter))

; ({ a: b }) => null
(required_parameter
  (object_pattern
    (pair_pattern
      value: (identifier) @local.definition.parameter)))

; ([ a ]) => null
(required_parameter
  (array_pattern
    (identifier) @local.definition.parameter))

(required_parameter
  (rest_pattern
    (identifier) @local.definition.parameter))
