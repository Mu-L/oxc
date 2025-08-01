use std::path::Path;

use nonmax::NonMaxU32;
use oxc_index::{Idx, IndexVec};
use oxc_span::Span;
use oxc_syntax::identifier::{LS, PS};

// Irregular line breaks - '\u{2028}' (LS) and '\u{2029}' (PS)
const LS_OR_PS_FIRST: u8 = 0xE2;
const LS_OR_PS_SECOND: u8 = 0x80;
const LS_THIRD: u8 = 0xA8;
const PS_THIRD: u8 = 0xA9;

/// Number of lines to check with linear search when translating byte position to line index
const LINE_SEARCH_LINEAR_ITERATIONS: usize = 16;

/// Index into vec of `ColumnOffsets`
#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct ColumnOffsetsId(NonMaxU32);

impl Idx for ColumnOffsetsId {
    #[expect(clippy::cast_possible_truncation)]
    fn from_usize(idx: usize) -> Self {
        assert!(idx < u32::MAX as usize);
        // SAFETY: We just checked `idx` is a legal value for `NonMaxU32`
        Self(unsafe { NonMaxU32::new_unchecked(idx as u32) })
    }

    fn index(self) -> usize {
        self.0.get() as usize
    }
}

/// Line offset tables.
///
/// Used for tracking lines and columns from byte offsets via binary search.
///
/// Code is adapted from [esbuild](https://github.com/evanw/esbuild/blob/cc74e6042a9f573bf58e1e3f165ebda70af4ad3b/internal/js_printer/js_printer.go#L4806-L4808)
///
/// Most lines of source code will not contain Unicode chars, so optimize storage for this common case.
///
/// Each line is represented by a `Line`.
/// Where a line is entirely ASCII, translating byte offset to UTF-16 column is simple,
/// given the byte offset of start of line. A column lookup table isn't needed for that line.
/// In this case, `Line::column_offsets_id` is `None`.
/// For rare lines which do contain Unicode chars, we store column offsets in a `ColumnOffsets` which
/// is stored in a separate `IndexVec`. `Line::column_offsets_id` contains index for that line's `ColumnOffsets`.
/// Storing column offset info which is rarely used in a separate structure keeps `Line` as small as possible.
#[derive(Debug, Default)]
pub struct LineOffsetTables {
    lines: Vec<Line>,
    column_offsets: IndexVec<ColumnOffsetsId, ColumnOffsets>,
}

#[derive(Debug)]
pub struct Line {
    byte_offset_to_start_of_line: u32,
    column_offsets_id: Option<ColumnOffsetsId>,
}

#[derive(Debug)]
pub struct ColumnOffsets {
    byte_offset_to_first: u32,
    columns: Box<[u32]>,
}

#[expect(clippy::struct_field_names)]
pub struct SourcemapBuilder<'a> {
    source_id: u32,
    original_source: &'a str,
    last_generated_update: usize,
    last_position: Option<u32>,
    line_offset_tables: LineOffsetTables,
    sourcemap_builder: oxc_sourcemap::SourceMapBuilder,
    generated_line: u32,
    generated_column: u32,
    /// Tracks the last accessed line index to optimize sequential lookups in `search_original_line_and_column`.
    /// Most calls to this method access positions in increasing order (e.g., when mapping source tokens linearly),
    /// so we can avoid unnecessary binary searches by advancing linearly from this cached index.
    last_line_lookup: u32,
}

impl<'a> SourcemapBuilder<'a> {
    pub fn new(path: &Path, source_text: &'a str) -> Self {
        let mut sourcemap_builder = oxc_sourcemap::SourceMapBuilder::default();
        let line_offset_tables = Self::generate_line_offset_tables(source_text);
        let source_id =
            sourcemap_builder.set_source_and_content(path.to_string_lossy().as_ref(), source_text);
        Self {
            source_id,
            original_source: source_text,
            last_generated_update: 0,
            last_position: None,
            line_offset_tables,
            sourcemap_builder,
            generated_line: 0,
            generated_column: 0,
            last_line_lookup: 0,
        }
    }

