Outline queries (`<grammar>/outline.scm`), consumed by `src/syntax/outline.rs`.

Captures: `@def.<kind>` (outline + symbol index), `@outline.<kind>` (outline only),
`@name` (identifier; several are joined with `.`), `@detail`, `@trait` (Rust impls),
`@container` (explicit container, e.g. Go receivers). Captures starting with `_` are ignored.

Kinds: class interface struct enum enum_member trait function method constructor
field property variable constant module namespace package type macro event operator
key heading impl extension. A `function` directly inside a type becomes a `method`.
When several patterns capture the same node, the earliest pattern wins.
