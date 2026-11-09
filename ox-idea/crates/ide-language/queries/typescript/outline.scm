(class_declaration name: (type_identifier) @name) @def.class
(abstract_class_declaration name: (type_identifier) @name) @def.class
(class name: (type_identifier) @name) @def.class
(interface_declaration name: (type_identifier) @name) @def.interface
(enum_declaration name: (identifier) @name) @def.enum
(enum_body (property_identifier) @name @def.enum_member)
(enum_body (enum_assignment name: (property_identifier) @name) @def.enum_member)
(type_alias_declaration name: (type_identifier) @name) @def.type
(internal_module name: (_) @name) @def.namespace
(module name: (_) @name) @def.module

(function_declaration name: (identifier) @name parameters: (formal_parameters) @detail) @def.function
(generator_function_declaration name: (identifier) @name parameters: (formal_parameters) @detail) @def.function
(function_signature name: (identifier) @name parameters: (formal_parameters) @detail) @def.function

(method_definition name: (property_identifier) @name (#eq? @name "constructor") parameters: (formal_parameters) @detail) @def.constructor
(method_definition name: (_) @name parameters: (formal_parameters) @detail) @def.method
(method_signature name: (_) @name parameters: (formal_parameters) @detail) @def.method
(abstract_method_signature name: (_) @name parameters: (formal_parameters) @detail) @def.method
(public_field_definition name: (_) @name value: [(arrow_function) (function_expression)]) @def.method
(public_field_definition name: (_) @name) @def.field
(property_signature name: (_) @name) @def.property

(program (lexical_declaration (variable_declarator name: (identifier) @name value: [(arrow_function) (function_expression)]) @def.function))
(program (export_statement (lexical_declaration (variable_declarator name: (identifier) @name value: [(arrow_function) (function_expression)]) @def.function)))
(program (lexical_declaration "const" (variable_declarator name: (identifier) @name) @def.constant))
(program (export_statement (lexical_declaration "const" (variable_declarator name: (identifier) @name) @def.constant)))
(program (lexical_declaration (variable_declarator name: (identifier) @name) @def.variable))
(program (export_statement (lexical_declaration (variable_declarator name: (identifier) @name) @def.variable)))
(program (variable_declaration (variable_declarator name: (identifier) @name) @def.variable))
(program (export_statement (variable_declaration (variable_declarator name: (identifier) @name) @def.variable)))
(program (ambient_declaration (variable_declaration (variable_declarator name: (identifier) @name) @def.variable)))
