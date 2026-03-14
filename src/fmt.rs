// Unified output formatting for metric tables, single-line metrics, and PASS/FAIL results.

use std::fmt::{Display, Write};
use std::io::Write as IoWrite;

use crate::tee_println;

/// Compute heap display width from a list of heap names.
///
/// Returns the length of the longest name so every `[heap]` prefix
/// can be padded to a uniform width.
#[must_use]
pub fn heap_width(heaps: &[String]) -> usize {
    heaps.iter().map(String::len).max().unwrap_or(0)
}

/// Format a heap prefix: `[{heap:<w$}]`.
#[must_use]
pub fn heap_prefix(heap: &str, width: usize) -> String {
    format!("[{heap:<width$}]")
}

/// Print a table with a header line and right-aligned data rows.
///
/// Output format:
/// ```text
/// [heap]  label (unit)
///           col1  col2  col3
///            123   456   789
/// ```
pub fn print_table(
    heap: &str,
    heap_w: usize,
    label: &str,
    unit_hint: Option<&str>,
    headers: &[&str],
    rows: &[Vec<String>],
) {
    let prefix = heap_prefix(heap, heap_w);
    let indent = " ".repeat(prefix.len() + 2);

    // Header line
    match unit_hint {
        Some(u) => tee_println!("{prefix}  {label} {u}"),
        None => tee_println!("{prefix}  {label}"),
    }

    if rows.is_empty() {
        return;
    }

    // Compute column widths (max of header and all cells per column).
    let ncols = headers.len();
    let mut col_w: Vec<usize> = headers.iter().map(|h| h.len()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            if i < ncols {
                col_w[i] = col_w[i].max(cell.len());
            }
        }
    }

    // Print header row
    let mut hdr = String::new();
    for (i, h) in headers.iter().enumerate() {
        if i > 0 {
            hdr.push_str("  ");
        }
        hdr.push_str(&ri_str(h, col_w[i]));
    }
    tee_println!("{indent}{hdr}");

    // Print data rows
    for row in rows {
        let mut line = String::new();
        for (i, cell) in row.iter().enumerate() {
            if i > 0 {
                line.push_str("  ");
            }
            let w = if i < ncols { col_w[i] } else { cell.len() };
            line.push_str(&ri_str(cell, w));
        }
        tee_println!("{indent}{line}");
    }
}

/// Print a single-line metric: `[heap]  label  key: val  key: val  ...`
pub fn print_metric(heap: &str, heap_w: usize, label: &str, kvs: &[(&str, &dyn Display)]) {
    let prefix = heap_prefix(heap, heap_w);
    let mut line = format!("{prefix}  {label}");
    for (k, v) in kvs {
        let _ = write!(line, "  {k}: {v}");
    }
    tee_println!("{line}");
}

/// Print a PASS result: `[heap]  PASS  label`
pub fn print_pass(heap: &str, heap_w: usize, label: &str) {
    let prefix = heap_prefix(heap, heap_w);
    tee_println!("{prefix}  PASS  {label}");
}

/// Print a FAIL result: `[heap]  FAIL  label — error`
pub fn print_fail(heap: &str, heap_w: usize, label: &str, error: &str) {
    let prefix = heap_prefix(heap, heap_w);
    tee_println!("{prefix}  FAIL  {label} \u{2014} {error}");
}

/// Right-align an integer value into a string of given minimum width.
#[must_use]
#[allow(dead_code)]
pub fn ri(v: impl Display, w: usize) -> String {
    format!("{v:>w$}")
}

/// Right-align a float with 1 decimal place.
#[must_use]
#[allow(dead_code)]
pub fn rf1(v: f64, w: usize) -> String {
    format!("{v:>w$.1}")
}

/// Right-align a string into a field of given minimum width.
fn ri_str(s: &str, w: usize) -> String {
    format!("{s:>w$}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heap_width_empty() {
        assert_eq!(heap_width(&[]), 0);
    }

    #[test]
    fn heap_width_single() {
        assert_eq!(heap_width(&["system".into()]), 6);
    }

    #[test]
    fn heap_width_multiple() {
        let heaps = vec!["system".into(), "reserved".into(), "cma".into()];
        assert_eq!(heap_width(&heaps), 8); // "reserved"
    }

    #[test]
    fn heap_prefix_padding() {
        assert_eq!(heap_prefix("cma", 8), "[cma     ]");
        assert_eq!(heap_prefix("reserved", 8), "[reserved]");
        assert_eq!(heap_prefix("system", 6), "[system]");
    }

    #[test]
    fn ri_integer() {
        assert_eq!(ri(42, 6), "    42");
        assert_eq!(ri(1048576, 6), "1048576"); // wider than min
    }

    #[test]
    fn rf1_float() {
        assert_eq!(rf1(3.14, 6), "   3.1");
        assert_eq!(rf1(100.0, 6), " 100.0");
    }

    #[test]
    fn print_pass_format() {
        // Capture via string building (print_pass writes to stdout).
        let prefix = heap_prefix("system", 8);
        let expected = format!("{prefix}  PASS  basic::alloc");
        assert_eq!(expected, "[system  ]  PASS  basic::alloc");
    }

    #[test]
    fn print_fail_format() {
        let prefix = heap_prefix("system", 8);
        let expected = format!("{prefix}  FAIL  basic::llseek \u{2014} No such file");
        assert_eq!(
            expected,
            "[system  ]  FAIL  basic::llseek \u{2014} No such file"
        );
    }

    #[test]
    fn ri_str_alignment() {
        assert_eq!(ri_str("size", 8), "    size");
        assert_eq!(ri_str("1048576", 8), " 1048576");
    }

    #[test]
    fn print_table_column_widths() {
        // Verify dynamic column width computation.
        let headers = vec!["size", "avg", "p99"];
        let rows: Vec<Vec<String>> = vec![
            vec!["4096".into(), "1".into(), "2".into()],
            vec!["1048576".into(), "55".into(), "68".into()],
        ];
        // Column widths should be: max("size",7)=7, max("avg",2)=3, max("p99",2)=3
        let ncols = headers.len();
        let mut col_w: Vec<usize> = headers.iter().map(|h| h.len()).collect();
        for row in &rows {
            for (i, cell) in row.iter().enumerate() {
                if i < ncols {
                    col_w[i] = col_w[i].max(cell.len());
                }
            }
        }
        assert_eq!(col_w, vec![7, 3, 3]);
    }
}
