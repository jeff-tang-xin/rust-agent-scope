; Adapted from nvim-treesitter queries/haskell/locals.scm (Apache-2.0):
; captures the bound names (not whole declarations) and adds scopes.

(function
  name: (variable) @local.definition.function
  (#set! definition.function.scope "parent"))

(bind
  name: (variable) @local.definition.var)

(pattern/variable) @local.definition.parameter

(expression/variable) @local.reference

[
  (function)
  (lambda)
  (let_in)
  (alternative)
] @local.scope
