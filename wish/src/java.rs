//! Java support: items and "will be generated" references, via tree-sitter.
//!
//! Item paths use `.`: `Window` (type), `Window.open` (method),
//! `Window.<init>` (constructor), `Mode.MAXIMIZED` (enum constant),
//! `Window.title` (field). A method or constructor whose name is overloaded
//! within its type carries its parameter types: `Window.open(int, int)`.
//! Hashes are over whitespace-normalised text, so reformatting never breaks an
//! adoption. Unlike the Rust side, a file with any syntax error yields no
//! items at all rather than a partial guess.

use crate::items::Item;
use crate::pending::{Known, Pending, utf16_col_from_bytes};
use anyhow::{Result, bail};
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, HashMap};
use tree_sitter::{Node, Parser, Tree};

/// The syntax tree, or None if the source has errors.
fn parse_tree(src: &str) -> Option<Tree> {
    let mut parser = Parser::new();
    parser.set_language(&tree_sitter_java::LANGUAGE.into()).ok()?;
    let tree = parser.parse(src, None)?;
    (!tree.root_node().has_error()).then_some(tree)
}

fn text<'a>(n: Node, src: &'a str) -> &'a str {
    &src[n.byte_range()]
}

fn hash(s: &str) -> String {
    let norm = s.split_whitespace().collect::<Vec<_>>().join(" ");
    Sha256::digest(norm.as_bytes()).iter().take(8).map(|b| format!("{b:02x}")).collect()
}

