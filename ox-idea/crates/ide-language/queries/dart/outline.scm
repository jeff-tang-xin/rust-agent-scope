(class_declaration name: (identifier) @name) @def.class
(mixin_declaration name: (identifier) @name) @def.class
(extension_declaration name: (identifier) @name) @outline.extension
(extension_type_declaration name: (identifier) @name) @def.class
(enum_declaration name: (identifier) @name) @def.enum
(enum_constant name: (identifier) @name) @def.enum_member
(type_alias (type_identifier) @name) @def.type
(class_member (declaration (constructor_signature name: (identifier) @name "." name: (identifier) @name parameters: (formal_parameter_list) @detail))) @def.constructor
(class_member (declaration (constructor_signature name: (identifier) @name parameters: (formal_parameter_list) @detail))) @def.constructor
(class_member (declaration (factory_constructor_signature (identifier) @name))) @def.constructor
(class_member (declaration (constant_constructor_signature (identifier) @name))) @def.constructor
(class_member (method_declaration signature: (method_signature (function_signature name: (identifier) @name parameters: (formal_parameter_list) @detail)))) @def.method
(class_member (method_declaration signature: (method_signature (getter_signature name: (identifier) @name)))) @def.property
(class_member (method_declaration signature: (method_signature (setter_signature name: (identifier) @name)))) @def.property
(class_member (declaration (function_signature name: (identifier) @name parameters: (formal_parameter_list) @detail))) @def.method
(class_member (declaration (initialized_identifier_list (initialized_identifier name: (identifier) @name)))) @def.field
(class_member (declaration (static_final_declaration_list (static_final_declaration name: (identifier) @name)))) @def.constant
(function_declaration signature: (function_signature name: (identifier) @name parameters: (formal_parameter_list) @detail)) @def.function
(top_level_variable_declaration (static_final_declaration_list (static_final_declaration name: (identifier) @name))) @def.constant
(top_level_variable_declaration (initialized_identifier_list (initialized_identifier name: (identifier) @name))) @def.variable
