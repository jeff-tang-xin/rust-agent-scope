; HCL / Terraform highlighting (the grammar crate ships no query). Later patterns win.

(comment) @comment
(string_lit) @string
(quoted_template) @string
(heredoc_template) @string
[(heredoc_start) (heredoc_identifier)] @string.special
(template_literal) @string
(numeric_lit) @number
[(bool_lit) (null_lit)] @keyword

(identifier) @variable
(attribute (identifier) @property)
(object_elem key: (expression (variable_expr (identifier) @property)))
(get_attr (identifier) @property)
(function_call (identifier) @function)
; `resource "aws_instance" "web" {`
(block (identifier) @keyword)
(block (string_lit (template_literal) @type))

(template_interpolation) @embedded
[(template_interpolation_start) (template_interpolation_end)] @punctuation.special
[(template_directive_start) (template_directive_end) (strip_marker)] @punctuation.special
["for" "in" "if" "else" "endfor" "endif"] @keyword

(operation) @operator
["!" "!=" "%" "&&" "*" "+" "-" "/" "<" "<=" "==" ">" ">=" "||" "?" "=>"] @operator
["=" ":" "," "."] @punctuation.delimiter
["(" ")" "[" "]" "{" "}" "[*]" ".*"] @punctuation.bracket