    pub fn into_sourcemap(self) -> oxc_sourcemap::SourceMap {
        self.sourcemap_builder.into_sourcemap()
    }

    pub fn add_source_mapping_for_name(&mut self, output: &[u8], span: Span, name: &str) {
        debug_assert!(
            (span.end as usize) <= self.original_source.len(),
            "violated {}:{} <= {} for {name}",
            span.start,
            span.end,
            self.original_source.len()
        );
        let original_name = self.original_source.get(span.start as usize..span.end as usize);
        // The token name should be original name.
        // If it hasn't change, name should be `None` to reduce `SourceMap` size.
        let token_name = if original_name == Some(name) { None } else { original_name };
        self.add_source_mapping(output, span.start, token_name);
    }

    pub fn add_source_mapping(&mut self, output: &[u8], position: u32, name: Option<&str>) {
        if self.last_position == Some(position) {
            return;
        }
        let (original_line, original_column) = self.search_original_line_and_column(position);
        self.update_generated_line_and_column(output);
        let name_id = name.map(|s| self.sourcemap_builder.add_name(s));
        self.sourcemap_builder.add_token(
            self.generated_line,
            self.generated_column,
            original_line,
            original_column,
            Some(self.source_id),
            name_id,
        );
        self.last_position = Some(position);
    }

    #[expect(clippy::cast_possible_truncation)]
    fn search_original_line_and_column(&mut self, position: u32) -> (u32, u32) {
        let original_line = self.search_original_line(position);

        // Store line index as starting point for next search
        self.last_line_lookup = original_line as u32;

        let line = &self.line_offset_tables.lines[original_line];
        let mut original_column = position - line.byte_offset_to_start_of_line;
        if let Some(column_offsets_id) = line.column_offsets_id {
            let column_offsets = &self.line_offset_tables.column_offsets[column_offsets_id];
            if original_column >= column_offsets.byte_offset_to_first {
                original_column = column_offsets.columns
                    [(original_column - column_offsets.byte_offset_to_first) as usize];
            }
        }
        (original_line as u32, original_column)
    }

    /// Find line index for byte index `position`, using line offset table.
    ///
    /// Usually output code is roughly in same order as it was in original source,
    /// so line will be close to the line found in last call to this function.
    ///
    /// So do fast linear search first over a few lines, and fallback to slower binary search
    /// if this doesn't find the line.
    fn search_original_line(&self, position: u32) -> usize {
        let lines = &self.line_offset_tables.lines;
        let idx = self.last_line_lookup as usize;

        if position >= lines[idx].byte_offset_to_start_of_line {
            self.search_original_line_forwards(position)
        } else {
            self.search_original_line_backwards(position)
        }
    }

    /// Find line index for byte index `position`, starting search at `last_line_lookup`,
    /// and working forwards.
    ///
    /// Search forwards, looking for first line which starts *after* `position`.
    /// `position` then must be on the line before that one.
    fn search_original_line_forwards(&self, position: u32) -> usize {
        let lines = &self.line_offset_tables.lines;
        let last_idx = self.last_line_lookup as usize;

        let start_idx = last_idx + 1;
        let end_idx = start_idx + LINE_SEARCH_LINEAR_ITERATIONS;

        // We have a fast path for when there are more than `LINE_SEARCH_LINEAR_ITERATIONS` lines
        // to search (common case unless file is very short).
        // Fast path is do linear search on first `LINE_SEARCH_LINEAR_ITERATIONS` lines,
        // then fallback to binary search over remaining lines.
        // If less than `LINE_SEARCH_LINEAR_ITERATIONS` lines to search, just do linear search,
        // but that's slower as number of lines being searched is not constant, so loop cannot be unrolled.
        if end_idx > lines.len() {
            // Less than `LINE_SEARCH_LINEAR_ITERATIONS` lines to search. Take slow path.
            // Unless file is very short, this branch is rarely taken.
            return self.search_original_line_forwards_when_few_lines(position);
        }

        // Linear search for `LINE_SEARCH_LINEAR_ITERATIONS` lines.
        // Compiler should unroll this loop as it has constant number of iterations.
        // https://godbolt.org/z/heh1cnYa4
        for (line_idx, line) in lines[start_idx..end_idx].iter().enumerate() {
            if line.byte_offset_to_start_of_line > position {
                // This line starts after `position`. `position` must be on previous line.
                return start_idx + line_idx - 1;
            }
        }

        // Line not found yet. Binary search over remaining lines.
        lines[end_idx..].partition_point(|line| line.byte_offset_to_start_of_line <= position)
            + end_idx
            - 1
    }

