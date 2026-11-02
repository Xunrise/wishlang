//! Item identity and hashing.
//!
//! An "item" is a unit the user can adopt: a top-level type, fn, trait or
//! const (`Mode`, `run`), an enum variant (`Mode::Maximized`), an inherent
//! method or associated const (`Window::open`), or a whole trait impl
//! (`impl Display for Mode`). Java items are the same idea with `.` paths
//! (see `java.rs`).
//!
//! Hashes cover the item's *token stream* (including doc attributes), so
//! reformatting with rustfmt never breaks an adoption, but any change to
//! code or docs does. `sig_hash` covers only the public contract (signature
//! or type definition, without docs), which implement mode must preserve.

use crate::project::Lang;
use anyhow::{Context, Result};
use quote::ToTokens;
use syn::spanned::Spanned;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

#[derive(Debug, Clone)]
pub struct Item {
    pub path: String,
    pub kind: &'static str,
    pub hash: String,
    /// Doc comment text, joined with newlines.
    pub docs: String,
    /// 1-based line of the item's name.
    pub line: usize,
    /// 1-based last line of the item (closing brace), for "which item encloses this line".
    pub end_line: usize,
    /// Hash of the public contract: fn signature, or the type definition
    /// without doc attributes. None for impls and private items.
    pub sig_hash: Option<String>,
    /// Java only: the head of a method's return type or a field's type (`List` for `List<Code>`,
    /// `[]` for arrays), so a call chain can tell when it leaves the project's own types. A head the
    /// declaring file imports, or takes from java.lang, is `import:Head`: only the project's
    /// own types can make that one of ours, because only that file knew where it came from.
    pub ty: Option<String>,
}

impl Item {
    /// The `# Mocked` section, if this item is still a prototype.
    pub fn mocked(&self) -> Option<Vec<String>> {
        doc_section(&self.docs, "mocked")
    }
}

