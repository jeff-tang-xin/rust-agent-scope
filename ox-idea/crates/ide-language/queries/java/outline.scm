(class_declaration name: (identifier) @name) @def.class
(record_declaration name: (identifier) @name) @def.class
(interface_declaration name: (identifier) @name) @def.interface
(annotation_type_declaration name: (identifier) @name) @def.interface
(enum_declaration name: (identifier) @name) @def.enum
(enum_constant name: (identifier) @name) @def.enum_member
(method_declaration name: (identifier) @name parameters: (formal_parameters) @detail) @def.method
(constructor_declaration name: (identifier) @name parameters: (formal_parameters) @detail) @def.constructor
(compact_constructor_declaration name: (identifier) @name) @def.constructor
(annotation_type_element_declaration name: (identifier) @name) @def.method
(field_declaration declarator: (variable_declarator name: (identifier) @name)) @def.field
(constant_declaration declarator: (variable_declarator name: (identifier) @name)) @def.constant
