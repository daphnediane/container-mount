/*
 * Copyright (c) 2026 Daphne Pfister
 * SPDX-License-Identifier: BSD-2-Clause
 * See LICENSE file for full license text
 */

//! Small helpers for rendering plain-text tables.

/// Render rows as left-aligned columns separated by `gap` spaces. The last
/// column is not padded.
#[must_use]
pub fn columns(rows: &[Vec<String>], gap: usize) -> Vec<String> {
    let ncols = rows.iter().map(Vec::len).max().unwrap_or(0);
    let widths: Vec<usize> = (0..ncols)
        .map(|c| {
            rows.iter()
                .filter_map(|r| r.get(c))
                .map(|s| s.chars().count())
                .max()
                .unwrap_or(0)
        })
        .collect();
    rows.iter()
        .map(|row| {
            let mut line = String::new();
            for (c, cell) in row.iter().enumerate() {
                if c + 1 == row.len() {
                    line.push_str(cell);
                } else {
                    let pad = widths[c] + gap - cell.chars().count();
                    line.push_str(cell);
                    line.extend(std::iter::repeat_n(' ', pad));
                }
            }
            line
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn columns_pad_all_but_last() {
        let rows = vec![
            vec!["a".to_string(), "bb".to_string(), "c".to_string()],
            vec!["aaa".to_string(), "b".to_string(), "c".to_string()],
        ];
        assert_eq!(columns(&rows, 2), ["a    bb  c", "aaa  b   c"]);
    }
}
