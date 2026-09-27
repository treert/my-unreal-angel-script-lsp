//! 文档存储：overlay 表（LSP实现规划 §5.1）。
//!
//! - **overlay 是文本的唯一真值来源**（打开期间）：didOpen 后内容以客户端为准，
//!   didClose 时丢弃（M2 的三个请求只服务已打开文档，无磁盘回落需求；
//!   磁盘读取与 workspace 索引随 M3 落地，届时按 §5.1 规则回落）；
//! - 增量 didChange：逐 change 换算字节区间 → 文本拼接 + `Tree::edit` →
//!   带 old_tree 增量 parse（§5.2：单文件同步完成，毫秒级）；
//! - `didSave` 不触发重读（磁盘内容此刻必然等于 overlay）。
//!
//! 本模块不依赖 LSP 协议类型：协议 `TextDocumentContentChangeEvent` 在
//! server.rs 转成中性的 [`TextChange`]（行/UTF-16 列），便于单测。

use std::collections::HashMap;

use as_core::id::FileId;
use as_core::intern::intern_file;
use as_core::{as_syntax, LineIndex};
use as_syntax::tree_sitter::{InputEdit, Point, Tree};

/// 一次文本变更（UTF-16 坐标，与协议一致）；`range = None` 即全量替换。
pub struct TextChange {
    pub range: Option<((u32, u32), (u32, u32))>,
    pub text: String,
}

/// 一个已打开文档的全部派生物。
pub struct Doc {
    pub version: i32,
    pub text: String,
    pub tree: Tree,
    pub lines: LineIndex,
}

/// 打开文档表（FileId → Doc）。所有变更在锁内同步完成（§5.2）。
#[derive(Default)]
pub struct DocStore {
    docs: HashMap<FileId, Doc>,
}

impl DocStore {
    pub fn new() -> Self {
        DocStore { docs: HashMap::new() }
    }

    pub fn open(&mut self, path: &str, version: i32, text: String) -> FileId {
        let file = intern_file(path, 0);
        let lines = LineIndex::new(&text);
        let tree = as_syntax::parse(&text, None);
        self.docs.insert(file, Doc { version, text, tree, lines });
        file
    }

    pub fn close(&mut self, file: FileId) {
        self.docs.remove(&file);
    }

    pub fn get(&self, file: FileId) -> Option<&Doc> {
        self.docs.get(&file)
    }

    /// 已打开文档迭代（M6：Ready 补推诊断用——锁内完成计算再发布）。
    pub fn entries(&self) -> impl Iterator<Item = (FileId, &Doc)> {
        self.docs.iter().map(|(f, d)| (*f, d))
    }

    /// overlay 快照（FileId, version, text）——冷启动线程构建索引用（§5.1
    /// overlay 优先：不与本锁交叉持锁，快照后释放）。
    pub fn overlays(&self) -> Vec<(FileId, i32, String)> {
        self.docs
            .iter()
            .map(|(f, d)| (*f, d.version, d.text.clone()))
            .collect()
    }

