//! Human-readable Parquet previews, separate from the lossless inspection API.

use std::fmt::Write;

use unicode_width::UnicodeWidthStr;

use crate::cli::parquet_cell;
use crate::parquet::ParquetInspection;

fn single_line(text: &str) -> String {
    text.chars()
        .flat_map(|ch| {
            if ch.is_control() {
                ch.escape_default().collect::<Vec<_>>()
            } else {
                vec![ch]
            }
        })
        .collect()
}

fn padded(text: &str, width: usize) -> String {
    format!("{text}{}", " ".repeat(width.saturating_sub(text.width())))
}

fn table(out: &mut String, headers: &[String], rows: &[Vec<String>]) {
    let widths: Vec<_> = headers
        .iter()
        .enumerate()
        .map(|(index, header)| {
            rows.iter()
                .map(|row| row[index].width())
                .max()
                .unwrap_or(0)
                .max(header.width())
        })
        .collect();
    let line = |row: &[String]| {
        row.iter()
            .zip(&widths)
            .map(|(cell, width)| padded(cell, *width))
            .collect::<Vec<_>>()
            .join(" | ")
    };
    writeln!(out, "{}", line(headers)).unwrap();
    writeln!(
        out,
        "{}",
        widths
            .iter()
            .map(|width| "-".repeat(*width))
            .collect::<Vec<_>>()
            .join("-+-")
    )
    .unwrap();
    for row in rows {
        writeln!(out, "{}", line(row)).unwrap();
    }
}

pub(crate) fn render(
    report: &ParquetInspection,
    full: bool,
    expanded: bool,
    width: usize,
) -> String {
    let mut out = format!(
        "file: {}\nrows: {}  row groups: {}  selected columns: {}\n",
        single_line(&report.path),
        report.rows_total,
        report.row_groups,
        report.columns.len()
    );
    if report.preview_rows.is_empty() {
        out.push_str("\nSchema\n");
        table(
            &mut out,
            &["Column".into(), "Type".into(), "Nullable".into()],
            &report
                .columns
                .iter()
                .map(|column| {
                    vec![
                        single_line(&column.name),
                        single_line(&column.r#type),
                        if column.nullable { "yes" } else { "no" }.into(),
                    ]
                })
                .collect::<Vec<_>>(),
        );
        if report.preview_offset > 0 {
            writeln!(
                out,
                "\nNo preview rows at offset {}.",
                report.preview_offset
            )
            .unwrap();
        }
        return out;
    }
    let headers: Vec<_> = std::iter::once("Row".into())
        .chain(
            report
                .columns
                .iter()
                .map(|column| single_line(&column.name)),
        )
        .collect();
    let rows: Vec<Vec<String>> = report
        .preview_rows
        .iter()
        .enumerate()
        .map(|(index, row)| {
            std::iter::once((report.preview_offset + index + 1).to_string())
                .chain(report.columns.iter().map(|column| {
                    row.get(&column.name)
                        .map(|value| parquet_cell(value, full))
                        .unwrap_or_default()
                }))
                .collect()
        })
        .collect();
    let table_width = headers
        .iter()
        .enumerate()
        .map(|(index, header)| {
            rows.iter()
                .map(|row| row[index].width())
                .max()
                .unwrap_or(0)
                .max(header.width())
        })
        .sum::<usize>()
        + (headers.len() - 1) * 3;
    out.push_str("\nPreview\n");
    if expanded || table_width > width {
        let name_width = headers
            .iter()
            .skip(1)
            .map(|name| name.width())
            .max()
            .unwrap_or(0);
        for row in &rows {
            writeln!(out, "── Row {} ──", row[0]).unwrap();
            for (name, value) in headers.iter().skip(1).zip(row.iter().skip(1)) {
                // Wrap values by terminal display width, including wide Unicode characters.
                let prefix = format!("{} : ", padded(name, name_width));
                let available = width.saturating_sub(prefix.width()).max(1);
                let mut used = 0;
                out.push_str(&prefix);
                for ch in value.chars() {
                    let char_width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
                    if used + char_width > available && used > 0 {
                        out.push('\n');
                        out.push_str(&" ".repeat(prefix.width()));
                        used = 0;
                    }
                    out.push(ch);
                    used += char_width;
                }
                out.push('\n');
            }
        }
    } else {
        table(&mut out, &headers, &rows);
    }
    writeln!(
        out,
        "\npreviewed {} of {} row(s){}; offset {} (zero-based)",
        report.preview_rows.len(),
        report.rows_total,
        if report.preview_truncated {
            " (truncated)"
        } else {
            ""
        },
        report.preview_offset
    )
    .unwrap();
    if !full {
        out.push_str("Cells limited to 120 characters (…); use --full for complete values.\n");
    }
    out.push_str("Use --schema for column definitions; --columns name,... and --offset N --limit N for a local preview.\n");
    out
}