/// Text of the Javadoc comment directly before `n`, without the comment markers.
fn javadoc(n: Node, src: &str) -> String {
    let Some(c) = n.prev_sibling().filter(|c| c.kind() == "block_comment" && text(*c, src).starts_with("/**")) else { return String::new() };
    text(c, src)
        .lines()
        .map(|l| {
            let t = l.trim().trim_start_matches("/**").trim_end_matches("*/").trim();
            t.strip_prefix('*').unwrap_or(t).trim().to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

/// Parse a Java source file into its adoptable items.
pub fn parse(src: &str) -> Result<Vec<Item>> {
    let Some(tree) = parse_tree(src) else { bail!("parsing Java source") };
    let root = tree.root_node();
    let mut out = Vec::new();
    walk(members(root), "", false, src, &mut out);
    Ok(out)
}

/// The declarations directly inside `parent`. An enum keeps its members in a
/// nested `enum_body_declarations` node after the constants; flatten it.
fn members(parent: Node) -> Vec<Node> {
    let mut out = Vec::new();
    let mut cur = parent.walk();
    for n in parent.named_children(&mut cur) {
        if n.kind() == "enum_body_declarations" {
            out.extend(members(n));
        } else {
            out.push(n);
        }
    }
    out
}

/// Parameter types of a method or constructor, e.g. `int, List<String>`.
fn param_types(n: Node, src: &str) -> String {
    let Some(params) = n.child_by_field_name("parameters") else { return String::new() };
    let mut cur = params.walk();
    params
        .named_children(&mut cur)
        .filter_map(|p| match p.kind() {
            "formal_parameter" => p.child_by_field_name("type").map(|t| text(t, src).to_string()),
            "spread_parameter" => p.named_children(&mut p.walk()).find(|c| !matches!(c.kind(), "modifiers" | "variable_declarator")).map(|t| format!("{}...", text(t, src))),
            _ => None,
        })
        .map(|t| t.split_whitespace().collect::<Vec<_>>().join(" "))
        .collect::<Vec<_>>()
        .join(", ")
}

fn is_public(n: Node) -> bool {
    n.children(&mut n.walk()).find(|c| c.kind() == "modifiers").is_some_and(|m| m.children(&mut m.walk()).any(|c| c.kind() == "public"))
}

fn walk(nodes: Vec<Node>, prefix: &str, in_interface: bool, src: &str, out: &mut Vec<Item>) {
    // Names declared more than once in this type are overloads.
    let mut count: HashMap<String, usize> = HashMap::new();
    for n in &nodes {
        if let Some(name) = callable_name(*n, src) {
            *count.entry(name).or_default() += 1;
        }
    }
    for n in nodes {
        let kind: &'static str = match n.kind() {
            "class_declaration" => "class",
            "interface_declaration" => "interface",
            "enum_declaration" => "enum",
            "record_declaration" => "record",
            "method_declaration" => "method",
            "constructor_declaration" => "constructor",
            "field_declaration" => "field",
            "enum_constant" => "variant",
            _ => continue,
        };
        let is_type = matches!(kind, "class" | "interface" | "enum" | "record");
        let public = is_public(n) || in_interface || kind == "variant";
        let docs = javadoc(n, src);
        let body = n.child_by_field_name("body");
        let head = &src[n.start_byte()..body.map_or(n.end_byte(), |b| b.start_byte())];
        let full = hash(&format!("{}\u{0}{docs}", text(n, src)));
        let sig = (public && matches!(kind, "class" | "interface" | "enum" | "record" | "method" | "constructor")).then(|| hash(head));
        // One item per name: a field declaration may declare several.
        let names: Vec<(String, Node)> = match kind {
            "field" => n.children_by_field_name("declarator", &mut n.walk()).filter_map(|d| d.child_by_field_name("name")).map(|nm| (text(nm, src).to_string(), nm)).collect(),
            "constructor" => n.child_by_field_name("name").map(|nm| ("<init>".to_string(), nm)).into_iter().collect(),
            _ => n.child_by_field_name("name").map(|nm| (text(nm, src).to_string(), nm)).into_iter().collect(),
        };
        for (name, name_node) in names {
            let overloaded = callable_name(n, src).is_some_and(|c| count[&c] > 1);
            let path = if overloaded { format!("{prefix}{name}({})", param_types(n, src)) } else { format!("{prefix}{name}") };
            out.push(Item {
                path: path.clone(),
                kind,
                hash: full.clone(),
                docs: docs.clone(),
                line: name_node.start_position().row + 1,
                end_line: n.end_position().row + 1,
                sig_hash: sig.clone(),
            });
            if let (true, Some(b)) = (is_type, body) {
                walk(members(b), &format!("{path}."), n.kind() == "interface_declaration", src, out);
            }
        }
    }
}

/// The name a method or constructor is overloaded by.
fn callable_name(n: Node, src: &str) -> Option<String> {
    match n.kind() {
        "constructor_declaration" => Some("<init>".into()),
        "method_declaration" => n.child_by_field_name("name").map(|nm| text(nm, src).to_string()),
        _ => None,
    }
}

/// Names that need no import or definition: `java.lang` plus a few JDK
/// types that are common enough that flagging them would only be noise.
const JAVA_LANG: &[&str] = &[
    "String", "System", "Math", "Integer", "Long", "Double", "Float", "Boolean", "Character", "Byte", "Short", "Object", "Thread", "Runnable",
    "Exception", "RuntimeException", "Error", "Throwable", "StringBuilder", "Iterable", "Comparable", "Override", "Deprecated", "Record", "Enum",
    "Class", "Number", "Void", "List", "Map", "Set", "Optional", "Arrays", "Collections", "Objects", "ArrayList", "HashMap", "HashSet",
    "IllegalArgumentException", "IllegalStateException", "AutoCloseable", "CharSequence", "Process", "Runtime", "StackWalker", "SafeVarargs",
    "FunctionalInterface", "SuppressWarnings", "Collectors", "Stream", "Files", "Paths", "Path", "Duration", "Instant", "LocalDate", "LocalDateTime",
];

fn upper(s: &str) -> bool {
    s.chars().next().is_some_and(|c| c.is_uppercase())
}

/// Names the file brings in itself: single-type imports, and type parameters
/// (`<T>`). ponytail: wildcard imports are not resolved, so a name that only a
/// wildcard provides shows as wished; javac's own errors stay the backstop.
fn declared_names(root: Node, src: &str) -> BTreeSet<String> {
    fn type_params(n: Node, src: &str, out: &mut BTreeSet<String>) {
        if let (true, Some(id)) = (n.kind() == "type_parameter", n.named_child(0)) {
            out.insert(text(id, src).to_string());
        }
        let mut cur = n.walk();
        for c in n.children(&mut cur) {
            type_params(c, src, out);
        }
    }
    let mut out = BTreeSet::new();
    let mut cur = root.walk();
    for n in root.named_children(&mut cur).filter(|n| n.kind() == "import_declaration") {
        let t = text(n, src).trim().trim_end_matches(';');
        if let Some(last) = t.rsplit('.').next().map(str::trim).filter(|l| *l != "*") {
            out.insert(last.to_string());
        }
    }
    type_params(root, src, &mut out);
    out
}

struct V<'a> {
    src: &'a str,
    lines: Vec<&'a str>,
    known: &'a Known,
    declared: BTreeSet<String>,
    out: Vec<Pending>,
}

impl V<'_> {
    fn push(&mut self, name: String, a: Node, b: Node) {
        let (s, e) = (a.start_position(), b.end_position());
        let col = |row: usize, byte: usize| utf16_col_from_bytes(self.lines.get(row).copied().unwrap_or(""), byte);
        let (start, end) = ((s.row as u32, col(s.row, s.column)), (e.row as u32, col(e.row, e.column)));
        self.out.push(Pending { name, start, end });
    }

    fn text(&self, n: Node) -> &str {
        text(n, self.src)
    }

    fn external(&self, t: &str) -> bool {
        JAVA_LANG.contains(&t) || self.declared.contains(t)
    }

    /// A capitalised name that is neither external nor already defined: a
    /// type that will be generated.
    fn wished_type(&self, t: &str) -> bool {
        upper(t) && !self.external(t) && !self.known.has_path(t) && !self.known.has_name(t)
    }

    /// Does this expression chain start at one of our (existing or pending) types?
    fn rooted(&self, n: Node) -> bool {
        match n.kind() {
            "method_invocation" | "field_access" => n.child_by_field_name("object").is_some_and(|o| self.rooted(o)),
            "parenthesized_expression" => n.named_child(0).is_some_and(|c| self.rooted(c)),
            "object_creation_expression" => n.child_by_field_name("type").is_some_and(|t| {
                let name = self.text(t);
                upper(name) && !self.external(name)
            }),
            "identifier" => {
                let t = self.text(n);
                self.known.has_path(t) || (upper(t) && !self.external(t))
            }
            _ => false,
        }
    }

    /// `X` in `X.y`: a capitalised identifier that is not external.
    fn type_ref(&self, n: Node) -> bool {
        n.kind() == "identifier" && upper(self.text(n)) && !self.external(self.text(n))
    }

    fn visit(&mut self, n: Node) {
        match n.kind() {
            "import_declaration" | "package_declaration" | "annotation" | "marker_annotation" | "line_comment" | "block_comment" => return,
            "type_identifier" => {
                // The tail of a qualified name (`Map.Entry`, `java.awt.Frame`) is judged with its head.
                let qualified_tail = n.parent().is_some_and(|p| p.kind() == "scoped_type_identifier") && n.prev_named_sibling().is_some();
                let t = self.text(n);
                if !qualified_tail && self.wished_type(t) {
                    self.push(t.to_string(), n, n);
                }
            }
            "method_invocation" => {
                if let Some(name) = n.child_by_field_name("name") {
                    match n.child_by_field_name("object") {
                        Some(o) if self.type_ref(o) => {
                            let full = format!("{}.{}", self.text(o), self.text(name));
                            if !self.known.has_path(&full) {
                                self.push(full, o, name);
                            }
                        }
                        Some(o) if self.rooted(o) => {
                            let m = self.text(name);
                            if !self.known.has_name(m) {
                                self.push(format!(".{m}()"), name, name);
                            }
                        }
                        _ => {}
                    }
                }
            }
            "field_access" => {
                if let (Some(o), Some(f)) = (n.child_by_field_name("object"), n.child_by_field_name("field")) {
                    let full = format!("{}.{}", self.text(o), self.text(f));
                    if self.type_ref(o) && !self.known.has_path(&full) {
                        self.push(full, o, f);
                    }
                }
            }
            _ => {}
        }
        let mut cur = n.walk();
        for c in n.children(&mut cur) {
            self.visit(c);
        }
    }
}

