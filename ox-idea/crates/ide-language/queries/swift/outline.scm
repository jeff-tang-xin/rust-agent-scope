(class_declaration "extension" name: (_) @name) @outline.extension
(class_declaration "struct" name: (type_identifier) @name) @def.struct
(class_declaration "enum" name: (type_identifier) @name) @def.enum
(class_declaration "actor" name: (type_identifier) @name) @def.class
(class_declaration name: (type_identifier) @name) @def.class
(protocol_declaration name: (type_identifier) @name) @def.interface
(enum_entry name: (simple_identifier) @name) @def.enum_member
(typealias_declaration name: (type_identifier) @name) @def.type
(associatedtype_declaration name: (type_identifier) @name) @def.type
(init_declaration "init" @name) @def.constructor
(deinit_declaration "deinit" @name) @def.method
(subscript_declaration "subscript" @name) @def.method
(function_declaration name: (simple_identifier) @name) @def.function
(protocol_function_declaration name: (simple_identifier) @name) @def.method
(protocol_property_declaration name: (pattern bound_identifier: (simple_identifier) @name)) @def.property
(class_body (property_declaration name: (pattern bound_identifier: (simple_identifier) @name)) @def.property)
(source_file (property_declaration (value_binding_pattern "let") name: (pattern bound_identifier: (simple_identifier) @name)) @def.constant)
(source_file (property_declaration name: (pattern bound_identifier: (simple_identifier) @name)) @def.variable)
