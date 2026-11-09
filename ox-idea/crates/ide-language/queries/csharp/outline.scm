(namespace_declaration name: (_) @name) @def.namespace
(file_scoped_namespace_declaration name: (_) @name) @def.namespace
(class_declaration name: (identifier) @name) @def.class
(record_declaration name: (identifier) @name) @def.class
(struct_declaration name: (identifier) @name) @def.struct
(interface_declaration name: (identifier) @name) @def.interface
(enum_declaration name: (identifier) @name) @def.enum
(enum_member_declaration name: (identifier) @name) @def.enum_member
(delegate_declaration name: (identifier) @name) @def.type
(method_declaration name: (identifier) @name parameters: (parameter_list) @detail) @def.method
(local_function_statement name: (identifier) @name parameters: (parameter_list) @detail) @def.function
(constructor_declaration name: (identifier) @name parameters: (parameter_list) @detail) @def.constructor
(destructor_declaration name: (identifier) @name) @def.method
(operator_declaration) @def.operator
(property_declaration name: (identifier) @name) @def.property
(indexer_declaration "this" @name) @def.property
(event_field_declaration (variable_declaration (variable_declarator name: (identifier) @name))) @def.event
(event_declaration name: (identifier) @name) @def.event
(field_declaration (modifier) @_m (variable_declaration (variable_declarator name: (identifier) @name)) (#eq? @_m "const")) @def.constant
(field_declaration (variable_declaration (variable_declarator name: (identifier) @name))) @def.field
