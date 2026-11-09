(class_declaration (modifiers (class_modifier) @_m) name: (identifier) @name (#eq? @_m "enum")) @def.enum
(class_declaration "interface" name: (identifier) @name) @def.interface
(class_declaration name: (identifier) @name) @def.class
(object_declaration name: (identifier) @name) @def.class
(companion_object name: (identifier) @name) @def.class
(companion_object "companion" @name) @def.class
(enum_entry (identifier) @name) @def.enum_member
(type_alias type: (identifier) @name) @def.type
(secondary_constructor "constructor" @name) @def.constructor
(function_declaration name: (identifier) @name (function_value_parameters) @detail) @def.function
(class_body (property_declaration (variable_declaration (identifier) @name)) @def.property)
(source_file (property_declaration (variable_declaration (identifier) @name)) @def.property)