    #[cold]
    fn search_original_line_forwards_when_few_lines(&self, position: u32) -> usize {
        let lines = &self.line_offset_tables.lines;
        let last_idx = self.last_line_lookup as usize;
        let start_idx = last_idx + 1;

        for (line_idx, line) in lines[start_idx..].iter().enumerate() {
            if line.byte_offset_to_start_of_line > position {
                // This line starts after `position`. `position` must be on previous line.
                return start_idx + line_idx - 1;
            }
        }

        // No line starts after `position`. `position` must be on last line.
        lines.len() - 1
    }

    fn search_original_line_backwards(&self, position: u32) -> usize {
        let lines = &self.line_offset_tables.lines;
        let mut idx = self.last_line_lookup as usize;

        while lines[idx].byte_offset_to_start_of_line > position {
            idx -= 1;
        }

        if lines[idx].byte_offset_to_start_of_line < position {
            idx = lines
                .partition_point(|table| table.byte_offset_to_start_of_line <= position)
                .saturating_sub(1);
        }

        idx
    }

    #[expect(clippy::cast_possible_truncation)]
    fn update_generated_line_and_column(&mut self, output: &[u8]) {
        let remaining = &output[self.last_generated_update..];

        // Find last line break
        let mut line_start_ptr = remaining.as_ptr();
        let mut last_line_is_ascii = true;
        let mut iter = remaining.iter();
        while let Some(&b) = iter.next() {
            match b {
                b'\n' => {}
                b'\r' => {
                    // Handle Windows-specific "\r\n" newlines
                    if iter.clone().next() == Some(&b'\n') {
                        iter.next();
                    }
                }
                _ if b.is_ascii() => {
                    continue;
                }
                LS_OR_PS_FIRST => {
                    let next_byte = *iter.next().unwrap();
                    let next_next_byte = *iter.next().unwrap();
                    if next_byte != LS_OR_PS_SECOND
                        || !matches!(next_next_byte, LS_THIRD | PS_THIRD)
                    {
                        last_line_is_ascii = false;
                        continue;
                    }
                }
                _ => {
                    // Unicode char
                    last_line_is_ascii = false;
                    continue;
                }
            }

            // Line break found.
            // `iter` is now positioned after line break.
            line_start_ptr = iter.as_slice().as_ptr();
            self.generated_line += 1;
            self.generated_column = 0;
            last_line_is_ascii = true;
        }

        // Calculate column
        self.generated_column += if last_line_is_ascii {
            // `iter` is now exhausted, so `iter.as_slice().as_ptr()` is pointer to end of `output`
            (iter.as_slice().as_ptr() as usize - line_start_ptr as usize) as u32
        } else {
            let line_byte_offset = line_start_ptr as usize - remaining.as_ptr() as usize;
            // TODO: It'd be better if could use `from_utf8_unchecked` here, but we'd need to make this
            // function unsafe and caller guarantees `output` contains a valid UTF-8 string
            let last_line = std::str::from_utf8(&remaining[line_byte_offset..]).unwrap();
            // Mozilla's "source-map" library counts columns using UTF-16 code units
            last_line.encode_utf16().count() as u32
        };
        self.last_generated_update = output.len();
    }

