(package_clause name: (package_identifier) @name) @outline.package
(class_definition name: (identifier) @name) @def.class
(object_definition name: (identifier) @name) @def.class
(trait_definition name: (identifier) @name) @def.trait
(enum_definition name: (identifier) @name) @def.enum
(simple_enum_case name: (identifier) @name) @def.enum_member
(full_enum_case name: (identifier) @name) @def.enum_member
(type_definition name: (type_identifier) @name) @def.type
(function_definition name: (identifier) @name parameters: (parameters) @detail) @def.function
(function_definition name: (identifier) @name) @def.function
(function_declaration name: (identifier) @name parameters: (parameters) @detail) @def.function
(function_declaration name: (identifier) @name) @def.function
(given_definition name: (identifier) @name) @def.variable
(template_body (val_definition pattern: (identifier) @name) @def.field)
(template_body (var_definition pattern: (identifier) @name) @def.field)
(template_body (val_declaration name: (identifier) @name) @def.field)
(compilation_unit (val_definition pattern: (identifier) @name) @def.constant)
(compilation_unit (var_definition pattern: (identifier) @name) @def.variable)
