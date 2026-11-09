(variable_declaration (identifier) @name [(struct_declaration) (opaque_declaration)]) @def.struct
(variable_declaration (identifier) @name (union_declaration)) @def.struct
(variable_declaration (identifier) @name (enum_declaration)) @def.enum
(variable_declaration (identifier) @name (error_set_declaration)) @def.enum
(enum_declaration (container_field name: (identifier) @name) @def.enum_member)
(container_field name: (identifier) @name) @def.field
(function_declaration name: (identifier) @name (parameters) @detail) @def.function
(test_declaration [(string) (identifier)] @name) @def.function
(source_file (variable_declaration "const" (identifier) @name) @def.constant)
(source_file (variable_declaration (identifier) @name) @def.variable)
(struct_declaration (variable_declaration "const" (identifier) @name) @def.constant)
