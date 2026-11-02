//! "Will be generated": references in a buffer that the vocabulary doesn't
//! define yet. Purely syntactic (syn), so it works on unsaved buffers and in
//! milliseconds, at the cost of being a heuristic rather than type-aware.

use crate::items::Item;
use crate::project::Lang;
use proc_macro2::Span;
use std::collections::BTreeSet;
use syn::visit::{self, Visit};

#[derive(Debug, Clone, PartialEq)]
pub struct Pending {
    pub name: String,
    /// 0-based (line, column) start and end, as LSP wants them.
    pub start: (u32, u32),
    pub end: (u32, u32),
}

/// What the project already defines.
#[derive(Default)]
pub struct Known {
    paths: BTreeSet<String>,
    /// Last path segments of everything (variants, methods, types, fns).
    names: BTreeSet<String>,
}

impl Known {
    pub fn from_items<'a>(items: impl IntoIterator<Item = &'a Item>) -> Self {
        let mut k = Known::default();
        for i in items {
            k.paths.insert(i.path.clone());
            if let Some(last) = i.path.rsplit("::").next() {
                k.names.insert(last.to_string());
            }
        }
        k
    }
}

/// Names that come from std's prelude or are otherwise always available.
const PRELUDE: &[&str] = &[
    "Self", "Some", "None", "Ok", "Err", "Option", "Result", "String", "Vec", "Box", "Rc", "Arc", "Cell", "RefCell", "HashMap", "HashSet",
    "BTreeMap", "BTreeSet", "VecDeque", "Default", "Clone", "Copy", "Debug", "Display", "PartialEq", "Eq", "PartialOrd", "Ord", "Hash",
    "Iterator", "IntoIterator", "From", "Into", "TryFrom", "TryInto", "AsRef", "AsMut", "Send", "Sync", "Sized", "Drop", "Fn", "FnMut",
    "FnOnce", "ToString", "ToOwned", "Duration", "Instant", "Path", "PathBuf", "Ordering", "Mutex", "RwLock", "PhantomData", "Cow",
    "u8", "u16", "u32", "u64", "u128", "usize", "i8", "i16", "i32", "i64", "i128", "isize", "f32", "f64", "bool", "char", "str",
    "drop", "main",
];

fn upper(s: &str) -> bool {
    s.chars().next().is_some_and(|c| c.is_uppercase())
}

struct V<'a> {
    known: &'a Known,
    /// Names imported from outside the project (`use winit::window::Window`).
    imported: &'a BTreeSet<String>,
    out: Vec<Pending>,
}

impl V<'_> {
    fn push(&mut self, name: String, a: Span, b: Span) {
        let (s, e) = (a.start(), b.end());
        self.out.push(Pending { name, start: (s.line as u32 - 1, s.column as u32), end: (e.line as u32 - 1, e.column as u32) });
    }

    fn is_known_type(&self, t: &str) -> bool {
        PRELUDE.contains(&t) || self.known.paths.contains(t) || self.known.names.contains(t)
    }

    /// Check a path used in an expression, type or pattern position.
    fn check_path(&mut self, path: &syn::Path, call: bool) {
        let mut segs: Vec<&syn::PathSegment> = path.segments.iter().collect();
        // crate::/self::/super:: point into this project; other lowercase
        // roots are external crates or std modules, which we don't judge.
        while segs.first().is_some_and(|s| matches!(s.ident.to_string().as_str(), "crate" | "self" | "super")) {
            segs.remove(0);
        }
        if segs.is_empty() || path.leading_colon.is_some() {
            return;
        }
        let names: Vec<String> = segs.iter().map(|s| s.ident.to_string()).collect();
        let (first, last) = (segs[0].ident.span(), segs[segs.len() - 1].ident.span());
        match names.as_slice() {
            [one] if upper(one) => {
                if !self.is_known_type(one) {
                    self.push(one.clone(), first, last);
                }
            }
            [one] if call => {
                if !PRELUDE.contains(&one.as_str()) && !self.known.paths.contains(one) {
                    self.push(format!("{one}()"), first, last);
                }
            }
            [ty, member] if upper(ty) => {
                if PRELUDE.contains(&ty.as_str()) {
                    return;
                }
                let full = format!("{ty}::{member}");
                if !self.known.paths.contains(&full) {
                    self.push(full, first, last);
                }
            }
            _ => {}
        }
    }

    /// Does this receiver chain start at one of our (existing or pending) types?
    fn ours(&self, e: &syn::Expr) -> bool {
        match e {
            syn::Expr::MethodCall(m) => self.ours(&m.receiver),
            syn::Expr::Field(f) => self.ours(&f.base),
            syn::Expr::Try(t) => self.ours(&t.expr),
            syn::Expr::Paren(p) => self.ours(&p.expr),
            syn::Expr::Reference(r) => self.ours(&r.expr),
            syn::Expr::Call(c) => match &*c.func {
                syn::Expr::Path(p) => self.path_is_ours(&p.path),
                _ => false,
            },
            syn::Expr::Path(p) => self.path_is_ours(&p.path),
            syn::Expr::Struct(s) => self.path_is_ours(&s.path),
            _ => false,
        }
    }

    fn path_is_ours(&self, p: &syn::Path) -> bool {
        let first = p.segments.iter().map(|s| s.ident.to_string()).find(|s| !matches!(s.as_str(), "crate" | "self" | "super"));
        // Ours: a project type, or an unknown capitalised name (a type that will be generated).
        first.is_some_and(|f| self.known.paths.contains(&f) || (upper(&f) && !PRELUDE.contains(&f.as_str()) && !self.imported.contains(&f)))
    }
}

