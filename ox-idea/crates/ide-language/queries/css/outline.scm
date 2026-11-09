(rule_set (selectors) @name) @outline.class
(media_statement "@media" @name) @outline.namespace
(keyframes_statement (keyframes_name) @name) @outline.function
(supports_statement "@supports" @name) @outline.namespace
(declaration (property_name) @name (#match? @name "^--")) @outline.variable
