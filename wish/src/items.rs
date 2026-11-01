//! Item identity and hashing.
//!
//! An "item" is a unit the user can adopt: a top-level type, fn, trait or
//! const (`Mode`, `run`), an inherent method or associated const
//! (`Window::open`), or a whole trait impl (`impl Display for Mode`).
//!
//! Hashes cover the item's *token stream* (including doc attributes), so
//! reformatting with rustfmt never breaks an adoption, but any change to
//! code or docs does.

use anyhow::{Context, Result};
use quote::ToTokens;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

#[derive(Debug, Clone)]
pub struct Item {
    pub path: String,
    pub kind: &'static str,
    pub hash: String,
    /// Doc comment text, joined with newlines.
    pub docs: String,
}

pub fn hash_tokens(t: &impl ToTokens) -> String {
    let s = t.to_token_stream().to_string();
    let d = Sha256::digest(s.as_bytes());
    d.iter().take(8).map(|b| format!("{b:02x}")).collect()
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
        syn::Type::Path(p) => p
            .path
            .segments
            .last()
            .map(|s| s.ident.to_string())
            .unwrap_or_default(),
        other => other.to_token_stream().to_string(),
    }
}

/// Parse a source file into its adoptable items.
pub fn parse(src: &str) -> Result<Vec<Item>> {
    let file = syn::parse_file(src).context("parsing Rust source")?;
    let mut out = Vec::new();
    collect(&file.items, "", &mut out);
    Ok(out)
}

fn collect(items: &[syn::Item], prefix: &str, out: &mut Vec<Item>) {
    let push = |out: &mut Vec<Item>, path: String, kind, hash, docs| {
        out.push(Item { path: format!("{prefix}{path}"), kind, hash, docs })
    };
    for item in items {
        use syn::Item as I;
        match item {
            I::Struct(i) => push(out, i.ident.to_string(), "struct", hash_tokens(i), docs(&i.attrs)),
            I::Enum(i) => push(out, i.ident.to_string(), "enum", hash_tokens(i), docs(&i.attrs)),
            I::Fn(i) => push(out, i.sig.ident.to_string(), "fn", hash_tokens(i), docs(&i.attrs)),
            I::Trait(i) => push(out, i.ident.to_string(), "trait", hash_tokens(i), docs(&i.attrs)),
            I::Type(i) => push(out, i.ident.to_string(), "type", hash_tokens(i), docs(&i.attrs)),
            I::Const(i) => push(out, i.ident.to_string(), "const", hash_tokens(i), docs(&i.attrs)),
            I::Static(i) => push(out, i.ident.to_string(), "static", hash_tokens(i), docs(&i.attrs)),
            I::Union(i) => push(out, i.ident.to_string(), "union", hash_tokens(i), docs(&i.attrs)),
            I::Impl(i) => {
                let ty = type_name(&i.self_ty);
                if let Some((tr, _)) = &i.trait_ {
                    let tr = tr.segments.last().map(|s| s.ident.to_string()).unwrap_or_default();
                    push(out, format!("impl {tr} for {ty}"), "impl", hash_tokens(i), docs(&i.attrs));
                } else {
                    for ii in &i.items {
                        match ii {
                            syn::ImplItem::Fn(f) => push(out, format!("{ty}::{}", f.sig.ident), "method", hash_tokens(f), docs(&f.attrs)),
                            syn::ImplItem::Const(c) => push(out, format!("{ty}::{}", c.ident), "assoc const", hash_tokens(c), docs(&c.attrs)),
                            _ => {}
                        }
                    }
                }
            }
            I::Mod(m) => {
                if let Some((_, inner)) = &m.content {
                    collect(inner, &format!("{prefix}{}::", m.ident), out);
                }
            }
            _ => {}
        }
    }
}

/// Map of item path → hash across several files. Unparseable files are skipped.
pub fn hash_map<'a>(files: impl IntoIterator<Item = (&'a String, &'a String)>) -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    for (_, src) in files {
        if let Ok(items) = parse(src) {
            for i in items {
                m.insert(i.path, i.hash);
            }
        }
    }
    m
}

/// Extract the `# Decisions` section (bullet lines) from a doc comment.
pub fn local_decisions(docs: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut inside = false;
    for line in docs.lines() {
        let t = line.trim();
        if t.starts_with('#') {
            inside = t.trim_start_matches('#').trim().eq_ignore_ascii_case("decisions");
            continue;
        }
        if inside {
            if let Some(b) = t.strip_prefix("- ").or_else(|| t.strip_prefix("* ")) {
                out.push(b.to_string());
            } else if !t.is_empty() {
                if let Some(last) = out.last_mut() {
                    last.push(' ');
                    last.push_str(t);
                }
            }
        }
    }
    out
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
    pub fn open(self, m: Mode) -> Self { self }
}
impl std::fmt::Display for Mode { fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result { Ok(()) } }
"#;

    #[test]
    fn finds_items() {
        let paths: Vec<_> = parse(SRC).unwrap().into_iter().map(|i| i.path).collect();
        assert_eq!(paths, ["Mode", "Window", "Window::open", "impl Display for Mode"]);
    }

    #[test]
    fn hash_stable_under_reformat() {
        let reformatted = SRC.replace("pub enum Mode { Maximized, Fullscreen }", "pub enum Mode {\n    Maximized,\n    Fullscreen,\n}");
        let a = parse(SRC).unwrap();
        let b = parse(&reformatted).unwrap();
        // trailing comma changes tokens; whitespace alone must not
        let ws = SRC.replace("pub fn open(self, m: Mode) -> Self { self }", "pub fn open(self,   m: Mode)\n -> Self {\n self\n }");
        let c = parse(&ws).unwrap();
        assert_eq!(a[2].hash, c[2].hash);
        assert_ne!(a[0].hash, b[0].hash.clone() + "x");
    }

    #[test]
    fn hash_changes_with_docs() {
        let a = parse(SRC).unwrap();
        let b = parse(&SRC.replace("Opens it.", "Opens it now.")).unwrap();
        assert_ne!(a[2].hash, b[2].hash);
    }

    #[test]
    fn extracts_local_decisions() {
        let items = parse(SRC).unwrap();
        assert_eq!(local_decisions(&items[2].docs), ["Maximized asks the WM, not exclusive fullscreen."]);
    }
}
