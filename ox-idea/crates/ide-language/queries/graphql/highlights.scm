; GraphQL highlighting (the grammar crate ships no query). Later patterns win.

(comment) @comment
(description (string_value) @comment.doc)
(string_value) @string
(int_value) @number
(float_value) @number
[(boolean_value) (null_value)] @keyword
(enum_value (name) @constant)

(variable) @variable
(argument (name) @variable.parameter)
(input_value_definition (name) @variable.parameter)
(object_field (name) @property)

(field (name) @property)
(alias (name) @property)
(field_definition (name) @property)
(enum_value_definition (enum_value (name) @constant))

(named_type (name) @type)
(object_type_definition (name) @type)
(interface_type_definition (name) @type)
(union_type_definition (name) @type)
(enum_type_definition (name) @type)
(input_object_type_definition (name) @type)
(scalar_type_definition (name) @type)
(object_type_extension (name) @type)
(interface_type_extension (name) @type)
(union_type_extension (name) @type)
(enum_type_extension (name) @type)
(input_object_type_extension (name) @type)
(scalar_type_extension (name) @type)

(operation_definition (name) @function)
(fragment_name (name) @function)
(directive_definition (name) @attribute)
(directive (name) @attribute)
"@" @attribute
(directive_location) @constant

(operation_type) @keyword
[
  "query" "mutation" "subscription" "fragment" "on" "type" "interface" "union"
  "enum" "input" "scalar" "schema" "extend" "implements" "directive" "repeatable"
] @keyword

["(" ")" "[" "]" "{" "}"] @punctuation.bracket
[":" "=" "|" "&" "!" "..."] @punctuation.delimiter