    fn generate_line_offset_tables(content: &str) -> LineOffsetTables {
        let mut lines = vec![];
        let mut column_offsets = IndexVec::new();

        // Process content line-by-line.
        // For each line, start by assuming line will be entirely ASCII, and read byte-by-byte.
        // If line is all ASCII, UTF-8 columns and UTF-16 columns are the same,
        // so no need to create a `columns` Vec. This is the fast path for common case.
        // If a Unicode character found, read rest of line char-by-char, populating `columns` Vec.
        // At end of line, go back to top of outer loop, and again assume ASCII for next line.
        let mut line_byte_offset = 0;
        'lines: loop {
            lines.push(Line {
                byte_offset_to_start_of_line: line_byte_offset,
                column_offsets_id: None,
            });

            let remaining = &content.as_bytes()[line_byte_offset as usize..];
            for (byte_offset_from_line_start, b) in remaining.iter().enumerate() {
                #[expect(clippy::cast_possible_truncation)]
                let mut byte_offset_from_line_start = byte_offset_from_line_start as u32;
                match b {
                    b'\n' => {
                        byte_offset_from_line_start += 1;
                    }
                    b'\r' => {
                        byte_offset_from_line_start += 1;
                        // Handle Windows-specific "\r\n" newlines
                        if remaining.get(byte_offset_from_line_start as usize) == Some(&b'\n') {
                            byte_offset_from_line_start += 1;
                        }
                    }
                    _ if b.is_ascii() => {
                        continue;
                    }
                    _ => {
                        // Unicode char found.
                        // Set `column_offsets_id` for line and create `columns` Vec.
                        let line = lines.iter_mut().last().unwrap();
                        line.column_offsets_id =
                            Some(ColumnOffsetsId::from_usize(column_offsets.len()));

                        let mut columns = vec![];

                        // Loop through rest of line char-by-char.
                        // `chunk_byte_offset` in this loop is byte offset from start of this 1st
                        // Unicode char.
                        let mut column = byte_offset_from_line_start;
                        line_byte_offset += byte_offset_from_line_start;
                        let remaining = &content[line_byte_offset as usize..];
                        for (chunk_byte_offset, ch) in remaining.char_indices() {
                            #[expect(clippy::cast_possible_truncation)]
                            let mut chunk_byte_offset = chunk_byte_offset as u32;
                            columns.extend(std::iter::repeat_n(column, ch.len_utf8()));

                            match ch {
                                '\r' => {
                                    // Handle Windows-specific "\r\n" newlines
                                    chunk_byte_offset += 1;
                                    if remaining.as_bytes().get(chunk_byte_offset as usize)
                                        == Some(&b'\n')
                                    {
                                        chunk_byte_offset += 1;
                                        columns.push(column + 1);
                                    }
                                }
                                '\n' => {
                                    chunk_byte_offset += 1;
                                }
                                LS | PS => {
                                    chunk_byte_offset += 3;
                                }
                                #[expect(clippy::cast_possible_truncation)]
                                _ => {
                                    // Mozilla's "source-map" library counts columns using UTF-16 code units
                                    column += ch.len_utf16() as u32;
                                    continue;
                                }
                            }

                            // Line break found.
                            // `chunk_byte_offset` is now the offset of *end* of the line break.
                            line_byte_offset += chunk_byte_offset;

                            // Record column offsets
                            column_offsets.push(ColumnOffsets {
                                byte_offset_to_first: byte_offset_from_line_start,
                                columns: columns.into_boxed_slice(),
                            });

                            // Revert back to outer loop for next line
                            continue 'lines;
                        }

                        // EOF.
                        // One last column entry for EOF position.
                        columns.push(column);

                        // Record column offsets
                        column_offsets.push(ColumnOffsets {
                            byte_offset_to_first: byte_offset_from_line_start,
                            columns: columns.into_boxed_slice(),
                        });

                        break 'lines;
                    }
                }

                // Line break found.
                // `byte_offset_from_line_start` is now the length of line *including* line break.
                line_byte_offset += byte_offset_from_line_start;
                continue 'lines;
            }

            // EOF
            break;
        }

