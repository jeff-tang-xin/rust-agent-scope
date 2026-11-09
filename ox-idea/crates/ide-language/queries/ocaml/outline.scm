(module_definition (module_binding (module_name) @name)) @def.module
(module_type_definition (module_type_name) @name) @def.interface
(type_definition (type_binding name: (type_constructor) @name body: (record_declaration))) @def.struct
(type_definition (type_binding name: (type_constructor) @name body: (variant_declaration))) @def.enum
(type_definition (type_binding name: (type_constructor) @name)) @def.type
(constructor_declaration (constructor_name) @name) @def.enum_member
(field_declaration (field_name) @name) @def.field
(value_definition (let_binding pattern: (value_name) @name (parameter))) @def.function
(value_definition (let_binding pattern: (value_name) @name body: (fun_expression))) @def.function
(value_definition (let_binding pattern: (value_name) @name body: (function_expression))) @def.function
(value_definition (let_binding pattern: (value_name) @name)) @def.variable
(value_specification (value_name) @name) @def.function
(external (value_name) @name) @def.function
(class_definition (class_binding (class_name) @name)) @def.class
(method_definition (method_name) @name) @def.method
(exception_definition (constructor_declaration (constructor_name) @name)) @def.class
