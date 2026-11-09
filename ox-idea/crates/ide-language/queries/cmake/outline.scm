(function_def (function_command (argument_list . (argument) @name))) @def.function
(macro_def (macro_command (argument_list . (argument) @name))) @def.macro
(normal_command (identifier) @_c (argument_list . (argument) @name) (#match? @_c "^(?i)(project)$")) @outline.module
(normal_command (identifier) @_c (argument_list . (argument) @name) (#match? @_c "^(add_executable|add_library|ADD_EXECUTABLE|ADD_LIBRARY)$")) @def.class
