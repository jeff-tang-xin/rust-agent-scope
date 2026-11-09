(package_clause (package_identifier) @name) @outline.package

(type_spec name: (type_identifier) @name type: (struct_type)) @def.struct
(type_spec name: (type_identifier) @name type: (interface_type)) @def.interface
(type_spec name: (type_identifier) @name) @def.type
(type_alias name: (type_identifier) @name) @def.type

(field_declaration_list (field_declaration name: (field_identifier) @name) @def.field)
(interface_type (method_elem name: (field_identifier) @name parameters: (parameter_list) @detail) @def.method)

(function_declaration name: (identifier) @name parameters: (parameter_list) @detail) @def.function
(method_declaration
  receiver: (parameter_list (parameter_declaration type: [
    (type_identifier) @container
    (pointer_type (type_identifier) @container)
    (generic_type type: (type_identifier) @container)
    (pointer_type (generic_type type: (type_identifier) @container))
  ]))
  name: (field_identifier) @name
  parameters: (parameter_list) @detail) @def.method

(source_file (const_declaration (const_spec name: (identifier) @name) @def.constant))
(source_file (var_declaration (var_spec name: (identifier) @name) @def.variable))
(source_file (var_declaration (var_spec_list (var_spec name: (identifier) @name) @def.variable)))
