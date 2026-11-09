(function_declaration
  name: (method_index_expression table: (_) @container method: (identifier) @name)
  parameters: (parameters) @detail) @def.method
(function_declaration
  name: (dot_index_expression table: (_) @container field: (identifier) @name)
  parameters: (parameters) @detail) @def.function
(function_declaration name: (identifier) @name parameters: (parameters) @detail) @def.function
(chunk (assignment_statement
  (variable_list name: (dot_index_expression table: (_) @container field: (identifier) @name))
  (expression_list value: (function_definition parameters: (parameters) @detail))) @def.function)
(chunk (variable_declaration (assignment_statement
  (variable_list name: (identifier) @name)
  (expression_list value: (function_definition parameters: (parameters) @detail)))) @def.function)
(chunk (variable_declaration (assignment_statement (variable_list name: (identifier) @name))) @def.variable)
