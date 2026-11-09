(mod_item name: (identifier) @name) @def.module
(struct_item name: (type_identifier) @name) @def.struct
(union_item name: (type_identifier) @name) @def.struct
(enum_item name: (type_identifier) @name) @def.enum
(enum_variant name: (identifier) @name) @def.enum_member
(trait_item name: (type_identifier) @name) @def.trait
(type_item name: (type_identifier) @name) @def.type
(associated_type name: (type_identifier) @name) @def.type

(impl_item
  trait: (_) @trait
  type: (_) @name) @def.impl
(impl_item
  !trait
  type: (_) @name) @def.impl

(function_item
  name: (identifier) @name
  parameters: (parameters) @detail) @def.function
(function_signature_item
  name: (identifier) @name
  parameters: (parameters) @detail) @def.function

(field_declaration_list
  (field_declaration name: (field_identifier) @name) @def.field)

(const_item name: (identifier) @name) @def.constant
(static_item name: (identifier) @name) @def.constant
(macro_definition name: (identifier) @name) @def.macro