    /// 应用一批增量/全量变更（按客户端发送顺序）。
    pub fn apply_changes(&mut self, file: FileId, version: i32, changes: Vec<TextChange>) {
        let Some(doc) = self.docs.get_mut(&file) else {
            return;
        };
        doc.version = version;

        for change in changes {
            let Some(((sl, sc), (el, ec))) = change.range else {
                // 全量替换（兜底路径，不带 old_tree）
                doc.text = change.text;
                doc.lines = LineIndex::new(&doc.text);
                doc.tree = as_syntax::parse(&doc.text, None);
                continue;
            };

            let start = doc.lines.offset_of_utf16(&doc.text, sl, sc);
            let old_end = doc.lines.offset_of_utf16(&doc.text, el, ec);
            let new_text = change.text;

            // tree-sitter 增量编辑描述（Point 列单位：字节）
            let (s_line, s_col) = (doc.lines.line_of(start), start - doc.lines.line_start(doc.lines.line_of(start)));
            let inserted_newlines = new_text.bytes().filter(|&b| b == b'\n').count();
            let new_end = start as usize + new_text.len();
            let (ne_line, ne_col) = if inserted_newlines == 0 {
                (s_line, s_col + new_text.len() as u32)
            } else {
                let after_last_nl = new_text.rsplit('\n').next().map(str::len).unwrap_or(0);
                (s_line + inserted_newlines, after_last_nl as u32)
            };
            let (oe_line, oe_col) = {
                let l = doc.lines.line_of(old_end);
                (l, old_end - doc.lines.line_start(l))
            };

            doc.tree.edit(&InputEdit {
                start_byte: start as usize,
                old_end_byte: old_end as usize,
                new_end_byte: new_end,
                start_position: Point::new(s_line as usize, s_col as usize),
                old_end_position: Point::new(oe_line as usize, oe_col as usize),
                new_end_position: Point::new(ne_line as usize, ne_col as usize),
            });

            let mut text = String::with_capacity(doc.text.len() + new_text.len());
            text.push_str(&doc.text[..start as usize]);
            text.push_str(&new_text);
            text.push_str(&doc.text[old_end as usize..]);
            doc.text = text;
            doc.lines = LineIndex::new(&doc.text);
        }

        // 带 old_tree 增量 parse（§5.2）
        let new_tree = as_syntax::parse(&doc.text, Some(&doc.tree));
        doc.tree = new_tree;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn change(sl: u32, sc: u32, el: u32, ec: u32, text: &str) -> TextChange {
        TextChange {
            range: Some(((sl, sc), (el, ec))),
            text: text.to_string(),
        }
    }

    // 单测用例内置于源码（D1）。

    #[test]
    fn incremental_edit_keeps_syntax() {
        let mut store = DocStore::new();
        let base = "class Foo { int X; }\n".to_string();
        let file = store.open("unique://lspdocs/a.as", 1, base);

        // 在 "X" 前插入 "Y, " → class Foo { int Y, X; }
        store.apply_changes(file, 2, vec![change(0, 16, 0, 16, "Y, ")]);
        let doc = store.get(file).unwrap();
        assert_eq!(doc.text, "class Foo { int Y, X; }\n");
        assert_eq!(doc.version, 2);
        // 增量 parse 后语法树仍零错误
        assert!(as_syntax::verify_tree(&doc.tree).is_empty());
        let root = doc.tree.root_node();
        let class = root.named_child(0).unwrap();
        let body = class.child_by_field_name("body").unwrap();
        // `int Y, X;` 是一个 variable_declaration（内含两个 declarator）
        let var_decl = body.named_child(0).unwrap();
        assert_eq!(var_decl.kind(), "variable_declaration");
        let mut declarators = 0;
        let mut c = var_decl.walk();
        if c.goto_first_child() {
            loop {
                if c.node().kind() == "variable_declarator" {
                    declarators += 1;
                }
                if !c.goto_next_sibling() {
                    break;
                }
            }
        }
        assert_eq!(declarators, 2);
    }

    #[test]
    fn multi_line_insert_and_delete() {
        let mut store = DocStore::new();
        let file = store.open("unique://lspdocs/b.as", 1, "void F()\n{\n}\n".to_string());
        // 在 F() 体（行 1 末）插入两行语句
        store.apply_changes(
            file,
            2,
            vec![change(1, 1, 1, 1, "\n    int A = 1;\n    int B = 2;")],
        );
        let doc = store.get(file).unwrap();
        assert!(as_syntax::verify_tree(&doc.tree).is_empty(), "{}", doc.text);
        assert_eq!(
            doc.text,
            "void F()\n{\n    int A = 1;\n    int B = 2;\n}\n"
        );
        // 再删掉两行
        store.apply_changes(
            file,
            3,
            vec![change(2, 0, 4, 0, "")],
        );
        let doc = store.get(file).unwrap();
        assert_eq!(doc.text, "void F()\n{\n}\n");
        assert!(as_syntax::verify_tree(&doc.tree).is_empty());
    }

    #[test]
    fn full_replace_and_close() {
        let mut store = DocStore::new();
        let file = store.open("unique://lspdocs/c.as", 1, "int A;\n".to_string());
        store.apply_changes(
            file,
            2,
            vec![TextChange { range: None, text: "class B {}\n".to_string() }],
        );
        assert_eq!(store.get(file).unwrap().text, "class B {}\n");
        store.close(file);
        assert!(store.get(file).is_none());
    }
}