impl<'ast> Visit<'ast> for V<'_> {
    fn visit_expr_call(&mut self, c: &'ast syn::ExprCall) {
        if let syn::Expr::Path(p) = &*c.func {
            self.check_path(&p.path, true);
            for a in &c.args {
                self.visit_expr(a);
            }
            return;
        }
        visit::visit_expr_call(self, c);
    }

    fn visit_expr_path(&mut self, p: &'ast syn::ExprPath) {
        self.check_path(&p.path, false);
    }

    fn visit_type_path(&mut self, t: &'ast syn::TypePath) {
        if t.qself.is_none() {
            self.check_path(&t.path, false);
        }
        visit::visit_type_path(self, t);
    }

    fn visit_expr_struct(&mut self, s: &'ast syn::ExprStruct) {
        self.check_path(&s.path, false);
        for f in &s.fields {
            self.visit_expr(&f.expr);
        }
    }

    fn visit_pat_tuple_struct(&mut self, p: &'ast syn::PatTupleStruct) {
        self.check_path(&p.path, false);
        for e in &p.elems {
            self.visit_pat(e);
        }
    }

    fn visit_expr_method_call(&mut self, m: &'ast syn::ExprMethodCall) {
        if self.ours(&m.receiver) {
            let name = m.method.to_string();
            if !self.known.names.contains(&name) {
                self.push(format!(".{name}()"), m.method.span(), m.method.span());
            }
        }
        visit::visit_expr_method_call(self, m);
    }

    // Don't judge paths inside `use`, attributes or macro invocations.
    fn visit_item_use(&mut self, _: &'ast syn::ItemUse) {}
    fn visit_attribute(&mut self, _: &'ast syn::Attribute) {}
    fn visit_macro(&mut self, _: &'ast syn::Macro) {}
}

/// Find references in `src` that `known` doesn't define. None if `src` doesn't parse.
pub fn find(lang: Lang, src: &str, known: &Known) -> Option<Vec<Pending>> {
    match lang {
        Lang::Rust => find_rust(src, known),
        // Java support arrives with tree-sitter; until then nothing is pending.
        Lang::Java => Some(Vec::new()),
    }
}

fn find_rust(src: &str, known: &Known) -> Option<Vec<Pending>> {
    let file = syn::parse_file(src).ok()?;
    // Items defined in this very buffer count as known too.
    let own = crate::items::parse(Lang::Rust, src).unwrap_or_default();
    let mut k = Known { paths: known.paths.clone(), names: known.names.clone() };
    for i in &own {
        k.paths.insert(i.path.clone());
        if let Some(last) = i.path.rsplit("::").next() {
            k.names.insert(last.to_string());
        }
    }
    // Generic parameters and use-imported names are in scope, so they're known too.
    let imported = imported_names(&file);
    let mut v = V { known: &k, imported: &imported, out: Vec::new() };
    v.visit_file(&file);
    v.out.retain(|p| !imported.contains(p.name.split("::").next().unwrap_or("")));
    v.out.sort_by_key(|p| p.start);
    let mut seen = BTreeSet::new();
    v.out.retain(|p| seen.insert((p.name.clone(), p.start)));
    Some(v.out)
}

/// Names brought in by `use` from outside the project (e.g. `use winit::window::Window`).
fn imported_names(file: &syn::File) -> BTreeSet<String> {
    fn walk(t: &syn::UseTree, external: bool, out: &mut BTreeSet<String>) {
        match t {
            syn::UseTree::Path(p) => {
                let root = p.ident.to_string();
                let ext = external || !matches!(root.as_str(), "crate" | "self" | "super");
                walk(&p.tree, ext, out)
            }
            syn::UseTree::Name(n) if external => {
                out.insert(n.ident.to_string());
            }
            syn::UseTree::Rename(r) if external => {
                out.insert(r.rename.to_string());
            }
            syn::UseTree::Group(g) => g.items.iter().for_each(|i| walk(i, external, out)),
            _ => {}
        }
    }
    let mut out = BTreeSet::new();
    for item in &file.items {
        if let syn::Item::Use(u) = item {
            if let syn::UseTree::Path(p) = &u.tree {
                // `use foo::...` where foo is a local `mod foo;` is not external.
                let local_mods: BTreeSet<String> = file.items.iter().filter_map(|i| if let syn::Item::Mod(m) = i { Some(m.ident.to_string()) } else { None }).collect();
                let root = p.ident.to_string();
                let external = !matches!(root.as_str(), "crate" | "self" | "super") && !local_mods.contains(&root);
                walk(&p.tree, external, &mut out);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn known(src: &str) -> Known {
        Known::from_items(&crate::items::parse(Lang::Rust, src).unwrap())
    }

    const VOCAB: &str = "pub struct App; impl App { pub fn new(_: &str) -> Self { App } pub fn window(self) -> W { W } } pub struct W; impl W { pub fn open(self, _: Mode) -> W { self } pub fn run(self) {} } pub enum Mode { Maximized }";

    fn names(src: &str) -> Vec<String> {
        find(Lang::Rust, src, &known(VOCAB)).unwrap().into_iter().map(|p| p.name).collect()
    }

    #[test]
    fn nothing_pending_when_defined() {
        assert!(names("fn main() { App::new(\"x\").window().open(Mode::Maximized).run() }").is_empty());
    }

    #[test]
    fn finds_new_variant_method_and_type() {
        let n = names("fn main() { App::new(\"x\").window().open(Mode::Windowed(1, 2)).fade_in().run(); let t = Theme::Dark; }");
        assert_eq!(n, ["Mode::Windowed", ".fade_in()", "Theme::Dark"]);
    }

    #[test]
    fn ignores_std_and_foreign_chains() {
        assert!(names("use winit::window::Window; fn main() { let v = Vec::new(); let s = String::from(\"a\").trim().len(); let w: Option<Window> = None; v.iter().count(); println!(\"{}\", Unknown::thing()); }").is_empty());
    }

    #[test]
    fn ignores_chains_on_imported_types() {
        assert!(names("use winit::window::Window; fn f() { let a = Window::default_attributes().with_title(\"x\").with_maximized(true); }").is_empty());
    }

    #[test]
    fn positions() {
        let p = find(Lang::Rust, "fn main() {\n    Mode::Windowed(1, 2);\n}", &known(VOCAB)).unwrap();
        assert_eq!(p[0].start, (1, 4));
        assert_eq!(p[0].end, (1, 18));
    }

    #[test]
    fn unparseable_is_none() {
        assert!(find(Lang::Rust, "fn main() { App::new(", &known(VOCAB)).is_none());
    }
}