        LineOffsetTables { lines, column_offsets }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn builder_ascii() {
        assert_mapping("", &[(0, 0, 0)]);
        assert_mapping("a", &[(0, 0, 0), (1, 0, 1)]);
        assert_mapping("\n", &[(0, 0, 0), (1, 1, 0)]);
        assert_mapping("a\n", &[(0, 0, 0), (1, 0, 1), (2, 1, 0)]);
        assert_mapping("\na", &[(0, 0, 0), (1, 1, 0), (2, 1, 1)]);
        assert_mapping(
            "ab\ncd\n\nef",
            &[
                (0, 0, 0),
                (1, 0, 1),
                (2, 0, 2),
                (3, 1, 0),
                (4, 1, 1),
                (5, 1, 2),
                (6, 2, 0),
                (7, 3, 0),
                (8, 3, 1),
                (9, 3, 2),
            ],
        );

        assert_mapping("\r", &[(0, 0, 0), (1, 1, 0)]);
        assert_mapping("\r\r", &[(0, 0, 0), (1, 1, 0), (2, 2, 0)]);
        assert_mapping("a\ra", &[(0, 0, 0), (1, 0, 1), (2, 1, 0), (3, 1, 1)]);

        assert_mapping("\r\n", &[(0, 0, 0), (1, 0, 1), (2, 1, 0)]);
        assert_mapping("\r\n\r\n", &[(0, 0, 0), (1, 0, 1), (2, 1, 0), (3, 1, 1), (4, 2, 0)]);
        assert_mapping("a\r\na", &[(0, 0, 0), (1, 0, 1), (2, 0, 2), (3, 1, 0), (4, 1, 1)]);
    }

    #[test]
    fn builder_unicode() {
        assert_mapping("Ö", &[(0, 0, 0), (2, 0, 1)]);
        assert_mapping("ÖÖ", &[(0, 0, 0), (2, 0, 1), (4, 0, 2)]);
        assert_mapping("Ö\n", &[(0, 0, 0), (2, 0, 1), (3, 1, 0)]);
        assert_mapping("ÖÖ\n", &[(0, 0, 0), (2, 0, 1), (4, 0, 2), (5, 1, 0)]);
        assert_mapping("\nÖ", &[(0, 0, 0), (1, 1, 0), (3, 1, 1)]);
        assert_mapping("\nÖÖ", &[(0, 0, 0), (1, 1, 0), (3, 1, 1), (5, 1, 2)]);
        assert_mapping("Ö\nÖ", &[(0, 0, 0), (2, 0, 1), (3, 1, 0), (5, 1, 1)]);
        assert_mapping("\nÖÖ\n", &[(0, 0, 0), (1, 1, 0), (3, 1, 1), (5, 1, 2), (6, 2, 0)]);
        assert_mapping("Ö\ra", &[(0, 0, 0), (2, 0, 1), (3, 1, 0), (4, 1, 1)]);
        assert_mapping("Ö\r\na", &[(0, 0, 0), (2, 0, 1), (3, 0, 2), (4, 1, 0), (5, 1, 1)]);
    }

    #[test]
    fn builder_with_unordered_position() {
        assert_mapping("\na\nb", &[(4, 2, 1), (0, 0, 0), (1, 1, 0), (2, 1, 1), (3, 2, 0)]);
    }

    fn assert_mapping(source: &str, mappings: &[(u32, u32, u32)]) {
        let mut builder = SourcemapBuilder::new(Path::new("x.js"), source);
        for (position, expected_line, expected_col) in mappings.iter().copied() {
            let (line, col) = builder.search_original_line_and_column(position);
            assert_eq!(
                builder.search_original_line_and_column(position),
                (expected_line, expected_col),
                "Incorrect mapping for '{source}' - position {position} = line {line}, column {col}"
            );
        }
    }

