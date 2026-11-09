(element (start_tag (tag_name) @name (attribute (attribute_name) @_a (quoted_attribute_value (attribute_value) @detail)) (#eq? @_a "id"))) @outline.field
(element (start_tag (tag_name) @name) (#any-of? @name "html" "head" "body" "header" "footer" "main" "nav" "section" "article" "aside" "form" "table" "template" "dialog")) @outline.field
(script_element (start_tag (tag_name) @name)) @outline.field
(style_element (start_tag (tag_name) @name)) @outline.field
