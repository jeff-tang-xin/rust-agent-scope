(function_definition name: (word) @name) @def.function
(program (variable_assignment name: (variable_name) @name) @def.variable)
(program (declaration_command (variable_assignment name: (variable_name) @name)) @def.variable)