    #[test]
    fn add_source_mapping() {
        fn create_mappings(source: &str, line: u32, column: u32) {
            let mut builder = SourcemapBuilder::new(Path::new("x.js"), source);
            let output: Vec<u8> = source.as_bytes().into();
            for (i, _ch) in source.char_indices() {
                #[expect(clippy::cast_possible_truncation)]
                builder.add_source_mapping(&output, i as u32, None);
                assert!(
                    builder.generated_line == line && builder.generated_column == column,
                    "Incorrect generated mapping for '{source}' ({:?}) starting at {i} - line {}, column {}",
                    source.as_bytes(),
                    builder.generated_line,
                    builder.generated_column
                );
                assert_eq!(builder.last_generated_update, source.len());
            }
        }

        create_mappings("", 0, 0);
        create_mappings("abc", 0, 3);
        create_mappings("\n", 1, 0);
        create_mappings("\n\n\n", 3, 0);
        create_mappings("\r", 1, 0);
        create_mappings("\r\r\r", 3, 0);
        create_mappings("\r\n", 1, 0);
        create_mappings("\r\n\r\n\r\n", 3, 0);
        create_mappings("\nabc", 1, 3);
        create_mappings("abc\n", 1, 0);
        create_mappings("\rabc", 1, 3);
        create_mappings("abc\r", 1, 0);
        create_mappings("\r\nabc", 1, 3);
        create_mappings("abc\r\n", 1, 0);
        create_mappings("ÖÖ\nÖ\nÖÖÖ", 2, 3);
    }

    #[test]
    fn add_source_mapping_for_name() {
        let output = b"ac";
        let mut builder = SourcemapBuilder::new(Path::new("x.js"), "ab");
        builder.add_source_mapping_for_name(output, Span::new(0, 1), "a");
        builder.add_source_mapping_for_name(output, Span::new(1, 2), "c");
        let sm = builder.into_sourcemap();
        // The name `a` not change.
        assert_eq!(
            sm.get_source_view_token(0_u32).as_ref().and_then(|token| token.get_name()),
            None
        );
        // The name `b` -> `c`, save `b` to token.
        assert_eq!(
            sm.get_source_view_token(1_u32).as_ref().and_then(|token| token.get_name()),
            Some(&"b".into())
        );
    }

    #[test]
    fn add_source_mapping_for_unordered_position() {
        let output = b"";
        let mut builder = SourcemapBuilder::new(Path::new("x.js"), "ab");
        builder.add_source_mapping(output, 1, None);
        builder.add_source_mapping(output, 0, None);
        let sm = builder.into_sourcemap();
        assert_eq!(sm.get_tokens().count(), 2);
    }

    static SOURCE: &str = "a\nbc\ndef";
    static MAPPINGS: &[(u32, u32)] = &[
        (0, 0), // 'a'
        (0, 1), // '\n'
        (1, 0), // 'b'
        (1, 1), // 'c'
        (1, 2), // '\n'
        (2, 0), // 'd'
        (2, 1), // 'e'
        (2, 2), // 'f'
        (2, 3), // EOF
    ];

    #[test]
    fn test_search_original_line_and_column_sequential() {
        let mut builder = SourcemapBuilder::new(Path::new("x.js"), SOURCE);

        #[expect(clippy::cast_possible_truncation)]
        for (pos, (expected_line, expected_col)) in MAPPINGS.iter().copied().enumerate() {
            let (line, col) = builder.search_original_line_and_column(pos as u32);
            assert_eq!((line, col), (expected_line, expected_col), "Mismatch at position {pos}");
        }
    }

    #[test]
    fn test_search_original_line_and_column_reverse_sequential() {
        let mut builder = SourcemapBuilder::new(Path::new("x.js"), SOURCE);

        #[expect(clippy::cast_possible_truncation)]
        for (pos, (expected_line, expected_col)) in MAPPINGS.iter().copied().enumerate().rev() {
            let (line, col) = builder.search_original_line_and_column(pos as u32);
            assert_eq!((line, col), (expected_line, expected_col), "Mismatch at position {pos}");
        }
    }

    #[test]
    fn test_search_original_line_and_column_non_sequential() {
        let mut builder = SourcemapBuilder::new(Path::new("x.js"), SOURCE);

        let indexes = [8, 0, 7, 1, 6, 2, 5, 3, 4];

        for pos in indexes {
            let (expected_line, expected_col) = MAPPINGS[pos as usize];
            let (line, col) = builder.search_original_line_and_column(pos);
            assert_eq!((line, col), (expected_line, expected_col), "Mismatch at position {pos}");
        }
    }
}
