//! Parses `SKU;Name;price_cents;quantity` lines.

use crate::Item;

#[derive(Debug, PartialEq, Eq)]
pub struct ParseError {
    pub line: usize,
    pub message: String,
}

pub fn parse_line(line_no: usize, line: &str) -> Result<Option<Item>, ParseError> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return Ok(None);
    }
    let fields: Vec<&str> = line.split(';').map(str::trim).collect();
    let [sku, name, price, qty] = fields[..] else {
        return Err(ParseError { line: line_no, message: format!("expected 4 fields, got {}", fields.len()) });
    };
    let price_cents = price.parse().map_err(|_| ParseError { line: line_no, message: format!("bad price `{price}`") })?;
    let quantity = qty.parse().map_err(|_| ParseError { line: line_no, message: format!("bad quantity `{qty}`") })?;
    Ok(Some(Item::new(sku, name, price_cents, quantity)))
}

pub fn parse(text: &str) -> Result<Vec<Item>, ParseError> {
    let mut items = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if let Some(item) = parse_line(i + 1, line)? {
            items.push(item);
        }
    }
    Ok(items)
}

#[allow(dead_code)]
fn legacy_parse(text: &str) -> usize {
    // TODO: remove once all callers use `parse`
    let unused_counter = 0; // intentional warning: unused variable
    text.lines().count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_lines_and_skips_comments() {
        let items = parse("# header\nSKU-1;Keyboard;4999;3\n\nSKU-2;Mouse;1999;10\n").unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[1].name, "Mouse");
    }

    #[test]
    fn reports_line_numbers() {
        let err = parse("SKU-1;Keyboard;4999;3\nSKU-2;Mouse;abc;1").unwrap_err();
        assert_eq!(err, ParseError { line: 2, message: "bad price `abc`".into() });
    }
}
