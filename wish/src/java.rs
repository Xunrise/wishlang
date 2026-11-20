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
use std::collections::{BTreeMap, BTreeSet, HashMap};
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

/// Is type `t` (maybe qualified, `WebClient.Builder`) named through an import or `java.lang`?
fn from_outside(t: &str, imported: &BTreeSet<String>) -> bool {
    let head = t.split('.').next().unwrap_or(t);
    imported.contains(head) || JAVA_LANG.contains(&head)
}

/// Methods every object has.
const OBJECT_METHODS: &[&str] = &["getClass", "hashCode", "equals", "toString", "wait", "notify", "notifyAll", "clone", "finalize"];

/// Parse a Java source file into its adoptable items.
pub fn parse(src: &str) -> Result<Vec<Item>> {
    let Some(tree) = parse_tree(src) else { bail!("parsing Java source") };
    let root = tree.root_node();
    let mut out = Vec::new();
    let imported = declared_names(root, src);
    walk(members(root), "", false, src, &mut out);
    for i in &mut out {
        if let Some(t) = i.ty.as_mut().filter(|t| from_outside(t, &imported)) {
            *t = format!("import:{t}");
        }
    }
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
    let first = out.len();
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
            "interface_declaration" | "annotation_type_declaration" => "interface",
            "enum_declaration" => "enum",
            "record_declaration" => "record",
            "method_declaration" => "method",
            "constructor_declaration" => "constructor",
            "field_declaration" | "constant_declaration" => "field",
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
        let ty = matches!(kind, "method" | "field").then(|| n.child_by_field_name("type")).flatten().map(|t| type_head(t, src));
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
                ty: ty.clone(),
            });
            if let (true, Some(b)) = (is_type, body) {
                walk(members(b), &format!("{path}."), matches!(n.kind(), "interface_declaration" | "annotation_type_declaration"), src, out);
            }
        }
    }
    // A field sharing its path with a method, constructor or type (`private String subtitle;` and
    // `AppWindow subtitle(String)`) gets `#field`, so the path keeps meaning the callable.
    let taken: Vec<String> = out[first..].iter().filter(|i| i.kind != "field").map(|i| i.path.clone()).collect();
    for i in out[first..].iter_mut().filter(|i| i.kind == "field" && taken.contains(&i.path)) {
        i.path.push_str("#field");
    }
}

