(namespace_definition name: (namespace_name) @name) @def.namespace
(class_declaration name: (name) @name) @def.class
(interface_declaration name: (name) @name) @def.interface
(trait_declaration name: (name) @name) @def.trait
(enum_declaration name: (name) @name) @def.enum
(enum_case name: (name) @name) @def.enum_member
(method_declaration name: (name) @name parameters: (formal_parameters) @detail) @def.method
(function_definition name: (name) @name parameters: (formal_parameters) @detail) @def.function
(property_declaration (property_element name: (variable_name) @name)) @def.property
(property_promotion_parameter name: (variable_name) @name) @def.property
(const_declaration (const_element (name) @name)) @def.constant
