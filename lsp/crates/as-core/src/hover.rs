//! hover 渲染（架构设计 §4.6 / §2.4.4 规则 2）：
//! ` ```angelscript_snippet ` code fence 的紧凑签名 + doxygen tag 的 markdown 渲染。
//!
//! - 全部候选（重载组/同名多落点）的签名逐行放进同一个 fence（规划 §8.1 M3）；
//! - doc 注释里 doxygen 常见 8 tag（@param/@return/@returns/@see/@note/
//!   @warning/@brief/@todo）渲染为 markdown 结构，其余原样（D15 规则 2）；
//! - `@group` 的完整包路径（§2.2.1 推论 2）附在签名后一行。
//! 纯函数，全部可单测（D1：用例内置于源码）。

use crate::id::DefId;
use crate::index::WorkspaceIndex;
use crate::intern::sym_str;
use crate::resolve::Target;
use crate::symbol::{DefExtra, DefFlags, DefKind};
use crate::types::{RefKind, SynType};

/// 生成 hover markdown（fence + 签名们 + group + doc）。
pub fn hover_markdown(idx: &WorkspaceIndex, targets: &[Target]) -> Option<String> {
    if targets.is_empty() {
        return None;
    }
    let mut sigs = Vec::with_capacity(targets.len());
    for t in targets {
        sigs.push(signature(idx, t)?);
    }
    let mut out = format!("```angelscript_snippet\n{}\n```", sigs.join("\n"));

    // 归属显示：.d.as 文件头 @group 的完整包路径（§2.2.1 推论 2；
    // 合成符号回落源头声明所在文件）
    if let Target::Def(id) = &targets[0] {
        let d = idx.def(*id);
        let group_def = d.origin.map(|o| idx.def(o)).unwrap_or(d);
        if let Some(group) = idx.files.get(&group_def.file).and_then(|s| s.group.as_deref()) {
            out.push_str(&format!("\n\n_{group}_"));
        }
    }

    // doc（合成符号回落源头声明）
    let doc = match &targets[0] {
        Target::Def(id) => {
            let d = idx.def(*id);
            d.doc
                .as_deref()
                .or_else(|| d.origin.and_then(|o| idx.def(o).doc.as_deref()))
        }
        Target::Local(_) => None,
    };
    if let Some(d) = doc {
        if !d.is_empty() {
            out.push_str("\n\n");
            out.push_str(&render_doc(d));
        }
    }
    Some(out)
}

/// 紧凑签名（单行）。
pub fn signature(idx: &WorkspaceIndex, target: &Target) -> Option<String> {
    match target {
        Target::Local(l) => {
            let kind = if l.kind == DefKind::Param { "param" } else { "local" };
            let ty = l.ty.as_ref().map(render_syn).unwrap_or_else(|| "?".into());
            Some(format!("{ty} {}  // {kind}", sym_str(l.name)))
        }
        Target::Def(id) => Some(def_signature(idx, *id)),
    }
}

