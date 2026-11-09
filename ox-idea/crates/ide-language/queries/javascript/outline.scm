(class_declaration name: (identifier) @name) @def.class
(class name: (identifier) @name) @def.class

(function_declaration name: (identifier) @name parameters: (formal_parameters) @detail) @def.function
(generator_function_declaration name: (identifier) @name parameters: (formal_parameters) @detail) @def.function

(method_definition name: (property_identifier) @name (#eq? @name "constructor") parameters: (formal_parameters) @detail) @def.constructor
(method_definition name: (_) @name parameters: (formal_parameters) @detail) @def.method
(field_definition property: (_) @name value: [(arrow_function) (function_expression)]) @def.method
(field_definition property: (_) @name) @def.field

(program (lexical_declaration (variable_declarator name: (identifier) @name value: [(arrow_function) (function_expression)]) @def.function))
(program (export_statement (lexical_declaration (variable_declarator name: (identifier) @name value: [(arrow_function) (function_expression)]) @def.function)))
(program (lexical_declaration "const" (variable_declarator name: (identifier) @name) @def.constant))
(program (export_statement (lexical_declaration "const" (variable_declarator name: (identifier) @name) @def.constant)))
(program (lexical_declaration (variable_declarator name: (identifier) @name) @def.variable))
(program (export_statement (lexical_declaration (variable_declarator name: (identifier) @name) @def.variable)))
(program (variable_declaration (variable_declarator name: (identifier) @name) @def.variable))

(program (expression_statement (assignment_expression
  left: (member_expression property: (property_identifier) @name)
  right: [(arrow_function) (function_expression)]) @def.function))