/// The type name of a type node without its arguments: `List` for `List<Code>`, `Map.Entry` for `Map.Entry<K, V>`, `[]` for arrays.
fn type_head(n: Node, src: &str) -> String {
    match n.kind() {
        "generic_type" => n.named_child(0).map_or_else(String::new, |t| type_head(t, src)),
        "scoped_type_identifier" => {
            let mut cur = n.walk();
            n.named_children(&mut cur).map(|c| type_head(c, src)).collect::<Vec<_>>().join(".")
        }
        "array_type" => "[]".into(),
        _ => text(n, src).to_string(),
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

/// Names that need no import or definition: every public top-level type of `java.lang`
/// (as of JDK 25), plus a few JDK types common enough that flagging them would only be noise.
const JAVA_LANG: &[&str] = &[
    "AbstractMethodError", "Appendable", "ArithmeticException", "ArrayIndexOutOfBoundsException", "ArrayStoreException", "AssertionError",
    "AutoCloseable", "Boolean", "BootstrapMethodError", "Byte", "CharSequence", "Character", "Class", "ClassCastException",
    "ClassCircularityError", "ClassFormatError", "ClassLoader", "ClassNotFoundException", "ClassValue", "CloneNotSupportedException",
    "Cloneable", "Comparable", "Deprecated", "Double", "Enum", "EnumConstantNotPresentException", "Error", "Exception",
    "ExceptionInInitializerError", "Float", "FunctionalInterface", "IO", "IllegalAccessError", "IllegalAccessException",
    "IllegalArgumentException", "IllegalCallerException", "IllegalMonitorStateException", "IllegalStateException",
    "IllegalThreadStateException", "IncompatibleClassChangeError", "IndexOutOfBoundsException", "InheritableThreadLocal",
    "InstantiationError", "InstantiationException", "Integer", "InternalError", "InterruptedException", "Iterable",
    "LayerInstantiationException", "LinkageError", "Long", "MatchException", "Math", "Module", "ModuleLayer", "NegativeArraySizeException",
    "NoClassDefFoundError", "NoSuchFieldError", "NoSuchFieldException", "NoSuchMethodError", "NoSuchMethodException",
    "NullPointerException", "Number", "NumberFormatException", "Object", "OutOfMemoryError", "Override", "Package", "Process",
    "ProcessBuilder", "ProcessHandle", "Readable", "Record", "ReflectiveOperationException", "Runnable", "Runtime", "RuntimeException",
    "RuntimePermission", "SafeVarargs", "ScopedValue", "SecurityException", "SecurityManager", "Short", "StableValue", "StackOverflowError",
    "StackTraceElement", "StackWalker", "StrictMath", "String", "StringBuffer", "StringBuilder", "StringIndexOutOfBoundsException",
    "SuppressWarnings", "System", "Thread", "ThreadDeath", "ThreadGroup", "ThreadLocal", "Throwable", "TypeNotPresentException",
    "UnknownError", "UnsatisfiedLinkError", "UnsupportedClassVersionError", "UnsupportedOperationException", "VerifyError",
    "VirtualMachineError", "Void", "WrongThreadException",
    // not java.lang, but too common to flag
    "List", "Map", "Set", "Optional", "Arrays", "Collections", "Objects", "ArrayList", "HashMap", "HashSet", "Collectors", "Stream",
    "Files", "Paths", "Path", "Duration", "Instant", "LocalDate", "LocalDateTime",
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

/// Single-type imports (simple name -> package) and wildcard-imported packages, static ones aside.
fn imports(root: Node, src: &str) -> (BTreeMap<String, String>, Vec<String>) {
    let (mut named, mut wild) = (BTreeMap::new(), Vec::new());
    let mut cur = root.walk();
    for n in root.named_children(&mut cur).filter(|n| n.kind() == "import_declaration") {
        let t = text(n, src).trim().trim_start_matches("import").trim().trim_end_matches(';').trim();
        if t.starts_with("static ") {
            continue;
        }
        match t.rsplit_once('.') {
            Some((pkg, "*")) => wild.push(pkg.trim().to_string()),
            Some((pkg, name)) => {
                named.insert(name.trim().to_string(), pkg.trim().to_string());
            }
            None => {}
        }
    }
    (named, wild)
}

struct V<'a> {
    src: &'a str,
    /// Simple names imported from libraries (not from the project's own packages).
    foreign: BTreeSet<String>,
    /// A wildcard import from a library: any unknown name may come from it.
    foreign_wildcard: bool,
    lines: Vec<&'a str>,
    known: &'a Known,
    /// What this buffer itself declares: its own fields win over same-named ones elsewhere.
    own: Known,
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
        JAVA_LANG.contains(&t) || self.declared.contains(t) || (self.foreign_wildcard && !self.known.is_type(t) && !self.known.has_path(t))
    }

    /// Does type `t` extend or implement something that isn't the project's? Then it has members
    /// no project file declares.
    fn inherits_foreign(&self, t: &str) -> bool {
        let t = t.strip_prefix("import:").unwrap_or(t);
        let sups = self.known.types_at(&format!("{t}.{}", crate::pending::SUPER));
        sups.is_some_and(|sups| sups.iter().map(|s| s.strip_prefix("import:").unwrap_or(s)).any(|s| !self.known.is_type(s) || self.foreign.contains(s)))
    }

    /// Paths of the type declarations around `n`, innermost first (`Outer.Inner`, `Outer`).
    fn enclosing(&self, n: Node) -> Vec<String> {
        let mut names = Vec::new();
        let mut at = n.parent();
        while let Some(p) = at {
            if matches!(p.kind(), "class_declaration" | "interface_declaration" | "enum_declaration" | "record_declaration") {
                names.extend(p.child_by_field_name("name").map(|nm| self.text(nm).to_string()));
            }
            at = p.parent();
        }
        (0..names.len()).map(|i| names[i..].iter().rev().cloned().collect::<Vec<_>>().join(".")).collect()
    }

    /// Is the expression of a type that may have members from a library?
    fn open(&self, n: Node) -> bool {
        self.types_of_expr(n).is_some_and(|ts| ts.iter().any(|t| self.inherits_foreign(t)))
    }

    /// Does `t` have supertypes at all (so a member may be inherited)?
    fn has_supers(&self, t: &str) -> bool {
        let t = t.strip_prefix("import:").unwrap_or(t);
        self.known.types_at(&format!("{t}.{}", crate::pending::SUPER)).is_some()
    }

    /// The declared types an expression may have, where the project tells.
    fn types_of_expr(&self, n: Node) -> Option<BTreeSet<String>> {
        match n.kind() {
            "identifier" => {
                let t = self.text(n);
                if self.known.is_type(t) {
                    return Some([t.to_string()].into());
                }
                self.own.types_of(t).or_else(|| self.known.types_of(t)).cloned()
            }
            "method_invocation" | "field_access" => {
                let m = self.text(n.child_by_field_name(if n.kind() == "field_access" { "field" } else { "name" })?);
                let at = n.child_by_field_name("object").filter(|o| o.kind() == "identifier").and_then(|o| self.known.types_at(&format!("{}.{m}", self.text(o))));
                at.or_else(|| self.own.types_of(m)).or_else(|| self.known.types_of(m)).cloned()
            }
            "object_creation_expression" => n.child_by_field_name("type").map(|t| [type_head(t, self.src)].into()),
            "parenthesized_expression" => self.types_of_expr(n.named_child(0)?),
            _ => None,
        }
    }

    /// A capitalised name that is neither external nor already defined: a
    /// type that will be generated.
    /// A known field or method name that is not a type, like `LOGGER` or `ALL`: a value, whatever its case.
    fn value(&self, t: &str) -> bool {
        !self.known.is_type(t) && self.known.types_of(t).is_some()
    }

    fn wished_type(&self, t: &str) -> bool {
        upper(t) && !self.external(t) && !self.known.has_path(t) && !self.known.has_name(t)
    }

    /// Is `t` one of our types, existing or wished (not java.lang, not imported from a library)?
    fn ours(&self, t: &str) -> bool {
        match t.strip_prefix("import:") {
            Some(name) => !self.foreign.contains(name.split('.').next().unwrap_or(name)) && self.known.has_path_or_tail(name),
            None => self.known.has_path_or_tail(t) || (upper(t) && !self.external(t.split('.').next().unwrap_or(t))),
        }
    }

    /// Does the member `name` (on `object`, if given) yield one of our types? Its declared type is looked up
    /// by exact path (`Codes.VALUES`), else among this file's own members, else by name anywhere. Unknown
    /// members (still to be generated) do; a known one only if some declaration has one of our types. So
    /// `Codes.ALL.stream()` stops at `ALL`'s `List`, while `App.create().window()` stays on our types.
    fn yields_ours(&self, name: Node, object: Option<Node>) -> bool {
        let m = self.text(name);
        let at = match object {
            Some(o) => (o.kind() == "identifier").then(|| self.known.types_at(&format!("{}.{m}", self.text(o)))).flatten(),
            None => self.enclosing(name).iter().find_map(|t| self.own.types_at(&format!("{t}.{m}"))),
        };
        at.or_else(|| self.own.types_of(m)).or_else(|| self.known.types_of(m)).is_none_or(|ts| ts.iter().any(|t| self.ours(t)))
    }

    /// Does this expression chain start at one of our (existing or pending) types, and stay on them?
    fn rooted(&self, n: Node) -> bool {
        match n.kind() {
            "method_invocation" | "field_access" => {
                let object = n.child_by_field_name("object");
                let member = n.child_by_field_name(if n.kind() == "field_access" { "field" } else { "name" });
                // A member the project doesn't declare, on a type with a library supertype, is the library's.
                let inherited = object.zip(member).is_some_and(|(o, m)| !self.known.has_name(self.text(m)) && self.open(o));
                object.is_some_and(|o| self.rooted(o)) && !inherited && member.is_some_and(|m| self.yields_ours(m, object))
            }
            "parenthesized_expression" => n.named_child(0).is_some_and(|c| self.rooted(c)),
            "object_creation_expression" => n.child_by_field_name("type").is_some_and(|t| {
                let name = type_head(t, self.src);
                let head = name.split('.').next().unwrap_or(&name);
                upper(head) && !self.external(head) && !self.foreign.contains(head)
            }),
            "identifier" => {
                let t = self.text(n);
                if self.foreign.contains(t) {
                    return false;
                }
                if (upper(t) && self.value(t)) || self.own.types_of(t).is_some() {
                    // a constant like `LOGGER`, or a field this file declares (`stagingPort`); other
                    // lowercase names are locals and parameters, which we can't type
                    self.yields_ours(n, None)
                } else {
                    self.known.has_path_or_tail(t) || (upper(t) && !self.external(t))
                }
            }
            _ => false,
        }
    }

    /// `T.m` that `T` may inherit: from a library supertype, or from a project one declaring an `m`.
    fn maybe_inherited(&self, t: &str, m: &str) -> bool {
        self.inherits_foreign(t) || (self.has_supers(t) && self.known.has_name(m))
    }

    /// `X` in `X.y`: a capitalised identifier that is not external.
    /// A project type stays ours when imported from another package.
    fn type_ref(&self, n: Node) -> bool {
        let t = self.text(n);
        n.kind() == "identifier" && upper(t) && !self.foreign.contains(t) && (self.known.is_type(t) || !self.external(t)) && !self.value(t)
    }

    fn visit(&mut self, n: Node) {
        match n.kind() {
            "import_declaration" | "package_declaration" | "annotation" | "marker_annotation" | "line_comment" | "block_comment" => return,
            "type_identifier" => {
                // The tail of a qualified name (`Map.Entry`, `java.awt.Frame`) is judged with its head.
                let qualified_tail = n.parent().is_some_and(|p| p.kind() == "scoped_type_identifier") && n.prev_named_sibling().is_some();
                let t = self.text(n);
                // Inside a type with a library supertype, a name may be its nested type (`Map.Entry` as `Entry`).
                let inherited = || self.enclosing(n).iter().any(|e| self.inherits_foreign(e.rsplit('.').next().unwrap_or(e)));
                if !qualified_tail && self.wished_type(t) && !inherited() {
                    self.push(t.to_string(), n, n);
                }
            }
            "method_invocation" => {
                if let Some(name) = n.child_by_field_name("name") {
                    match n.child_by_field_name("object") {
                        Some(o) if self.type_ref(o) => {
                            let full = format!("{}.{}", self.text(o), self.text(name));
                            if !self.known.has_path_or_tail(&full) && !self.maybe_inherited(self.text(o), self.text(name)) {
                                self.push(full, o, name);
                            }
                        }
                        Some(o) if self.rooted(o) => {
                            let m = self.text(name);
                            if !self.known.has_name(m) && !self.open(o) && !OBJECT_METHODS.contains(&m) {
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
                    let special = matches!(self.text(f), "this" | "class" | "super");
                    if !special && self.type_ref(o) && !self.known.has_path_or_tail(&full) && !self.maybe_inherited(self.text(o), self.text(f)) {
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

/// The file's `package`, if it declares one.
fn package_of(root: Node, src: &str) -> Option<String> {
    let mut cur = root.walk();
    let decl = root.named_children(&mut cur).find(|n| n.kind() == "package_declaration")?;
    Some(text(decl, src).trim().trim_start_matches("package").trim().trim_end_matches(';').trim().to_string())
}

/// Simple names of what a type declaration extends and implements.
fn supertypes(decl: Node, src: &str) -> Vec<String> {
    fn heads(n: Node, src: &str, out: &mut Vec<String>) {
        match n.kind() {
            "type_identifier" | "generic_type" | "scoped_type_identifier" => out.push(type_head(n, src)),
            _ => {
                let mut cur = n.walk();
                for c in n.named_children(&mut cur) {
                    heads(c, src, out);
                }
            }
        }
    }
    let mut out = Vec::new();
    let mut cur = decl.walk();
    for c in decl.named_children(&mut cur).filter(|c| matches!(c.kind(), "superclass" | "super_interfaces" | "extends_interfaces")) {
        heads(c, src, &mut out);
    }
    out
}

/// Members Java declares without an item of their own: a record's component accessors and an
/// enum's `values()` / `valueOf(String)`, as (path, declared type).
pub fn implicit_members(src: &str) -> Vec<(String, Option<String>)> {
    fn go(n: Node, prefix: &str, src: &str, out: &mut Vec<(String, Option<String>)>) {
        let mut cur = n.walk();
        for c in n.named_children(&mut cur) {
            let kind = c.kind();
            if !matches!(kind, "class_declaration" | "interface_declaration" | "annotation_type_declaration" | "enum_declaration" | "record_declaration") {
                continue;
            }
            let Some(name) = c.child_by_field_name("name").map(|nm| text(nm, src).to_string()) else { continue };
            let path = format!("{prefix}{name}");
            for sup in supertypes(c, src) {
                out.push((format!("{path}.{}", crate::pending::SUPER), Some(sup)));
            }
            if kind == "enum_declaration" {
                out.push((format!("{path}.values"), Some("[]".into())));
                out.push((format!("{path}.valueOf"), Some(name.clone())));
            }
            if let (true, Some(params)) = (kind == "record_declaration", c.child_by_field_name("parameters")) {
                let mut pc = params.walk();
                for p in params.named_children(&mut pc).filter(|p| p.kind() == "formal_parameter") {
                    if let (Some(nm), Some(t)) = (p.child_by_field_name("name"), p.child_by_field_name("type")) {
                        out.push((format!("{path}.{}", text(nm, src)), Some(type_head(t, src))));
                    }
                }
            }
            if let Some(b) = c.child_by_field_name("body") {
                go(b, &format!("{path}."), src, out);
                if let Some(decls) = (0..b.named_child_count()).filter_map(|i| b.named_child(i)).find(|d| d.kind() == "enum_body_declarations") {
                    go(decls, &format!("{path}."), src, out);
                }
            }
        }
    }
    let mut out = Vec::new();
    if let Some(tree) = parse_tree(src) {
        if let Some(pkg) = package_of(tree.root_node(), src) {
            out.push((crate::pending::PACKAGE.to_string(), Some(pkg)));
        }
        go(tree.root_node(), "", src, &mut out);
        let imported = declared_names(tree.root_node(), src);
        for t in out.iter_mut().filter_map(|(_, t)| t.as_mut()).filter(|t| from_outside(t, &imported)) {
            *t = format!("import:{t}");
        }
    }
    out
}

/// Find references in `src` that `known` doesn't define. None if `src` doesn't parse.
pub fn find_pending(src: &str, known: &Known) -> Option<Vec<Pending>> {
    let tree = parse_tree(src)?;
    let root = tree.root_node();
    // Items defined in this very buffer count as known too.
    let mut own = Known::from_items(&parse(src).unwrap_or_default());
    for (path, ty) in implicit_members(src) {
        own.add_member(&path, ty.as_deref());
    }
    let mut k = known.clone();
    k.merge(&own);
    let (named, wild) = imports(root, src);
    let library = |pkg: &str| k.is_project_package(pkg) == Some(false);
    let foreign = named.into_iter().filter(|(_, pkg)| library(pkg)).map(|(n, _)| n).collect();
    let foreign_wildcard = wild.iter().any(|pkg| library(pkg));
    let mut v = V { src, foreign, foreign_wildcard, lines: src.lines().collect(), known: &k, own, declared: declared_names(root, src), out: Vec::new() };
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
    fn a_field_sharing_a_methods_path_is_set_apart() {
        let items = parse("class AppWindow { private String subtitle; private int size; AppWindow subtitle(String s) { return this; } }").unwrap();
        let paths: Vec<_> = items.iter().map(|i| (i.path.as_str(), i.kind)).collect();
        assert_eq!(paths, [("AppWindow", "class"), ("AppWindow.subtitle#field", "field"), ("AppWindow.size", "field"), ("AppWindow.subtitle", "method")]);
    }

    #[test]
    fn unparseable_has_no_items() {
        assert!(parse("public class App { void f( }").is_err());
        assert!(find_pending("public class App { void f( }", &Known::default()).is_none());
    }

    /// Shapes from real codebases (Spring, Shiro, commons-lang) that exist and must not show as wished.
    #[test]
    fn existing_library_and_inherited_code_is_not_wished() {
        let files = [
            "package app.model;\nimport org.springframework.data.jpa.repository.JpaRepository;\npublic interface OwnerRepository extends JpaRepository<Owner, Integer> { }",
            "package app.model;\npublic class Owner { }",
            "package app.codec;\npublic class Base64 { public static byte[] decode(String s) { return null; } }",
            "package app.fmt;\npublic interface ModularCryptFormat { String TOKEN_DELIMITER = \"$\"; }",
            "package app.aop;\npublic class Handler { public static final class Policy { public static boolean isStrict() { return false; } } }",
            "package app.ann;\nimport java.lang.annotation.*;\n@Retention(RetentionPolicy.RUNTIME)\npublic @interface Exclude { }",
            "package app.cache;\npublic interface Cache<K, V> { V get(K k); }",
            "package app.cfg;\npublic class Builder { public Builder build() { return this; } public Builder get() { return this; } }",
        ];
        let mut k = Known::default();
        for f in files {
            k.add(&parse(f).unwrap());
            for (path, ty) in implicit_members(f) {
                k.add_member(&path, ty.as_deref());
            }
        }
        let n = |src: &str| find_pending(src, &k).unwrap().into_iter().map(|p| p.name).collect::<Vec<_>>();
        let web = "package app.web;\nimport app.model.OwnerRepository;\nimport app.model.Owner;\nimport java.util.Base64;\nimport java.util.*;\nimport app.ann.Exclude;\nimport app.aop.Handler.Policy;\n\
            class R implements Collector<String, String, String> {\n\
                private final OwnerRepository owners;\n\
                @Exclude Owner find(int id) { return owners.findById(id).orElseThrow(); }\n\
                boolean skip(java.lang.reflect.Field f) { return f.isAnnotationPresent(Exclude.class); }\n\
                String enc() { return Base64.getEncoder().encodeToString(new byte[0]) + app.fmt.ModularCryptFormat.TOKEN_DELIMITER; }\n\
                boolean strict() { return Policy.isStrict(); }\n\
                Set<Characteristics> characteristics() { Map<String, LinkedHashMap<String, String>> m = null; return null; }\n\
                Object self() { return R.this; }\n\
            }";
        assert_eq!(n(web), Vec::<String>::new());
        let more = "package app.web;\nimport java.util.List;\nimport java.util.Map;\nimport javax.cache.Cache;\nimport org.springframework.web.reactive.function.client.WebClient;\n\
            public record Details(List<Integer> pets) {\n\
                List<Integer> ids() { return pets.stream().map(x -> x).toList(); }\n\
                static class DetailsBuilder { DetailsBuilder pets(List<Integer> p) { return this; } }\n\
            }\n\
            class Section implements Map<String, String> { java.util.Set<Entry<String, String>> entries() { return null; } }\n\
            class C { Cache<String, String> cache; WebClient.Builder web; Object f(Object o) { web.build().get().uri(\"/\"); return cache.getAndRemove(\"k\") + o.getClass().getName(); } }";
        assert_eq!(n(more), Vec::<String>::new());
        // wishes still show next to all that
        assert_eq!(n("package app.web;\nimport app.model.*;\nclass W { void f(OwnerRepository o) { new Dunning(o).run(); } }"), ["Dunning", ".run()"]);
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
    fn a_chain_stops_at_a_library_type() {
        let vocab = "class Codes { static final List<Code> ALL = null; static Optional<Code> find(String c) { return null; } static Code[] arr() { return null; } static Code first() { return null; } } record Code(String code) {}";
        let n = |src: &str| find_pending(src, &known(vocab)).unwrap().into_iter().map(|p| p.name).collect::<Vec<_>>();
        assert!(n("class M { void f() { Codes.ALL.stream().filter(c -> true).findFirst().orElseThrow(); } }").is_empty());
        assert!(n("class M { void f() { Codes.find(\"x\").map(c -> c).isPresent(); Codes.arr().clone(); } }").is_empty());
        // our own types keep the chain going, so wished members on them are still found
        assert_eq!(n("class M { void f() { Codes.first().label(); Codes.make().fade(); } }"), [".label()", "Codes.make", ".fade()"]);
    }

    #[test]
    fn skips_imports_and_well_known_names() {
        assert!(names("package demo; import java.awt.Color; import static java.lang.Math.max; class M { void f() { Color c = Color.RED; String s = String.valueOf(1); List<String> l = new ArrayList<>(); System.out.println(Math.max(1, 2)); Map.Entry<String, String> e = null; java.awt.Frame fr = null; } <T> T g(T t) { return t; } }").is_empty());
    }

    #[test]
    fn constants_nested_types_and_implicit_members_are_known() {
        let vocab = "import org.slf4j.Logger; import java.util.regex.Pattern; class Svc { private static final Logger LOGGER = null; static final Pattern ID = null; \
            enum Rejection { BLANK, WILDCARD } static final class Json { static Json fromRaw(String s) { return null; } } } \
            record Code(String code, Code parent) {} enum Kind { A }";
        let mut k = known(vocab);
        for (path, ty) in implicit_members(vocab) {
            k.add_member(&path, ty.as_deref());
        }
        let n = |src: &str| find_pending(src, &k).unwrap().into_iter().map(|p| p.name).collect::<Vec<_>>();
        // a constant is a value of its declared (library) type, not a type of its own
        assert_eq!(n("class M { void f() { LOGGER.warn(\"x\"); ID.matcher(\"y\").matches(); } }"), Vec::<String>::new());
        // a nested type is named from inside its outer type by its simple name
        assert!(n("class M { void f() { Object r = Rejection.BLANK; Json.fromRaw(\"{}\"); } }").is_empty());
        // record accessors and enum values() exist without being written
        assert!(n("class M { void f(Code c) { new Code(\"a\", null).code().isBlank(); Kind.values(); Kind.valueOf(\"A\"); } }").is_empty());
        // ... and wished members are still found past them
        assert_eq!(n("class M { void f() { new Code(\"a\", null).parent().label(); Rejection.EMPTY.x(); } }"), [".label()", "Rejection.EMPTY", ".x()"]);
    }

    #[test]
    fn same_named_members_are_told_apart() {
        let vocab = "import java.util.List; import java.util.regex.Pattern; class Tz { static final List<String> VALUES = null; } \
            class Pay { static final Values VALUES = null; static final Field ID = null; } class Values { Values extendedWith(int x) { return this; } } class Field {}";
        let k = known(vocab);
        let n = |src: &str| find_pending(src, &k).unwrap().into_iter().map(|p| p.name).collect::<Vec<_>>();
        assert!(n("class M { void f() { Tz.VALUES.stream().toList(); Pay.VALUES.extendedWith(1); } }").is_empty());
        assert_eq!(n("class M { void f() { Pay.VALUES.shrink(); } }"), [".shrink()"]);
        // this file's own ID (a Pattern) wins over Pay.ID (one of ours)
        assert!(n("import java.util.regex.Pattern; class Svc { static final Pattern ID = null; boolean ok(String s) { return ID.matcher(s).matches(); } }").is_empty());
        // a project type imported from another package is still ours
        assert_eq!(n("import a.Pay; import java.awt.Color; class M { void f() { Pay.EMPTY.x(); Color.RED.darker(); } }"), ["Pay.EMPTY", ".x()"]);
        // a field of this file is typed by its declaration
        assert_eq!(n("import a.Pay; class Svc { private final Pay pay; private final String name; void f() { pay.archive(); pay.VALUES.extendedWith(1); name.trim(); } }"), [".archive()"]);
        // locals and parameters are not typed: nothing is guessed about them
        assert!(n("class M { String f(String value) { return value.trim(); } }").is_empty());
    }

    #[test]
    fn positions_are_utf16() {
        // The emoji is 4 bytes in UTF-8 and two UTF-16 units.
        let p = find_pending("class M {\n    String s = \"😀\"; Theme t;\n}", &known(VOCAB)).unwrap();
        assert_eq!(p[0].name, "Theme");
        assert_eq!((p[0].start, p[0].end), ((1, 21), (1, 26)));
    }
}