/// Find references in `src` that `known` doesn't define. None if `src` doesn't parse.
pub fn find_pending(src: &str, known: &Known) -> Option<Vec<Pending>> {
    let tree = parse_tree(src)?;
    let root = tree.root_node();
    // Items defined in this very buffer count as known too.
    let mut k = known.clone();
    k.add(&parse(src).unwrap_or_default());
    let mut v = V { src, lines: src.lines().collect(), known: &k, declared: declared_names(root, src), out: Vec::new() };
    v.visit(root);
    v.out.sort_by_key(|p| p.start);
    let mut seen = BTreeSet::new();
    v.out.retain(|p| seen.insert((p.name.clone(), p.start)));
    Some(v.out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = "package demo;
/**
 * An app.
 * <h2>Mocked</h2>
 * <ul><li>fake</li></ul>
 */
public class App {
    private int n, m;
    /** Makes one. */
    public App() {}
    public App(String name) {}
    /** Opens. */
    public Win window() { return null; }
    public void say(int x) {}
    public void say(String s, java.util.List<String> more) {}
    enum Mode { A, B; void f() {} }
    record P(int x) {}
}
";

    fn get<'a>(items: &'a [Item], path: &str) -> &'a Item {
        items.iter().find(|i| i.path == path).unwrap()
    }

    #[test]
    fn finds_items() {
        let items = parse(SRC).unwrap();
        let paths: Vec<_> = items.iter().map(|i| i.path.as_str()).collect();
        assert_eq!(
            paths,
            ["App", "App.n", "App.m", "App.<init>()", "App.<init>(String)", "App.window", "App.say(int)", "App.say(String, java.util.List<String>)", "App.Mode", "App.Mode.A", "App.Mode.B", "App.Mode.f", "App.P"]
        );
        assert_eq!(get(&items, "App.window").docs, "Opens.");
        assert_eq!(get(&items, "App").mocked().unwrap(), ["fake"]);
        assert_eq!((get(&items, "App.window").line, get(&items, "App.window").end_line), (13, 13));
        assert!(get(&items, "App.window").sig_hash.is_some() && get(&items, "App.n").sig_hash.is_none() && get(&items, "App.Mode").sig_hash.is_none());
    }

    #[test]
    fn hash_ignores_formatting_but_not_docs_or_signature() {
        let a = parse(SRC).unwrap();
        let b = parse(&SRC.replace("public Win window() { return null; }", "public  Win window() {\n  return null;\n }")).unwrap();
        assert_eq!(get(&a, "App.window").hash, get(&b, "App.window").hash);
        let c = parse(&SRC.replace("Opens.", "Opens it.")).unwrap();
        assert_ne!(get(&a, "App.window").hash, get(&c, "App.window").hash);
        assert_eq!(get(&a, "App.window").sig_hash, get(&c, "App.window").sig_hash);
        let d = parse(&SRC.replace("public Win window()", "public Win window(int x)")).unwrap();
        assert_ne!(get(&a, "App.window").sig_hash, get(&d, "App.window").sig_hash);
    }

    #[test]
    fn unparseable_has_no_items() {
        assert!(parse("public class App { void f( }").is_err());
        assert!(find_pending("public class App { void f( }", &Known::default()).is_none());
    }

    fn known(src: &str) -> Known {
        Known::from_items(&parse(src).unwrap())
    }

    const VOCAB: &str = "class App { static App create(String n) { return null; } Win window() { return null; } void say(int x) {} void say(String s) {} } class Win { Win open(int w) { return this; } }";

    fn names(src: &str) -> Vec<String> {
        find_pending(src, &known(VOCAB)).unwrap().into_iter().map(|p| p.name).collect()
    }

    #[test]
    fn nothing_pending_when_defined() {
        assert!(names("class M { void f() { App.create(\"x\").window().open(1); } }").is_empty());
    }

    #[test]
    fn finds_types_members_and_chains() {
        let n = names("class M { void f() { App.make(\"x\").window().open(1).fade(); Theme t = new Theme(); Mode.DARK.x(); } }");
        assert_eq!(n, ["App.make", ".fade()", "Theme", "Theme", "Mode.DARK", ".x()"]);
        // overloads count as defined
        assert!(names("class M { void f() { App.create(\"x\").say(1); } }").is_empty());
    }

    #[test]
    fn skips_imports_and_well_known_names() {
        assert!(names("package demo; import java.awt.Color; import static java.lang.Math.max; class M { void f() { Color c = Color.RED; String s = String.valueOf(1); List<String> l = new ArrayList<>(); System.out.println(Math.max(1, 2)); Map.Entry<String, String> e = null; java.awt.Frame fr = null; } <T> T g(T t) { return t; } }").is_empty());
    }

    #[test]
    fn positions_are_utf16() {
        // The emoji is 4 bytes in UTF-8 and two UTF-16 units.
        let p = find_pending("class M {\n    String s = \"😀\"; Theme t;\n}", &known(VOCAB)).unwrap();
        assert_eq!(p[0].name, "Theme");
        assert_eq!((p[0].start, p[0].end), ((1, 21), (1, 26)));
    }
}
