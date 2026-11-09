(function_definition
  declarator: [
    (function_declarator declarator: (identifier) @name parameters: (parameter_list) @detail)
    (pointer_declarator declarator: (function_declarator declarator: (identifier) @name parameters: (parameter_list) @detail))
    (pointer_declarator declarator: (pointer_declarator declarator: (function_declarator declarator: (identifier) @name parameters: (parameter_list) @detail)))
  ]) @def.function
(translation_unit (declaration
  declarator: [
    (function_declarator declarator: (identifier) @name parameters: (parameter_list) @detail)
    (pointer_declarator declarator: (function_declarator declarator: (identifier) @name parameters: (parameter_list) @detail))
  ]) @def.function)

(struct_specifier name: (type_identifier) @name body: (field_declaration_list)) @def.struct
(union_specifier name: (type_identifier) @name body: (field_declaration_list)) @def.struct
(enum_specifier name: (type_identifier) @name body: (enumerator_list)) @def.enum
(enumerator name: (identifier) @name) @def.enum_member
(field_declaration_list (field_declaration declarator: [
  (field_identifier) @name
  (pointer_declarator declarator: (field_identifier) @name)
  (array_declarator declarator: (field_identifier) @name)
]) @def.field)
(type_definition declarator: (type_identifier) @name) @def.type
(preproc_def name: (identifier) @name) @def.macro
(preproc_function_def name: (identifier) @name parameters: (preproc_params) @detail) @def.macro
(translation_unit (declaration declarator: [
  (identifier) @name
  (init_declarator declarator: (identifier) @name)
  (init_declarator declarator: (pointer_declarator declarator: (identifier) @name))
]) @def.variable)
