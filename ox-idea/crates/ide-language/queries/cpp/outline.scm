(namespace_definition name: (_) @name) @def.namespace
(class_specifier name: (_) @name body: (field_declaration_list)) @def.class
(struct_specifier name: (_) @name body: (field_declaration_list)) @def.struct
(union_specifier name: (_) @name body: (field_declaration_list)) @def.struct
(enum_specifier name: (_) @name body: (enumerator_list)) @def.enum
(enumerator name: (identifier) @name) @def.enum_member
(concept_definition name: (identifier) @name) @def.interface
(alias_declaration name: (type_identifier) @name) @def.type
(type_definition declarator: (type_identifier) @name) @def.type

(function_definition
  declarator: (function_declarator declarator: (destructor_name) @name parameters: (parameter_list) @detail)) @def.method
(function_definition
  declarator: [
    (function_declarator declarator: [(identifier) (field_identifier) (qualified_identifier) (operator_name)] @name parameters: (parameter_list) @detail)
    (pointer_declarator declarator: (function_declarator declarator: [(identifier) (field_identifier) (qualified_identifier)] @name parameters: (parameter_list) @detail))
    (reference_declarator (function_declarator declarator: [(identifier) (field_identifier) (qualified_identifier) (operator_name)] @name parameters: (parameter_list) @detail))
  ]) @def.function
(field_declaration_list (declaration
  declarator: (function_declarator declarator: [(identifier) (field_identifier) (destructor_name) (operator_name)] @name parameters: (parameter_list) @detail)) @def.method)
(field_declaration_list (field_declaration
  declarator: (function_declarator declarator: [(field_identifier) (destructor_name) (operator_name)] @name parameters: (parameter_list) @detail)) @def.method)
(field_declaration_list (field_declaration declarator: [
  (field_identifier) @name
  (pointer_declarator declarator: (field_identifier) @name)
  (array_declarator declarator: (field_identifier) @name)
  (reference_declarator (field_identifier) @name)
]) @def.field)
(translation_unit (declaration declarator: (function_declarator declarator: [(identifier) (qualified_identifier)] @name parameters: (parameter_list) @detail)) @def.function)
(namespace_definition body: (declaration_list (declaration declarator: (function_declarator declarator: (identifier) @name parameters: (parameter_list) @detail)) @def.function))

(preproc_def name: (identifier) @name) @def.macro
(preproc_function_def name: (identifier) @name parameters: (preproc_params) @detail) @def.macro
