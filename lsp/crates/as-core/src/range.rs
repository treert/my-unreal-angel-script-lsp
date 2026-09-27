//! TextRange + 行首偏移表（LSP实现规划 §3.2.1）。
//!
//! as-core 内部一律**字节偏移**；行/UTF-16 列换算只在 as-lsp 边界发生，
//! 本模块提供换算原语。行首表是 Phase 2 的持久产物（LSP实现规划 §4），
//! 随文件文本一起失效/重建。

/// 字节偏移区间 `[start, end)`。
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct TextRange {
    pub start: u32,
    pub end: u32,
}

impl TextRange {
    #[inline]
    pub fn new(start: u32, end: u32) -> Self {
        TextRange { start, end }
    }

    #[inline]
    pub fn len(self) -> u32 {
        self.end.saturating_sub(self.start)
    }

    #[inline]
    pub fn is_empty(self) -> bool {
        self.start >= self.end
    }

    #[inline]
    pub fn contains(self, byte: u32) -> bool {
        self.start <= byte && byte < self.end
    }
}

/// 每文件行首字节偏移表（`line_starts[0] == 0`）。
#[derive(Clone, Debug)]
pub struct LineIndex {
    line_starts: Vec<u32>,
}

impl LineIndex {
    pub fn new(text: &str) -> Self {
        let mut line_starts = vec![0u32];
        for (i, b) in text.bytes().enumerate() {
            if b == b'\n' {
                line_starts.push(i as u32 + 1);
            }
        }
        LineIndex { line_starts }
    }

    pub fn line_count(&self) -> usize {
        self.line_starts.len()
    }

    /// 0 起行号。
    pub fn line_of(&self, byte: u32) -> usize {
        match self.line_starts.binary_search(&byte) {
            Ok(line) => line,
            Err(next) => next.saturating_sub(1),
        }
    }

    pub fn line_start(&self, line: usize) -> u32 {
        self.line_starts[line.min(self.line_starts.len() - 1)]
    }

    /// 0 起行 + UTF-16 列（LSP `Position` 语义，as-lsp 边界专用）。
    pub fn line_col_utf16(&self, text: &str, byte: u32) -> (u32, u32) {
        let line = self.line_of(byte);
        let start = self.line_starts[line] as usize;
        let end = (byte as usize).min(text.len());
        let mut col16 = 0u32;
        for ch in text[start..end].chars() {
            col16 += ch.len_utf16() as u32;
        }
        (line as u32, col16)
    }

    /// LSP `Position`（0 起行 + UTF-16 列）→ 字节偏移。越界取边界值。
    /// as-lsp 边界专用（增量编辑的 range 换算）。
    pub fn offset_of_utf16(&self, text: &str, line: u32, col16: u32) -> u32 {
        let line = line as usize;
        if line >= self.line_starts.len() {
            return text.len() as u32;
        }
        let start = self.line_starts[line] as usize;
        let line_end = if line + 1 < self.line_starts.len() {
            self.line_starts[line + 1] as usize
        } else {
            text.len()
        };
        let mut col = 0u32;
        let mut offset = start;
        for (i, ch) in text[start..line_end].char_indices() {
            if ch == '\n' || ch == '\r' {
                break;
            }
            if col >= col16 {
                return (start + i) as u32;
            }
            col += ch.len_utf16() as u32;
            offset = start + i + ch.len_utf8();
        }
        offset as u32
    }

    /// 1 起行 + 1 起字节列（dump / 调试输出用；不是 LSP 语义）。
    pub fn line_col_debug(&self, byte: u32) -> (u32, u32) {
        let line = self.line_of(byte);
        (line as u32 + 1, byte - self.line_starts[line] + 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_index_basic() {
        let text = "ab\ncd\r\nef";
        let li = LineIndex::new(text);
        assert_eq!(li.line_count(), 3);
        assert_eq!(li.line_of(0), 0);
        assert_eq!(li.line_of(3), 1); // '\n' 归属前一行末尾
        assert_eq!(li.line_of(4), 1);
        assert_eq!(li.line_of(7), 2); // 第二个 '\n'（\r\n 之后）
        assert_eq!(li.line_col_debug(3), (2, 1)); // 'c'
        assert_eq!(li.line_col_debug(4), (2, 2)); // 'd'
    }

    #[test]
    fn utf16_col_counts_surrogate_pairs() {
        let text = "a\u{1F600}b\nc";
        let li = LineIndex::new(text);
        // 'a'(1B) + emoji(4B=2 utf16 units) + 'b'(1B) → 行 0；行 1 的 'c'
        let (line, col) = li.line_col_utf16(text, "a\u{1F600}b".len() as u32 + 1);
        assert_eq!((line, col), (1, 0));
        let (_, col) = li.line_col_utf16(text, 1);
        assert_eq!(col, 1);
        let (_, col) = li.line_col_utf16(text, 5); // emoji 之后
        assert_eq!(col, 3); // 1 + 2 个 utf16 单元
    }
}
