// A raw string containing comment-like text.
pub const PATTERN: &str = r#"// not a comment"#;
pub const BLOCK: &str = r##"/* not a comment "# either */"##; // a real trailing comment
pub const PLAIN: &str = "// nor this"; /* a real block comment */
