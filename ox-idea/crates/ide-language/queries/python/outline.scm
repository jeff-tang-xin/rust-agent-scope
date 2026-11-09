(class_definition name: (identifier) @name) @def.class
(function_definition name: (identifier) @name parameters: (parameters) @detail) @def.function

(module (expression_statement (assignment left: (identifier) @name) @def.variable))
(class_definition body: (block (expression_statement (assignment left: (identifier) @name) @def.field)))