pub fn hash_tokens(t: &impl ToTokens) -> String {
    let d = Sha256::digest(t.to_token_stream().to_string().as_bytes());
    d.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

/// Hash of an item with its doc attributes stripped.
fn hash_without_docs<T: ToTokens + Clone>(t: &T, strip: impl Fn(&mut T)) -> String {
    let mut c = t.clone();
    strip(&mut c);
    hash_tokens(&c)
}

fn no_docs(attrs: &mut Vec<syn::Attribute>) {
    attrs.retain(|a| !a.path().is_ident("doc"));
}

fn docs(attrs: &[syn::Attribute]) -> String {
    attrs
        .iter()
        .filter(|a| a.path().is_ident("doc"))
        .filter_map(|a| match &a.meta {
            syn::Meta::NameValue(nv) => match &nv.value {
                syn::Expr::Lit(syn::ExprLit { lit: syn::Lit::Str(s), .. }) => Some(s.value()),
                _ => None,
            },
            _ => None,
        })
        .map(|l| l.strip_prefix(' ').unwrap_or(&l).to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

fn type_name(ty: &syn::Type) -> String {
    match ty {
        syn::Type::Path(p) => p.path.segments.last().map(|s| s.ident.to_string()).unwrap_or_default(),
        other => other.to_token_stream().to_string(),
    }
}

fn is_pub(v: &syn::Visibility) -> bool {
    !matches!(v, syn::Visibility::Inherited)
}

/// Parse a source file into its adoptable items.
pub fn parse(lang: Lang, src: &str) -> Result<Vec<Item>> {
    match lang {
        Lang::Rust => parse_rust(src),
        Lang::Java => crate::java::parse(src),
    }
}

fn parse_rust(src: &str) -> Result<Vec<Item>> {
    let file = syn::parse_file(src).context("parsing Rust source")?;
    let mut out = Vec::new();
    collect(&file.items, "", &mut out);
    Ok(out)
}

struct Push<'a> {
    out: &'a mut Vec<Item>,
    prefix: &'a str,
    /// End line of the node currently being pushed.
    end: usize,
}

impl Push<'_> {
    #[allow(clippy::too_many_arguments)]
    fn item(&mut self, path: String, kind: &'static str, hash: String, docs: String, line: usize, public: bool, sig_hash: Option<String>) {
        self.out.push(Item { path: format!("{}{path}", self.prefix), kind, hash, docs, line, end_line: self.end.max(line), sig_hash: if public { sig_hash } else { None }, ty: None });
    }
}

fn collect(items: &[syn::Item], prefix: &str, out: &mut Vec<Item>) {
    for item in items {
        use syn::Item as I;
        if let I::Mod(m) = item {
            if let Some((_, inner)) = &m.content {
                collect(inner, &format!("{prefix}{}::", m.ident), out);
            }
            continue;
        }
        let mut p = Push { out: &mut *out, prefix, end: item.span().end().line };
        match item {
            I::Struct(i) => p.item(i.ident.to_string(), "struct", hash_tokens(i), docs(&i.attrs), i.ident.span().start().line, is_pub(&i.vis), Some(hash_without_docs(i, |c| no_docs(&mut c.attrs)))),
            I::Enum(i) => {
                let sig = hash_without_docs(i, |c| {
                    no_docs(&mut c.attrs);
                    c.variants.iter_mut().for_each(|v| no_docs(&mut v.attrs));
                });
                p.item(i.ident.to_string(), "enum", hash_tokens(i), docs(&i.attrs), i.ident.span().start().line, is_pub(&i.vis), Some(sig));
                for v in &i.variants {
                    p.end = v.span().end().line;
                    p.item(format!("{}::{}", i.ident, v.ident), "variant", hash_tokens(v), docs(&v.attrs), v.ident.span().start().line, is_pub(&i.vis), None);
                }
            }
            I::Fn(i) => p.item(i.sig.ident.to_string(), "fn", hash_tokens(i), docs(&i.attrs), i.sig.ident.span().start().line, is_pub(&i.vis), Some(hash_tokens(&i.sig))),
            I::Trait(i) => p.item(i.ident.to_string(), "trait", hash_tokens(i), docs(&i.attrs), i.ident.span().start().line, is_pub(&i.vis), Some(hash_without_docs(i, |c| no_docs(&mut c.attrs)))),
            I::Type(i) => p.item(i.ident.to_string(), "type", hash_tokens(i), docs(&i.attrs), i.ident.span().start().line, is_pub(&i.vis), Some(hash_without_docs(i, |c| no_docs(&mut c.attrs)))),
            I::Const(i) => p.item(i.ident.to_string(), "const", hash_tokens(i), docs(&i.attrs), i.ident.span().start().line, is_pub(&i.vis), None),
            I::Static(i) => p.item(i.ident.to_string(), "static", hash_tokens(i), docs(&i.attrs), i.ident.span().start().line, is_pub(&i.vis), None),
            I::Union(i) => p.item(i.ident.to_string(), "union", hash_tokens(i), docs(&i.attrs), i.ident.span().start().line, is_pub(&i.vis), None),
            I::Impl(i) => {
                let ty = type_name(&i.self_ty);
                if let Some((tr, _)) = &i.trait_ {
                    let tr = tr.segments.last().map(|s| s.ident.to_string()).unwrap_or_default();
                    p.item(format!("impl {tr} for {ty}"), "impl", hash_tokens(i), docs(&i.attrs), i.impl_token.span.start().line, false, None);
                } else {
                    for ii in &i.items {
                        p.end = ii.span().end().line;
                        match ii {
                            syn::ImplItem::Fn(f) => p.item(format!("{ty}::{}", f.sig.ident), "method", hash_tokens(f), docs(&f.attrs), f.sig.ident.span().start().line, is_pub(&f.vis), Some(hash_tokens(&f.sig))),
                            syn::ImplItem::Const(c) => p.item(format!("{ty}::{}", c.ident), "assoc const", hash_tokens(c), docs(&c.attrs), c.ident.span().start().line, is_pub(&c.vis), None),
                            _ => {}
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

/// Map of item path → hash across several files. Unparseable files are skipped.
pub fn hash_map<'a>(lang: Lang, files: impl IntoIterator<Item = (&'a String, &'a String)>) -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    for (_, src) in files {
        if let Ok(items) = parse(lang, src) {
            for i in items {
                m.insert(i.path, i.hash);
            }
        }
    }
    m
}

/// A section of a doc comment, as bullet entries: markdown `# Name` (Rust) or
/// Javadoc `<h2>Name</h2>` with `<li>` bullets. Returns None if the heading is
/// absent. `# Name: text` counts as one entry.
pub fn doc_section(docs: &str, name: &str) -> Option<Vec<String>> {
    let html = regex::Regex::new(r"(?i)^<(h2|li|/?[uo]l)\b").unwrap();
    let tags = regex::Regex::new(r"<[^>]+>").unwrap();
    let mut out: Vec<String> = Vec::new();
    let mut found = false;
    let mut inside = false;
    for line in docs.lines() {
        let raw = line.trim();
        // Javadoc HTML: `<h2>X</h2>` reads as the heading `# X`, `<li>x</li>` as the bullet `- x`.
        let converted;
        let t = match html.captures(raw) {
            Some(c) => {
                let text = tags.replace_all(raw, "").trim().to_string();
                converted = match c[1].to_ascii_lowercase().as_str() {
                    "h2" => format!("# {text}"),
                    "li" => format!("- {text}"),
                    _ => text,
                };
                converted.as_str()
            }
            None if raw.contains("</") => {
                converted = tags.replace_all(raw, "").trim().to_string();
                converted.as_str()
            }
            None => raw,
        };
        if t.starts_with('#') {
            let h = t.trim_start_matches('#').trim();
            let (head, rest) = h.split_once(':').unwrap_or((h, ""));
            inside = head.trim().eq_ignore_ascii_case(name);
            if inside {
                found = true;
                if !rest.trim().is_empty() {
                    out.push(rest.trim().to_string());
                }
            }
            continue;
        }
        if inside {
            if let Some(b) = t.strip_prefix("- ").or_else(|| t.strip_prefix("* ")) {
                out.push(b.to_string());
            } else if !t.is_empty() {
                match out.last_mut() {
                    Some(last) => {
                        last.push(' ');
                        last.push_str(t);
                    }
                    None => out.push(t.to_string()),
                }
            }
        }
    }
    found.then_some(out)
}

/// Extract the `# Decisions` section (bullet lines) from a doc comment.
pub fn local_decisions(docs: &str) -> Vec<String> {
    doc_section(docs, "decisions").unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = r#"
/// A mode.
pub enum Mode { Maximized, Fullscreen }
pub struct Window;
impl Window {
    /// Opens it.
    ///
    /// # Decisions
    /// - Maximized asks the WM, not exclusive
    ///   fullscreen.
    ///
    /// # Mocked
    /// - Doesn't really open anything.
    pub fn open(self, m: Mode) -> Self { self }
}
impl std::fmt::Display for Mode { fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result { Ok(()) } }
"#;

    fn get<'a>(items: &'a [Item], path: &str) -> &'a Item {
        items.iter().find(|i| i.path == path).unwrap()
    }

    #[test]
    fn finds_items() {
        let paths: Vec<_> = parse(Lang::Rust, SRC).unwrap().into_iter().map(|i| i.path).collect();
        assert_eq!(paths, ["Mode", "Mode::Maximized", "Mode::Fullscreen", "Window", "Window::open", "impl Display for Mode"]);
    }

    #[test]
    fn lines() {
        let items = parse(Lang::Rust, SRC).unwrap();
        assert_eq!(get(&items, "Mode").line, 3);
        assert_eq!(get(&items, "Window::open").line, 14);
        assert_eq!(get(&items, "Window::open").end_line, 14);
        assert_eq!(get(&items, "Window").end_line, 4);
        let multi = parse(Lang::Rust, "pub struct A;\nimpl A {\n    pub fn f(&self) {\n        let x = 1;\n    }\n}\n").unwrap();
        assert_eq!((get(&multi, "A::f").line, get(&multi, "A::f").end_line), (3, 5));
    }

    #[test]
    fn hash_stable_under_reformat() {
        let ws = SRC.replace("pub fn open(self, m: Mode) -> Self { self }", "pub fn open(self,   m: Mode)\n -> Self {\n self\n }");
        assert_eq!(get(&parse(Lang::Rust, SRC).unwrap(), "Window::open").hash, get(&parse(Lang::Rust, &ws).unwrap(), "Window::open").hash);
    }

    #[test]
    fn hash_changes_with_docs_but_sig_does_not() {
        let a = parse(Lang::Rust, SRC).unwrap();
        let b = parse(Lang::Rust, &SRC.replace("Opens it.", "Opens it now.").replace("-> Self { self }", "-> Self { let s = self; s }")).unwrap();
        assert_ne!(get(&a, "Window::open").hash, get(&b, "Window::open").hash);
        assert_eq!(get(&a, "Window::open").sig_hash, get(&b, "Window::open").sig_hash);
        let c = parse(Lang::Rust, &SRC.replace("m: Mode", "m: Mode, x: u8")).unwrap();
        assert_ne!(get(&a, "Window::open").sig_hash, get(&c, "Window::open").sig_hash);
    }

    #[test]
    fn sections() {
        let items = parse(Lang::Rust, SRC).unwrap();
        let open = get(&items, "Window::open");
        assert_eq!(local_decisions(&open.docs), ["Maximized asks the WM, not exclusive fullscreen."]);
        assert_eq!(open.mocked().unwrap(), ["Doesn't really open anything."]);
        assert!(get(&items, "Mode").mocked().is_none());
        assert_eq!(doc_section("# Mocked: fakes it", "mocked").unwrap(), ["fakes it"]);
    }

    #[test]
    fn javadoc_sections() {
        let docs = "Opens it.\n<h2>Decisions</h2>\n<ul>\n<li>Maximized asks the WM,\nnot exclusive.</li>\n<li>Second.</li>\n</ul>\n<h2>Mocked</h2>\n<ul><li>Fakes the <code>scene</code>.</li></ul>";
        assert_eq!(local_decisions(docs), ["Maximized asks the WM, not exclusive.", "Second."]);
        assert_eq!(doc_section(docs, "mocked").unwrap(), ["Fakes the scene."]);
        assert!(doc_section("Opens it.", "mocked").is_none());
    }
}
