(object_type_definition (name) @name) @def.class
(object_type_extension (name) @name) @outline.extension
(interface_type_definition (name) @name) @def.interface
(enum_type_definition (name) @name) @def.enum
(enum_value_definition (enum_value (name) @name)) @def.enum_member
(input_object_type_definition (name) @name) @def.struct
(union_type_definition (name) @name) @def.type
(scalar_type_definition (name) @name) @def.type
(directive_definition (name) @name) @def.function
(field_definition (name) @name) @def.field
(input_fields_definition (input_value_definition (name) @name) @def.field)
(operation_definition (name) @name) @def.function
(fragment_definition (fragment_name (name) @name)) @def.function
