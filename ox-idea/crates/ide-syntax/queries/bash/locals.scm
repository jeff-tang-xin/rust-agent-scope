; Adapted from nvim-treesitter queries/bash/locals.scm (Apache-2.0). Plain
; assignments and loop variables are global in Bash, so only `local` /
; `declare` / `typeset` / `readonly` inside a function declare a local.

; Scopes
(function_definition) @local.scope

; Definitions
(function_definition
  (compound_statement
    (declaration_command
      (variable_assignment
        name: (variable_name) @local.definition.var))))

(function_definition
  (compound_statement
    (declaration_command
      (variable_name) @local.definition.var)))

(function_definition
  name: (word) @local.definition.function
  (#set! definition.function.scope "parent"))

; References
(variable_name) @local.reference

(word) @local.reference
