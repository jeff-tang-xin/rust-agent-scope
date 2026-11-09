(call target: (identifier) @_k (arguments (alias) @name) (#any-of? @_k "defmodule" "defprotocol")) @def.module
(call target: (identifier) @_k (arguments (alias) @name) (#eq? @_k "defimpl")) @outline.impl
(call target: (identifier) @_k (arguments [
  (call target: (identifier) @name)
  (identifier) @name
  (binary_operator left: (call target: (identifier) @name))
]) (#any-of? @_k "def" "defp" "defdelegate" "defguard" "defguardp" "defn" "defnp")) @def.function
(call target: (identifier) @_k (arguments [
  (call target: (identifier) @name)
  (identifier) @name
  (binary_operator left: (call target: (identifier) @name))
]) (#any-of? @_k "defmacro" "defmacrop")) @def.macro
(call target: (identifier) @_k (arguments (string) @name) (#any-of? @_k "test" "describe")) @def.function