fn def_signature(idx: &WorkspaceIndex, id: DefId) -> String {
    let d = idx.def(id);
    let name = sym_str(d.name);
    match &d.extra {
        DefExtra::Callable { return_type, params } => {
            let head = match d.kind {
                DefKind::Constructor | DefKind::Destructor => String::new(),
                DefKind::Delegate => "delegate ".to_string(),
                DefKind::Event => "event ".to_string(),
                _ => match return_type {
                    Some(t) => format!("{} ", render_syn(t)),
                    None => "void ".to_string(),
                },
            };
            let ps: Vec<String> = params
                .iter()
                .map(|p| match &p.ty {
                    Some(t) => format!("{} {}", render_syn(t), sym_str(p.name)),
                    None => sym_str(p.name).to_string(),
                })
                .collect();
            let tail = if d.flags.contains(DefFlags::CONST) { " const" } else { "" };
            format!("{head}{name}({}){tail}", ps.join(", "))
        }
        DefExtra::Variable { ty } => {
            let t = ty.as_ref().map(render_syn).unwrap_or_else(|| "?".into());
            match d.kind {
                DefKind::AssetDecl => format!("asset {name} of {t}"),
                DefKind::VirtualProperty => format!("{t} {name} {{ get; set; }}"),
                _ => format!("{t} {name}"),
            }
        }
        DefExtra::TypeDecl { bases, .. } => match d.kind {
            DefKind::Class => {
                let base = bases
                    .iter()
                    .map(|b| sym_str(b.name).to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                if base.is_empty() {
                    format!("class {name}")
                } else {
                    format!("class {name} : {base}")
                }
            }
            DefKind::Struct => format!("struct {name}"),
            _ => format!("enum {name}"),
        },
        DefExtra::EnumValue { value } => {
            let enum_name = d
                .parent
                .map(|p| format!("{}::", sym_str(idx.def(p).name)))
                .unwrap_or_default();
            match value {
                Some(v) => format!("{enum_name}{name} = {v}"),
                None => format!("{enum_name}{name}"),
            }
        }
        DefExtra::None => match d.kind {
            DefKind::Namespace => format!("namespace {name}"),
            _ => name.to_string(),
        },
    }
}

/// 语法层类型渲染（保持源码形态：const / &in / T[] / Name<Args>）。
pub fn render_syn(syn: &SynType) -> String {
    match syn {
        SynType::Primitive(name, _) | SynType::Named(name, _) => sym_str(*name).to_string(),
        SynType::Auto => "auto".into(),
        SynType::Wildcard => "?".into(),
        SynType::Template { name, args, .. } => {
            let inner: Vec<String> = args.iter().map(render_syn).collect();
            format!("{}<{}>", sym_str(*name), inner.join(", "))
        }
        SynType::Qualified(segs) => segs
            .iter()
            .map(|(s, _)| sym_str(*s).to_string())
            .collect::<Vec<_>>()
            .join("::"),
        SynType::Array(inner) => format!("{}[]", render_syn(inner)),
        SynType::Const(inner) => format!("const {}", render_syn(inner)),
        SynType::Ref(inner, k) => {
            let sep = match k {
                RefKind::Plain => "",
                _ => " ",
            };
            format!("{}{}{}", render_syn(inner), sep, k.label())
        }
        SynType::UnresolvedObject(inner) => format!("{} unresolved_object", render_syn(inner)),
    }
}

/// doxygen tag → markdown（§2.4.4 规则 2：8 个常见 tag 渲染，其余原样）。
pub fn render_doc(doc: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    for line in doc.lines() {
        let trimmed = line.trim_start();
        let Some(tagged) = trimmed.strip_prefix('@') else {
            out.push(line.to_string());
            continue;
        };
        let (tag, rest) = match tagged.find(char::is_whitespace) {
            Some(i) => (&tagged[..i], tagged[i..].trim()),
            None => (tagged, ""),
        };
        match tag {
            "param" | "arg" => {
                let mut parts = rest.splitn(2, char::is_whitespace);
                let name = parts.next().unwrap_or("");
                let desc = parts.next().unwrap_or("").trim();
                if name.is_empty() {
                    out.push(format!("- {rest}"));
                } else {
                    out.push(format!("- `{name}` — {desc}"));
                }
            }
            "return" | "returns" => out.push(format!("**Returns:** {rest}")),
            "see" => out.push(format!("**See:** {rest}")),
            "note" => out.push(format!("**Note:** {rest}")),
            "warning" => out.push(format!("**Warning:** {rest}")),
            "todo" => out.push(format!("**TODO:** {rest}")),
            // @brief 就是正文首段——去 tag 保留文本
            "brief" => out.push(rest.to_string()),
            _ => out.push(line.to_string()),
        }
    }
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{FileInput, FileKind, IndexConfig};
    use crate::intern::{intern_file, intern_sym};
    use crate::resolve::{resolve_at, LEVEL_DECL_SELF};

    fn build(srcs: &[(&str, &str)]) -> WorkspaceIndex {
        let inputs = srcs
            .iter()
            .map(|(path, src)| FileInput {
                file: intern_file(path, 0),
                kind: if path.ends_with(".d.as") { FileKind::Decl } else { FileKind::Script },
                source: (*src).to_string(),
                module: None,
            })
            .collect();
        WorkspaceIndex::build(IndexConfig::default(), inputs)
    }

    /// 用例源码全部内置（AGENTS.md 硬性规则 / D1）。

    const DECL: &str = "\
// ==== AngelscriptTypeDeclarations ====
// @cache_format 1
// @group /Script/Engine
// Generated by AngelscriptTypeExporter - do not edit.

class AActor
{
    // Returns the location of the actor.
    // @param InName The name to check
    // @return The location vector
    // @see GetActorTransform
    FVector GetActorLocation(Name InName) const;
}
";

    #[test]
    fn hover_fence_and_doc_render() {
        let idx = build(&[("unique://hov/Engine.d.as", DECL)]);
        let file = intern_file("unique://hov/Engine.d.as", 0);
        let byte = DECL.find("GetActorLocation(Name").unwrap() as u32;
        let r = resolve_at(&idx, file, byte).unwrap();
        assert_eq!(r.level, LEVEL_DECL_SELF, "声明自指也可 hover");

        let md = hover_markdown(&idx, &r.targets).unwrap();
        assert!(md.starts_with("```angelscript_snippet\n"), "fence 起始: {md}");
        assert!(md.contains("FVector GetActorLocation(Name InName) const"), "签名: {md}");
        // @group 完整包路径
        assert!(md.contains("_/Script/Engine_"), "group 路径: {md}");
        // doxygen 渲染
        assert!(md.contains("- `InName` — The name to check"), "@param: {md}");
        assert!(md.contains("**Returns:** The location vector"), "@return: {md}");
        assert!(md.contains("**See:** GetActorTransform"), "@see: {md}");
        assert!(md.contains("Returns the location of the actor."), "doc 正文: {md}");
    }

    #[test]
    fn hover_local_param() {
        const SRC: &str = "void F(float Delta) { float L = Delta; }\n";
        let idx = build(&[("unique://hov/local.as", SRC)]);
        let file = intern_file("unique://hov/local.as", 0);
        let byte = SRC.rfind("Delta").unwrap() as u32;
        let r = resolve_at(&idx, file, byte).unwrap();
        let md = hover_markdown(&idx, &r.targets).unwrap();
        assert!(md.contains("float Delta"), "参数签名: {md}");
        assert!(md.contains("param"), "param 标注: {md}");
    }

    #[test]
    fn hover_delegate_synthetic_member() {
        const DLG: &str =
            "delegate void FOnHit(int Damage);\nclass A { FOnHit OnHit; void M() { OnHit.Execute(5); } }\n";
        let idx = build(&[("unique://hov/dlg.as", DLG)]);
        let file = intern_file("unique://hov/dlg.as", 0);
        let byte = DLG.find("Execute").unwrap() as u32;
        let r = resolve_at(&idx, file, byte).unwrap();
        let md = hover_markdown(&idx, &r.targets).unwrap();
        // 展开成员的签名（参数从委托声明克隆）
        assert!(md.contains("void Execute(int Damage) const"), "Execute 签名: {md}");
    }

    #[test]
    fn hover_unnamed_param_placeholder_kept() {
        // InArgN 占位名在签名中原样展示（声明就是如此）；命名实参「补全」跳过它
        const D: &str = "void SetX(float64 InArg0);\n";
        let idx = build(&[("unique://hov/arg.d.as", D)]);
        let file = intern_file("unique://hov/arg.d.as", 0);
        let byte = D.find("SetX").unwrap() as u32;
        let r = resolve_at(&idx, file, byte).unwrap();
        let md = hover_markdown(&idx, &r.targets).unwrap();
        assert!(md.contains("void SetX(float64 InArg0)"), "占位名原样: {md}");
    }

    #[test]
    fn hover_multiple_targets_in_one_fence() {
        // 重载组：全部签名同 fence 逐行。M4 起调用点先消歧（§7.2 消费方表：
        // hover 渲染选中候选）——`Log(1)` 唯一命中 int 重载；不可定型实参
        // （未解析符号）保留整组。
        const O: &str =
            "void Log(FString S) {}\nvoid Log(int N) {}\nvoid F() { Log(1); Log(Unresolved()); }\n";
        let idx = build(&[("unique://hov/ovl.as", O)]);
        let file = intern_file("unique://hov/ovl.as", 0);

        // 消歧成功：hover 显示选中的重载
        let byte = O.find("Log(1)").unwrap() as u32;
        let r = resolve_at(&idx, file, byte).unwrap();
        let md = hover_markdown(&idx, &r.targets).unwrap();
        assert!(md.contains("void Log(int N)"), "选中重载: {md}");
        assert!(!md.contains("void Log(FString S)"), "消歧后不显示另一重载: {md}");

        // 消歧失败：整组同 fence
        let byte = O.find("Log(Unresolved())").unwrap() as u32;
        let r = resolve_at(&idx, file, byte).unwrap();
        assert_eq!(r.targets.len(), 2, "不可定型实参 ⇒ 保留全部重载");
        let md = hover_markdown(&idx, &r.targets).unwrap();
        assert!(md.contains("void Log(FString S)"), "重载1: {md}");
        assert!(md.contains("void Log(int N)"), "重载2: {md}");
    }

    #[test]
    fn hover_class_signature_with_base() {
        const C: &str = "class APawn : AActor {}\nvoid F() { APawn P; }\n";
        let idx = build(&[("unique://hov/cls.as", C)]);
        let file = intern_file("unique://hov/cls.as", 0);
        let byte = C.find("APawn P").unwrap() as u32;
        let r = resolve_at(&idx, file, byte).unwrap();
        let md = hover_markdown(&idx, &r.targets).unwrap();
        assert!(md.contains("class APawn : AActor"), "类签名: {md}");
        let _ = intern_sym("unused");
    }
}
