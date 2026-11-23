//! `wishlang pseudo`: read a Java file as pseudocode.
//!
//! Every method body is cut into *units*, the statements of its blocks; a compound statement
//! (`try`, an `if`/`else` chain, a `switch`) is also cut into *parts* (`try`, each `catch`,
//! `finally`; `if`, each `else if`, `else`; each case). Claude only picks runs of sibling units and
//! words them, so a fold can never cut through an expression. The CLI turns those runs into
//! offset ranges, checks they cover the body exactly once, and falls back to one fold per
//! statement when they don't.
//!
//! Two levels: the method body folds to a one-line outline, and inside it each step folds to a
//! line of pseudocode. The renderer here and the IntelliJ plugin use the same ranges and
//! placeholders, so the terminal output is what the editor shows.

use crate::java;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tree_sitter::Node;

/// Bump when the prompt or the unit ids change: cached answers are keyed by it.
pub const PROMPT_VERSION: u32 = 6;
const DEFAULT_MODEL: &str = "haiku";
const MAX_LINE: usize = 100;

const PROMPT: &str = r#"You turn Java methods into pseudocode for a code reviewer who trusts the implementation details but wants to check the business logic: what happens, in which order, and which outcomes exist.

The file is marked up. Every method body starts with ⟦method PATH⟧. Inside it, ⟦ID⟧ marks where a unit starts. A unit is a statement; ids with a dot are nested: `4.try`, `4.catch1`, `4.finally` are the parts of try statement 4; `3.if`, `3.elif1`, `3.else` the parts of an if chain; `5.case1` the cases of a switch; `4.try.2` is the second statement inside part `4.try`, `6.1` the first statement inside loop 6.

Answer in three passes, with `path` exactly as listed:
- `types` first, for every listed type: one plain sentence, at most 120 characters, saying what the type is for, as you would explain it to a new colleague.
- `outlines`, for every listed method: one plain sentence, at most 100 characters, saying what the method does and what comes out of it, for a reader who has not seen the code: "hands a staged message to its handler and records whether it was handed over, rejected or failed". No arrows, no `|`, no lists of constants; name a status only when it is the point.
- then `methods`, for every listed method, its `steps`: the method body cut into steps. Each step is a run of sibling units `from`..`to` (same parent, in order; `from` = `to` for a single unit) with a `depth` and a `text`. Every unit of the body must be covered exactly once, either directly or because all of its parts/children are covered. A unit and one of its parts or children can never both be in steps, and a run never mixes levels: `4..6` or `4.try..4.catch1`, never `4..4.try` or `3..4.try`. To split statement 4 into its parts, list `4.try`, `4.catch1`, … as steps of their own.

How to cut steps:
1. A step is one business action or decision: the sentence a domain expert would say. Aim for 3 to 8 code lines per step and always fewer steps than statements.
2. Plumbing never gets a step of its own and never gets words: null checks and argument validation (requireNonNull, assert), logging-only calls, local aliases, `this.x = x`, Optional and stream unwrapping, casts, try-with-resources mechanics, and returning a value that was just built. Put it in the run of a neighbouring step.
3. Keep a decision (a compound statement, or a run of guards) in ONE step when all its outcomes fit on one line of at most 100 characters: "clear → book, review → hold(reason); block → PaymentBlockedException". Split a compound into its parts only when they don't fit, and then each part says one outcome.
4. Outcomes are depth+1 of the step they belong to: the parts after the first part of a split compound (each catch, else, case) get depth 1 when the compound is at depth 0. Everything else is depth 0. Say "final" or "retryable" when the code makes that distinction.
5. Each step is ONE line. When a single statement has an outcome (an orElseThrow, a ternary), put it on the same line after "; else" or "→".
6. Keep domain words verbatim: statuses, exception names, port and service names, constants, reasons. Drop Java words (Optional, stream, lambda, try, catch, null).
7. A call to a method of the same file reads as what that method does (its outline from the first pass), never as its name and never as "see X": "route to handler → HandedOver | Rejected | Failed", not "handOverClaimed(claimed)".
8. Steps (not outlines): at most 100 characters per line, terse: arrows (→) for results, `|` between alternatives, no full stops. Still readable to someone who does not know the code: say "claim the message" rather than "claim".
9. A method with nothing but plumbing or a single step returns one step covering the whole body; its outline carries the meaning. So does a method whose body is one mapping (a switch or if chain that only translates one value into another): never split a pure mapping into cases.
10. Prefer the fewest steps that still show every decision. Guards that all reject the input share one step ("no amount → 400; ≤ 0 → 422"); a lookup and its "not found" share one ("find handler; none → Failed NO_HANDLER"). Start lines lowercase.

Example. For
```java
public Receipt refund(UUID id) {⟦1⟧
    Objects.requireNonNull(id);
    ⟦2⟧Order order = orders.find(id).orElseThrow(OrderNotFound::new);
    ⟦3⟧if (order.isShipped()) {⟦3.1⟧
        throw new RefundNotAllowed(order.status());
    }
    ⟦4⟧try {⟦4.try.1⟧
        payments.refund(order.payment());
    ⟦4.catch1⟧} catch (GatewayDown e) {⟦4.catch1.1⟧
        queue.retryLater(id);
        ⟦4.catch1.2⟧return Receipt.pending(id);
    }
    ⟦5⟧order.markRefunded();
    ⟦6⟧return Receipt.done(id);
}
```
the answer is: outline "refund an unshipped order → done, or pending when the gateway is down"; steps
`1..3` d0 "find order; else OrderNotFound; shipped → RefundNotAllowed(status)", `4` d0 "refund the payment; gateway down → queue retry, return pending", `5..6` d0 "mark refunded, receipt done".
Had the catch needed more words than fit on the refund line, it would be `4.try` d0 "refund the payment" and `4.catch1` d1 "gateway down → …".
"#;

// ---------- units ----------

#[derive(Debug, Clone, PartialEq)]
pub struct Unit {
    pub id: String,
    /// Byte range: from the first non-blank character of the unit (or its leading comment) to the
    /// last character of its last line, so a fold over it keeps the line's indentation.
    pub start: usize,
    pub end: usize,
    /// Indentation a fold starting here puts before its text: a part on a line of its own (a
    /// `case`, an `else` on the next line) is folded from the line start and indented like its
    /// statement, so its outcome lines up under the statement's step.
    pub lead: String,
    pub children: Vec<Unit>,
}

#[derive(Debug, Clone)]
pub struct Method {
    pub path: String,
    /// The whole declaration, Javadoc and annotations included.
    pub decl: (usize, usize),
    /// `handOver(id)`: name and parameter names.
    pub short: String,
    /// The body's braces, `{` to `}` inclusive.
    pub body: (usize, usize),
    pub units: Vec<Unit>,
}

fn is_comment(n: Node) -> bool {
    matches!(n.kind(), "line_comment" | "block_comment")
}

fn line_start(src: &str, pos: usize) -> usize {
    src[..pos].rfind('\n').map_or(0, |i| i + 1)
}

fn line_end(src: &str, pos: usize) -> usize {
    src[pos..].find('\n').map_or(src.len(), |i| pos + i)
}

/// Is `pos` the first non-blank character of its line?
fn starts_line(src: &str, pos: usize) -> bool {
    src[line_start(src, pos)..pos].trim().is_empty()
}

/// The end of the content before `pos`, trailing blanks and newlines dropped.
fn trim_back(src: &str, pos: usize) -> usize {
    src[..pos].trim_end().len()
}

/// Units for the statements of a block (or constructor body). Comments directly above a statement
/// belong to it; comments after the last statement belong to that statement.
fn block_units(block: Node, src: &str, prefix: &str) -> Vec<Unit> {
    let mut units: Vec<Unit> = Vec::new();
    let mut lead: Option<usize> = None;
    let mut k = 0;
    let mut cur = block.walk();
    for n in block.named_children(&mut cur) {
        if is_comment(n) {
            // A trailing comment on the same line as the previous statement stays with it.
            if let Some(prev) = units.last_mut() {
                if lead.is_none() && !starts_line(src, n.start_byte()) {
                    prev.end = n.end_byte();
                    continue;
                }
            }
            lead.get_or_insert(n.start_byte());
            continue;
        }
        k += 1;
        let id = if prefix.is_empty() { k.to_string() } else { format!("{prefix}.{k}") };
        let mut end = n.end_byte();
        let rest = &src[end..line_end(src, end)];
        if rest.trim_start().starts_with("//") {
            end = line_end(src, end);
        }
        let start = lead.take().unwrap_or(n.start_byte());
        units.push(statement_unit(n, src, id, start, trim_back(src, end)));
    }
    if let (Some(_), Some(prev)) = (lead, units.last_mut()) {
        // Comments after the last statement: fold them with it.
        let last_comment = block.named_children(&mut block.walk()).filter(|n| is_comment(*n)).last();
        if let Some(c) = last_comment {
            prev.end = prev.end.max(c.end_byte());
        }
    }
    units
}

/// The block a part runs, if it is a block.
fn as_block(n: Option<Node>) -> Option<Node> {
    n.filter(|b| matches!(b.kind(), "block" | "constructor_body" | "switch_block_statement_group"))
}

/// A part of a compound statement before ranges are assigned: its name, where its own syntax
/// starts (`catch`, `else`, `case`), and the statements it holds.
struct RawPart<'a> {
    name: String,
    at: usize,
    block: Option<Node<'a>>,
}

fn statement_unit(n: Node, src: &str, id: String, start: usize, end: usize) -> Unit {
    let parts = parts_of(n, src);
    let children = match parts.len() {
        0 => match n.kind() {
            "for_statement" | "enhanced_for_statement" | "while_statement" | "do_statement" => {
                as_block(n.child_by_field_name("body")).map(|b| block_units(b, src, &id)).unwrap_or_default()
            }
            _ => Vec::new(),
        },
        // One part (an `if` without `else`): its statements are the children, as for a loop.
        1 => parts[0].block.map(|b| block_units(b, src, &id)).unwrap_or_default(),
        _ => split(&parts, src, &id, start, end),
    };
    Unit { id, start, end, lead: String::new(), children }
}

/// Give each part a range. The first part starts with the statement, the last ends with it; a part
/// whose line starts with the previous part's `}` (`} catch (E e) {`) takes that line, so no
/// structural line stays outside the folds.
fn split(parts: &[RawPart], src: &str, id: &str, start: usize, end: usize) -> Vec<Unit> {
    let starts: Vec<usize> = parts
        .iter()
        .enumerate()
        .map(|(i, p)| {
            if i == 0 {
                return start;
            }
            let ls = line_start(src, p.at);
            let before = src[ls..p.at].trim();
            if before == "}" { ls + (src[ls..].len() - src[ls..].trim_start().len()) } else { p.at }
        })
        .collect();
    let indent = {
        let ls = line_start(src, start);
        src[ls..].chars().take_while(|c| *c == ' ' || *c == '\t').collect::<String>()
    };
    parts
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let pid = format!("{id}.{}", p.name);
            let (s, lead) = if i > 0 && starts_line(src, starts[i]) { (line_start(src, starts[i]), indent.clone()) } else { (starts[i], String::new()) };
            let e = match starts.get(i + 1) {
                Some(&next) if starts_line(src, next) => trim_back(src, line_start(src, next)),
                Some(&next) => trim_back(src, next),
                None => end,
            };
            Unit { children: p.block.map(|b| block_units(b, src, &pid)).unwrap_or_default(), id: pid, start: s, end: e, lead }
        })
        .collect()
}

fn parts_of<'a>(n: Node<'a>, src: &str) -> Vec<RawPart<'a>> {
    match n.kind() {
        "try_statement" | "try_with_resources_statement" => {
            let mut out = vec![RawPart { name: "try".into(), at: n.start_byte(), block: as_block(n.child_by_field_name("body")) }];
            let mut c = 0;
            for ch in n.named_children(&mut n.walk()) {
                match ch.kind() {
                    "catch_clause" => {
                        c += 1;
                        out.push(RawPart { name: format!("catch{c}"), at: ch.start_byte(), block: as_block(ch.child_by_field_name("body")) });
                    }
                    "finally_clause" => {
                        let b = ch.named_children(&mut ch.walk()).find(|b| b.kind() == "block");
                        out.push(RawPart { name: "finally".into(), at: ch.start_byte(), block: b });
                    }
                    _ => {}
                }
            }
            out
        }
        "if_statement" => {
            let mut out = Vec::new();
            let mut cur = n;
            let mut elif = 0;
            let mut at = n.start_byte();
            loop {
                let name = if out.is_empty() { "if".to_string() } else { elif += 1; format!("elif{elif}") };
                out.push(RawPart { name, at, block: as_block(cur.child_by_field_name("consequence")) });
                let Some(alt) = cur.child_by_field_name("alternative") else { break };
                // `else` keyword: the unnamed child right before the alternative.
                let else_at = alt.prev_sibling().filter(|s| s.kind() == "else").map_or(alt.start_byte(), |s| s.start_byte());
                if alt.kind() == "if_statement" {
                    cur = alt;
                    at = else_at;
                } else {
                    out.push(RawPart { name: "else".into(), at: else_at, block: as_block(Some(alt)) });
                    break;
                }
            }
            out
        }
        _ => match switch_in(n) {
            Some(sw) => {
                let Some(body) = sw.child_by_field_name("body") else { return Vec::new() };
                let mut out = Vec::new();
                for (i, case) in body.named_children(&mut body.walk()).filter(|c| matches!(c.kind(), "switch_block_statement_group" | "switch_rule")).enumerate() {
                    let block = if case.kind() == "switch_rule" { as_block(case.named_children(&mut case.walk()).find(|b| b.kind() == "block")) } else { None };
                    out.push(RawPart { name: format!("case{}", i + 1), at: case.start_byte(), block });
                }
                let _ = src;
                out
            }
            None => Vec::new(),
        },
    }
}

/// A switch that is the statement itself, or what it returns, assigns or yields.
fn switch_in<'a>(n: Node<'a>) -> Option<Node<'a>> {
    if n.kind() == "switch_expression" {
        return Some(n);
    }
    match n.kind() {
        "return_statement" | "expression_statement" | "yield_statement" => n.named_children(&mut n.walk()).find(|c| c.kind() == "switch_expression"),
        "local_variable_declaration" => n.children_by_field_name("declarator", &mut n.walk()).find_map(|d| d.child_by_field_name("value").filter(|v| v.kind() == "switch_expression")),
        _ => None,
    }
}

/// Every method and constructor with a body, with its item path (as `wishlang items` names it).
pub fn methods(src: &str) -> Result<Vec<Method>> {
    let Some(tree) = java::parse_tree(src) else { bail!("the file has syntax errors") };
    let items = java::parse(src)?;
    let by_line: HashMap<usize, String> =
        items.iter().filter(|i| matches!(i.kind, "method" | "constructor")).map(|i| (i.line, i.path.clone())).collect();
    let mut out = Vec::new();
    let mut stack = vec![tree.root_node()];
    while let Some(n) = stack.pop() {
        if matches!(n.kind(), "method_declaration" | "constructor_declaration") {
            if let (Some(body), Some(name)) = (n.child_by_field_name("body"), n.child_by_field_name("name")) {
                if let Some(path) = by_line.get(&(name.start_position().row + 1)) {
                    let short = format!("{}{}", java::text(name, src), param_names(n, src));
                    out.push(Method { path: path.clone(), decl: (member_start(n), n.end_byte()), short, body: (body.start_byte(), body.end_byte()), units: block_units(body, src, "") });
                }
            }
            continue; // ponytail: lambdas and local/anonymous classes stay inside their statement; give a block lambda child units if reviewers need them.
        }
        let mut cur = n.walk();
        let kids: Vec<Node> = n.named_children(&mut cur).collect();
        stack.extend(kids.into_iter().rev());
    }
    out.sort_by_key(|m| m.body.0);
    Ok(out)
}

fn walk_units<'a>(units: &'a [Unit], out: &mut Vec<&'a Unit>) {
    for u in units {
        out.push(u);
        walk_units(&u.children, out);
    }
}

/// The source with `⟦method PATH⟧` before each body and `⟦ID⟧` where each unit starts.
pub fn marked(src: &str, methods: &[Method]) -> String {
    let mut marks: BTreeMap<usize, String> = BTreeMap::new();
    for m in methods {
        marks.entry(m.body.0).or_default().push_str(&format!("⟦method {}⟧", m.path));
        let mut all = Vec::new();
        walk_units(&m.units, &mut all);
        for u in all {
            marks.entry(u.start).or_default().push_str(&format!("⟦{}⟧", u.id));
        }
    }
    let mut out = String::with_capacity(src.len() + marks.len() * 8);
    let mut at = 0;
    for (pos, mark) in marks {
        out.push_str(&src[at..pos]);
        out.push_str(&mark);
        at = pos;
    }
    out.push_str(&src[at..]);
    out
}

// ---------- steps -> folds ----------

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct Step {
    pub from: String,
    pub to: String,
    #[serde(default)]
    pub depth: usize,
    pub text: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct Answer {
    pub path: String,
    #[serde(default)]
    pub outline: String,
    #[serde(default)]
    pub steps: Vec<Step>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Fold {
    pub start: usize,
    pub end: usize,
    pub kind: Kind,
    pub lead: String,
    pub depth: usize,
    pub text: String,
}

/// What a fold stands for. Class, method, step and body folds are the zoom levels; a detail fold
/// (imports, a class heading, fields, a signature) is shorthand that stays folded at every level.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub enum Kind {
    Class,
    Method,
    Body,
    Step,
    Detail,
}

impl Fold {
    pub fn placeholder(&self) -> String {
        format!("{}{}{}", self.lead, "  ".repeat(self.depth), self.text)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Folded {
    pub path: String,
    pub decl: (usize, usize),
    pub short: String,
    pub outline: String,
    pub body: (usize, usize),
    /// Step folds; empty for a method that only has its outline.
    pub folds: Vec<Fold>,
    /// Why the model's steps were not used, when they weren't.
    pub fallback: Option<String>,
    /// The sibling runs the folds came from, for the eval.
    #[serde(skip)]
    pub runs: Vec<(String, String)>,
}

fn parent(id: &str) -> &str {
    id.rfind('.').map_or("", |i| &id[..i])
}

/// Check the model's steps for a method and turn them into folds.
pub fn folds(m: &Method, steps: &[Step]) -> std::result::Result<Vec<Fold>, String> {
    let mut all = Vec::new();
    walk_units(&m.units, &mut all);
    let index: HashMap<&str, &Unit> = all.iter().map(|u| (u.id.as_str(), *u)).collect();
    // Siblings in order, per parent id.
    let mut siblings: HashMap<&str, Vec<&str>> = HashMap::new();
    for u in &all {
        siblings.entry(parent(&u.id)).or_default().push(u.id.as_str());
    }
    let mut covered: BTreeSet<&str> = BTreeSet::new();
    let mut out = Vec::new();
    for s in steps {
        // `4..4.try` (a statement and its own part) means the part: models mix the two levels.
        let from = if s.to.starts_with(&format!("{}.", s.from)) {
            siblings.get(parent(&s.to)).and_then(|v| v.first()).map_or(s.from.as_str(), |f| *f)
        } else {
            s.from.as_str()
        };
        let (Some(a), Some(b)) = (index.get(from), index.get(s.to.as_str())) else {
            return Err(format!("unknown unit {}..{}", s.from, s.to));
        };
        if parent(&a.id) != parent(&b.id) {
            return Err(format!("{}..{} are not siblings", s.from, s.to));
        }
        let sib = &siblings[parent(&a.id)];
        let (i, j) = (sib.iter().position(|x| *x == a.id).unwrap(), sib.iter().position(|x| *x == b.id).unwrap());
        if i > j {
            return Err(format!("{}..{} run backwards", s.from, s.to));
        }
        for id in &sib[i..=j] {
            if !covered.insert(id) {
                return Err(format!("{id} is in two steps"));
            }
        }
        let text = s.text.trim().replace('\n', " ");
        if text.is_empty() {
            return Err(format!("{}..{} has no text", s.from, s.to));
        }
        out.push(Fold { start: a.start, end: b.end, kind: Kind::Step, lead: a.lead.clone(), depth: s.depth.min(3), text });
    }
    for id in &covered {
        let mut p = parent(id);
        while !p.is_empty() {
            if covered.contains(p) {
                return Err(format!("{p} and its part {id} are both steps"));
            }
            p = parent(p);
        }
    }
    fn whole(u: &Unit, covered: &BTreeSet<&str>) -> bool {
        covered.contains(u.id.as_str()) || (!u.children.is_empty() && u.children.iter().all(|c| whole(c, covered)))
    }
    if let Some(u) = m.units.iter().find(|u| !whole(u, &covered)) {
        return Err(format!("unit {} is not covered", u.id));
    }
    out.sort_by_key(|f| f.start);
    Ok(out)
}

fn is_guard(u: &Unit, src: &str) -> bool {
    let t = src[u.start..u.end].trim_start();
    let t = t.strip_prefix("Objects.").unwrap_or(t);
    t.starts_with("requireNonNull") || t.starts_with("assert ") || t.starts_with("Preconditions.") || t.starts_with("Validate.")
}

fn first_line(s: &str) -> String {
    let l = s.lines().find(|l| {
        let t = l.trim();
        !t.is_empty() && !t.starts_with("//") && !t.starts_with("/*") && !t.starts_with('*')
    });
    let l = l.unwrap_or("").trim();
    if l.chars().count() > MAX_LINE { format!("{}…", l.chars().take(MAX_LINE - 1).collect::<String>()) } else { l.to_string() }
}

/// One step per top-level statement, guards merged into the next one: what the model has to beat.
pub fn baseline(m: &Method, src: &str) -> Vec<Step> {
    let mut steps: Vec<Step> = Vec::new();
    let mut pending: Option<String> = None;
    for u in &m.units {
        if is_guard(u, src) && pending.is_none() {
            pending = Some(u.id.clone());
            continue;
        }
        if is_guard(u, src) {
            continue;
        }
        let from = pending.take().unwrap_or(u.id.clone());
        steps.push(Step { from, to: u.id.clone(), depth: 0, text: first_line(&src[u.start..u.end]) });
    }
    if pending.is_some() {
        let last = m.units.last().unwrap().id.clone();
        match steps.last_mut() {
            Some(s) => s.to = last,
            None => steps.push(Step { from: m.units[0].id.clone(), to: last, depth: 0, text: first_line(&src[m.units[0].start..m.units[0].end]) }),
        }
    }
    steps
}

/// Folds for every method: the model's steps where they check out, the baseline otherwise.
pub fn fold_all(src: &str, methods: &[Method], answers: &[Answer]) -> Vec<Folded> {
    let by_path: HashMap<&str, &Answer> = answers.iter().map(|a| (a.path.as_str(), a)).collect();
    let last = |p: &str| p.rsplit(['.', '#']).next().unwrap_or(p).to_string();
    methods
        .iter()
        .map(|m| {
            // Models sometimes drop the class: match on the method name when that is unambiguous.
            let answer = by_path.get(m.path.as_str()).copied().or_else(|| {
                let mut same = answers.iter().filter(|a| last(&a.path) == last(&m.path));
                let one = same.next();
                one.filter(|_| same.next().is_none() && methods.iter().filter(|o| last(&o.path) == last(&m.path)).count() == 1)
            });
            let outline = answer.map(|a| a.outline.trim().replace('\n', " ")).filter(|o| !o.is_empty()).unwrap_or_else(|| "…".into());
            if m.units.is_empty() {
                return Folded { path: m.path.clone(), decl: m.decl, short: m.short.clone(), outline, body: m.body, folds: Vec::new(), fallback: None, runs: Vec::new() };
            }
            let (steps, fallback) = match answer {
                None => (baseline(m, src), Some("no answer for this method".to_string())),
                Some(a) => match folds(m, &a.steps) {
                    Ok(_) => (a.steps.clone(), None),
                    Err(e) => (baseline(m, src), Some(e)),
                },
            };
            let f = folds(m, &steps).unwrap_or_default();
            let runs = steps.iter().map(|s| (s.from.clone(), s.to.clone())).collect();
            // A single step says no more than the outline: show the outline only.
            let f = if f.len() <= 1 { Vec::new() } else { f };
            Folded { path: m.path.clone(), decl: m.decl, short: m.short.clone(), outline, body: m.body, folds: f, fallback, runs }
        })
        .collect()
}

// ---------- the frame: everything around the bodies ----------

/// Annotations that say nothing to a reader of the logic.
const QUIET_ANNOTATIONS: &[&str] = &["Override", "SuppressWarnings", "Nullable", "NonNull", "Nonnull", "NotNull", "Autowired", "Inject", "Deprecated", "SafeVarargs", "FunctionalInterface"];

fn squash(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Where a member starts once the comments directly above it are counted in.
fn member_start(n: Node) -> usize {
    let mut start = n.start_byte();
    let mut p = n.prev_sibling();
    while let Some(c) = p.filter(|c| is_comment(*c)) {
        start = c.start_byte();
        p = c.prev_sibling();
    }
    start
}

/// The annotations worth keeping (`@RestController`, `@PostMapping("/pay")`), each followed by a space.
fn loud_annotations(n: Node, src: &str) -> String {
    let Some(mods) = n.named_children(&mut n.walk()).find(|c| c.kind() == "modifiers") else { return String::new() };
    mods.named_children(&mut mods.walk())
        .filter(|a| matches!(a.kind(), "annotation" | "marker_annotation"))
        .filter(|a| a.child_by_field_name("name").is_none_or(|nm| !QUIET_ANNOTATIONS.contains(&java::text(nm, src).rsplit('.').next().unwrap_or(""))))
        .map(|a| format!("{} ", squash(java::text(a, src))))
        .collect()
}

fn is_static(n: Node, src: &str) -> bool {
    n.named_children(&mut n.walk()).find(|c| c.kind() == "modifiers").is_some_and(|m| java::text(m, src).split_whitespace().any(|w| w == "static"))
}

fn is_final(n: Node, src: &str) -> bool {
    n.named_children(&mut n.walk()).find(|c| c.kind() == "modifiers").is_some_and(|m| java::text(m, src).split_whitespace().any(|w| w == "final"))
}

/// `(id, request)`: parameter names only.
fn param_names(n: Node, src: &str) -> String {
    let names: Vec<&str> = n
        .child_by_field_name("parameters")
        .map(|ps| {
            ps.named_children(&mut ps.walk())
                .filter_map(|p| match p.kind() {
                    "formal_parameter" => p.child_by_field_name("name"),
                    "spread_parameter" => p.named_children(&mut p.walk()).find(|c| c.kind() == "variable_declarator").and_then(|d| d.child_by_field_name("name")),
                    _ => None,
                })
                .map(|nm| java::text(nm, src))
                .collect()
        })
        .unwrap_or_default();
    format!("({})", names.join(", "))
}

/// `handOver(id) → StagedReceipt`, `POST … pay(accountId, request) → ResponseEntity<PaymentView>`, `new PaymentService(…)`.
fn signature(n: Node, src: &str) -> String {
    let name = n.child_by_field_name("name").map_or("", |nm| java::text(nm, src));
    let head = format!("{}{name}{}", loud_annotations(n, src), param_names(n, src));
    match n.kind() {
        "constructor_declaration" => format!("{}new {name}{}", loud_annotations(n, src), param_names(n, src)),
        _ => match n.child_by_field_name("type").map(|t| squash(java::text(t, src))) {
            Some(t) if t != "void" => format!("{head} → {t}"),
            _ => head,
        },
    }
}

/// `class HandOverService implements HandOverPort`, `record Ended(outcome, cause)`.
fn type_header(n: Node, src: &str) -> String {
    let kind = match n.kind() {
        "class_declaration" => "class",
        "interface_declaration" => "interface",
        "enum_declaration" => "enum",
        "record_declaration" => "record",
        _ => "annotation",
    };
    let name = n.child_by_field_name("name").map_or("", |nm| java::text(nm, src));
    let mut out = format!("{}{kind} {name}", loud_annotations(n, src));
    if kind == "record" {
        out.push_str(&param_names(n, src));
    }
    for c in n.named_children(&mut n.walk()) {
        if matches!(c.kind(), "superclass" | "super_interfaces" | "extends_interfaces" | "permits") {
            out.push(' ');
            out.push_str(&squash(java::text(c, src)));
        }
    }
    out
}

/// A constructor that only stores its arguments (`this.x = requireNonNull(x)`): it tells a reader nothing the fields don't.
fn is_wiring(n: Node, src: &str) -> bool {
    n.kind() == "constructor_declaration"
        && n.child_by_field_name("body").is_some_and(|b| {
            b.named_children(&mut b.walk()).filter(|c| !is_comment(*c)).all(|st| {
                let t = java::text(st, src).trim_start();
                st.kind() == "expression_statement" && (t.starts_with("this.") || t.starts_with("Objects.requireNonNull") || t.starts_with("requireNonNull"))
            })
        })
}

fn field_names(n: Node, src: &str) -> Vec<String> {
    n.children_by_field_name("declarator", &mut n.walk()).filter_map(|d| d.child_by_field_name("name")).map(|nm| java::text(nm, src).to_string()).collect()
}

/// The folds around the bodies, no model involved. Each type folds to `class Name: <outline>`; inside
/// it its heading folds to `class Name implements X`, runs of fields to `uses a, b` / `constants A, B` /
/// `state x` (swallowing a constructor that only wires them), and each method's Javadoc, annotations,
/// modifiers and types to `name(params) → Type`. Package and imports fold to `package x.y`.
pub fn frame(src: &str, type_outlines: &HashMap<String, String>) -> Vec<Fold> {
    let Some(tree) = java::parse_tree(src) else { return Vec::new() };
    let root = tree.root_node();
    let mut out = Vec::new();
    let heads: Vec<Node> = root.named_children(&mut root.walk()).filter(|c| matches!(c.kind(), "package_declaration" | "import_declaration")).collect();
    if let (Some(first), Some(last)) = (heads.first(), heads.last()) {
        let text = match heads.iter().find(|h| h.kind() == "package_declaration") {
            Some(p) => squash(java::text(*p, src).trim_end_matches(';')),
            None => "imports".into(),
        };
        out.push(Fold { start: first.start_byte(), end: last.end_byte(), kind: Kind::Detail, lead: String::new(), depth: 0, text });
    }
    types(root, "", src, type_outlines, &mut out);
    out.sort_by_key(|f| (f.start, std::cmp::Reverse(f.end)));
    out.dedup_by_key(|f| (f.start, f.end));
    out
}

fn is_type(n: Node) -> bool {
    matches!(n.kind(), "class_declaration" | "interface_declaration" | "enum_declaration" | "record_declaration")
}

fn types(parent: Node, prefix: &str, src: &str, outlines: &HashMap<String, String>, out: &mut Vec<Fold>) {
    let detail = |start: usize, end: usize, text: String| Fold { start, end, kind: Kind::Detail, lead: String::new(), depth: 0, text };
    for t in parent.named_children(&mut parent.walk()).filter(|c| is_type(*c)) {
        let start = member_start(t);
        let name = t.child_by_field_name("name").map_or("", |nm| java::text(nm, src));
        let path = format!("{prefix}{name}");
        let Some(body) = t.child_by_field_name("body") else { continue };
        let members: Vec<Node> = body.named_children(&mut body.walk()).filter(|c| !is_comment(*c)).collect();
        if members.is_empty() {
            out.push(detail(start, t.end_byte(), type_header(t, src)));
            continue;
        }
        let outline = outlines.get(&path).map(|o| o.trim().replace('\n', " ")).filter(|o| !o.is_empty());
        let kind = type_header(t, src).split_whitespace().find(|w| matches!(*w, "class" | "interface" | "enum" | "record" | "annotation")).unwrap_or("class").to_string();
        out.push(Fold { start, end: t.end_byte(), kind: Kind::Class, lead: String::new(), depth: 0, text: format!("{kind} {name}: {}", outline.unwrap_or_else(|| "…".into())) });
        // Down to and including the body's `{`, so the expanded class reads as a heading.
        out.push(detail(start, body.start_byte() + 1, type_header(t, src)));
        let mut i = 0;
        while i < members.len() {
            let m = members[i];
            match m.kind() {
                "field_declaration" | "constant_declaration" => {
                    let kind = |f: Node| if is_static(f, src) { "constants" } else if is_final(f, src) { "uses" } else { "state" };
                    let k = kind(m);
                    let mut j = i;
                    let mut names = Vec::new();
                    while j < members.len() && matches!(members[j].kind(), "field_declaration" | "constant_declaration") && kind(members[j]) == k {
                        names.extend(field_names(members[j], src));
                        j += 1;
                    }
                    let mut end = members[j - 1].end_byte();
                    if k == "uses" && j < members.len() && is_wiring(members[j], src) {
                        end = members[j].end_byte();
                        j += 1;
                    }
                    out.push(detail(member_start(m), end, format!("{k} {}", names.join(", "))));
                    i = j;
                    continue;
                }
                "method_declaration" | "constructor_declaration" => {
                    let s = member_start(m);
                    let end = m.child_by_field_name("body").map_or(m.end_byte(), |b| trim_back(src, b.start_byte()));
                    out.push(detail(s, end, signature(m, src)));
                }
                _ => {}
            }
            i += 1;
        }
        types(body, &format!("{path}."), src, outlines, out);
    }
}

/// Every fold of the file: the frame, each method folded to `name(params): <outline>`, and inside it
/// either its steps or, for a method without steps, its body folded to `{ <outline> }`.
pub fn all_folds(src: &str, folded: &[Folded], answers: &[Answer]) -> Vec<Fold> {
    let types: HashMap<String, String> = answers.iter().map(|a| (a.path.clone(), a.outline.clone())).collect();
    let mut out = frame(src, &types);
    let wired: Vec<(usize, usize)> = out.iter().filter(|f| f.kind == Kind::Detail && f.text.starts_with("uses ")).map(|f| (f.start, f.end)).collect();
    for f in folded {
        // A wiring constructor already went into its `uses` line.
        if wired.iter().any(|(s, e)| *s <= f.decl.0 && f.decl.1 <= *e) {
            continue;
        }
        out.push(Fold { start: f.decl.0, end: f.decl.1, kind: Kind::Method, lead: String::new(), depth: 0, text: format!("{}: {}", f.short, f.outline) });
        if f.folds.is_empty() {
            out.push(Fold { start: f.body.0, end: f.body.1, kind: Kind::Body, lead: String::new(), depth: 0, text: format!("{{ {} }}", f.outline) });
        } else {
            out.extend(f.folds.iter().cloned());
        }
    }
    out.sort_by_key(|f| (f.start, std::cmp::Reverse(f.end)));
    out
}

/// The zoom level at which a fold shows as its placeholder: the number of zoomable folds around
/// it. Detail folds never open by zooming.
pub fn zoom_of(folds: &[Fold], f: &Fold) -> u8 {
    if f.kind == Kind::Detail {
        return u8::MAX;
    }
    folds.iter().filter(|o| o.kind != Kind::Detail && !std::ptr::eq(*o, f) && o.start <= f.start && f.end <= o.end && (o.start, o.end) != (f.start, f.end)).count() as u8
}

// ---------- rendering ----------

/// The file as the editor shows it zoomed to `zoom`: 0 the class as one line, 1 one line per
/// method, 2 each method's steps, 3 the code.
pub fn render(src: &str, folds: &[Fold], zoom: u8) -> String {
    let mut out = String::new();
    let mut at = 0;
    for f in folds {
        if f.start < at || zoom_of(folds, f) < zoom {
            continue;
        }
        out.push_str(&src[at..f.start]);
        out.push_str(&f.placeholder());
        at = f.end;
    }
    out.push_str(&src[at..]);
    out
}

fn utf16(src: &str, byte: usize) -> usize {
    src[..byte].encode_utf16().count()
}

/// The IntelliJ wire format: UTF-16 offsets, as a Document counts them.
const KEYWORDS: &[&str] = &["class", "interface", "enum", "record", "annotation", "package", "implements", "extends", "permits", "new", "uses", "constants", "state", "else", "null", "true", "false", "this", "return", "throw"];

/// What a word of pseudocode is, named after the editor's Java colours. `None`: plain text.
#[derive(Default)]
pub struct Names {
    pub params: BTreeSet<String>,
    pub fields: BTreeSet<String>,
    pub methods: BTreeSet<String>,
}

/// Highlighting spans for a placeholder, in UTF-16 units: `(start, end, kind)`, kind one of
/// keyword, method_decl, method_call, parameter, field, constant, type, string, number,
/// annotation, operator. The names the file declares decide parameters, fields and methods; the
/// rest goes by shape (`ALL_CAPS` constant, `Capitalised` type).
pub fn tokens(text: &str, names: &Names) -> Vec<(usize, usize, &'static str)> {
    let chars: Vec<char> = text.chars().collect();
    let u16_at: Vec<usize> = std::iter::once(0).chain(chars.iter().scan(0, |acc, c| { *acc += c.len_utf16(); Some(*acc) })).collect();
    let mut out = Vec::new();
    let mut i = 0;
    let first_word = chars.iter().position(|c| !c.is_whitespace()).unwrap_or(0);
    while i < chars.len() {
        let c = chars[i];
        let start = i;
        let kind: Option<&'static str> = if c == '"' {
            i += 1;
            while i < chars.len() && chars[i] != '"' {
                i += 1;
            }
            i = (i + 1).min(chars.len());
            Some("string")
        } else if c == '@' && chars.get(i + 1).is_some_and(|n| n.is_alphabetic()) {
            i += 1;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_' || chars[i] == '.') {
                i += 1;
            }
            Some("annotation")
        } else if c.is_ascii_digit() {
            while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.' || chars[i] == '_') {
                i += 1;
            }
            Some("number")
        } else if c.is_alphabetic() || c == '_' || c == '<' && chars.get(i + 1) == Some(&'i') {
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_' || chars[i] == '<' || chars[i] == '>') && !(chars[i] == '>' && chars[start] != '<') {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            let next = chars.get(i).copied();
            let after_dot = start > 0 && chars[start - 1] == '.';
            // Keywords only where Java puts them: the heading word, and a class heading's clauses.
            // In prose "record an outcome" is a verb.
            let heading = KEYWORDS.contains(&chars[first_word..].iter().take_while(|c| c.is_alphabetic()).collect::<String>().as_str());
            if KEYWORDS.contains(&word.as_str()) && (start == first_word || heading && matches!(word.as_str(), "implements" | "extends" | "permits")) {
                Some("keyword")
            } else if next == Some('(') && start == first_word {
                Some("method_decl")
            } else if next == Some('(') && !word.starts_with(char::is_uppercase) || names.methods.contains(&word) && !after_dot {
                Some("method_call")
            } else if names.params.contains(&word) && !after_dot {
                Some("parameter")
            } else if word.len() > 1 && word.chars().any(|c| c.is_alphabetic()) && word.chars().all(|c| c.is_uppercase() || c.is_ascii_digit() || c == '_') {
                Some("constant")
            } else if names.fields.contains(&word) && !after_dot {
                Some("field")
            } else if word.starts_with(char::is_uppercase) {
                Some("type")
            } else {
                None
            }
        } else if matches!(c, '→' | '|' | ';' | '=' | '≤' | '≥' | '+') {
            i += 1;
            Some("operator")
        } else {
            i += 1;
            None
        };
        if let Some(k) = kind {
            out.push((u16_at[start], u16_at[i], k));
        }
    }
    out
}

/// The names a fold's text can mention: its method's parameters, and the file's fields and methods.
fn names_for(f: &Fold, folds: &[Fold], folded: &[Folded]) -> Names {
    let mut n = Names::default();
    for d in folds.iter().filter(|d| d.kind == Kind::Detail) {
        for prefix in ["uses ", "constants ", "state "] {
            if let Some(rest) = d.text.strip_prefix(prefix) {
                n.fields.extend(rest.split(", ").map(str::to_string));
            }
        }
    }
    for m in folded {
        let (name, rest) = m.short.split_once('(').unwrap_or((&m.short, ""));
        n.methods.insert(name.to_string());
        if m.decl.0 <= f.start && f.end <= m.decl.1 {
            n.params.extend(rest.trim_end_matches(')').split(", ").filter(|p| !p.is_empty()).map(str::to_string));
        }
    }
    // A constructor is named like its class: that word is a type.
    n.methods.retain(|m| !m.starts_with(char::is_uppercase));
    n
}

pub fn wire(src: &str, folds: &[Fold], folded: &[Folded]) -> Value {
    json!({"folds": folds.iter().map(|x| {
        let text = x.placeholder();
        let spans: Vec<Value> = tokens(&text, &names_for(x, folds, folded)).into_iter().map(|(s, e, k)| json!([s, e, k])).collect();
        json!({"start": utf16(src, x.start), "end": utf16(src, x.end), "placeholder": text, "zoom": zoom_of(folds, x), "tokens": spans})
    }).collect::<Vec<_>>(),
    "methods": folded.iter().map(|f| json!({
        "path": f.path,
        "outline": f.outline,
        "body": [utf16(src, f.body.0), utf16(src, f.body.1)],
        "folds": f.folds.iter().map(|x| json!({"start": utf16(src, x.start), "end": utf16(src, x.end), "placeholder": x.placeholder()})).collect::<Vec<_>>(),
        "fallback": f.fallback,
    })).collect::<Vec<_>>()})
}

// ---------- the model ----------

fn schema() -> Value {
    json!({
        "type": "object",
        "required": ["types", "outlines", "methods"],
        "properties": {"types": {"type": "array", "items": {
            "type": "object",
            "required": ["path", "outline"],
            "properties": {"path": {"type": "string"}, "outline": {"type": "string"}}
        }}, "outlines": {"type": "array", "items": {
            "type": "object",
            "required": ["path", "outline"],
            "properties": {"path": {"type": "string"}, "outline": {"type": "string"}}
        }}, "methods": {"type": "array", "items": {
            "type": "object",
            "required": ["path", "steps"],
            "properties": {
                "path": {"type": "string"},
                "steps": {"type": "array", "items": {
                    "type": "object",
                    "required": ["from", "to", "depth", "text"],
                    "properties": {
                        "from": {"type": "string"},
                        "to": {"type": "string"},
                        "depth": {"type": "integer", "minimum": 0},
                        "text": {"type": "string"}
                    }
                }}
            }
        }}}
    })
}

pub fn model() -> String {
    std::env::var("WISH_PSEUDO_MODEL").ok().filter(|m| !m.is_empty()).unwrap_or_else(|| DEFAULT_MODEL.into())
}

fn cache_file(src: &str, model: &str) -> PathBuf {
    crate::daemon::home().join("pseudo").join(format!("{}-{model}-v{PROMPT_VERSION}.json", java::hash(src)))
}

/// Ask Claude for every method's outline and steps.
pub fn ask(src: &str, methods: &[Method], model: &str) -> Result<Vec<Answer>> {
    if methods.is_empty() {
        return Ok(Vec::new());
    }
    let list: Vec<&str> = methods.iter().map(|m| m.path.as_str()).collect();
    let types: Vec<String> = java::parse(src)?.into_iter().filter(|i| matches!(i.kind, "class" | "interface" | "enum" | "record")).map(|i| i.path).collect();
    let prompt = format!("Types: {}\nMethods: {}\n\n```java\n{}\n```\n", types.join(", "), list.join(", "), marked(src, methods));
    let exe = std::env::var("WISH_CLAUDE").unwrap_or_else(|_| "claude".into());
    let mut child = Command::new(&exe)
        .env(crate::session::IN_CYCLE, "1")
        .args(["-p", "--output-format", "json", "--no-session-persistence", "--disable-slash-commands", "--strict-mcp-config"])
        .args(["--tools", ""])
        .args(["--json-schema", &schema().to_string()])
        .args(["--system-prompt", PROMPT])
        .args(["--model", model])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("starting `{exe}` (is Claude Code installed and on PATH?)"))?;
    child.stdin.take().unwrap().write_all(prompt.as_bytes())?;
    let out = child.wait_with_output()?;
    let res: Value = serde_json::from_slice(&out.stdout)
        .with_context(|| format!("claude exited ({}) without a result: {}", out.status, String::from_utf8_lossy(&out.stderr).trim()))?;
    if res["is_error"] == true {
        bail!("claude reported an error: {}", res["result"].as_str().unwrap_or(&res["subtype"].to_string()));
    }
    let v = if res["structured_output"].is_object() {
        res["structured_output"].clone()
    } else {
        let text = res["result"].as_str().unwrap_or("");
        let (s, e) = (text.find('{').unwrap_or(0), text.rfind('}').map_or(text.len(), |e| e + 1));
        serde_json::from_str(&text[s..e]).context("claude's answer is not JSON")?
    };
    let mut answers: Vec<Answer> = serde_json::from_value(v["methods"].clone()).context("claude's answer has no methods")?;
    for o in v["outlines"].as_array().into_iter().chain(v["types"].as_array()).flatten() {
        let (path, outline) = (o["path"].as_str().unwrap_or(""), o["outline"].as_str().unwrap_or(""));
        match answers.iter_mut().find(|a| a.path == path) {
            Some(a) if a.outline.is_empty() => a.outline = outline.to_string(),
            Some(_) => {}
            None => answers.push(Answer { path: path.to_string(), outline: outline.to_string(), steps: Vec::new() }),
        }
    }
    Ok(answers)
}

/// Answers from the cache, or from Claude (then cached).
pub fn answers(src: &str, methods: &[Method], model: &str) -> Result<Vec<Answer>> {
    let file = cache_file(src, model);
    if let Some(a) = std::fs::read(&file).ok().and_then(|b| serde_json::from_slice(&b).ok()) {
        return Ok(a);
    }
    let a = ask(src, methods, model)?;
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(&file, serde_json::to_vec(&a)?);
    Ok(a)
}

fn read_input(file: &str) -> Result<String> {
    if file == "-" {
        let mut s = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut s)?;
        Ok(s)
    } else {
        std::fs::read_to_string(file).with_context(|| format!("reading {file}"))
    }
}

/// `wishlang pseudo`.
pub fn run(file: &str, json_out: bool, zoom: u8, baseline_only: bool) -> Result<String> {
    let src = read_input(file)?;
    let ms = methods(&src)?;
    let answers = if baseline_only {
        ms.iter().map(|m| Answer { path: m.path.clone(), outline: String::new(), steps: baseline(m, &src) }).collect()
    } else {
        answers(&src, &ms, &model())?
    };
    let folded = fold_all(&src, &ms, &answers);
    let folds = all_folds(&src, &folded, &answers);
    if json_out {
        return Ok(wire(&src, &folds, &folded).to_string());
    }
    let mut out = render(&src, &folds, zoom);
    for f in folded.iter().filter(|f| f.fallback.is_some() && !baseline_only) {
        out.push_str(&format!("\n// pseudo: {} fell back to one step per statement: {}", f.path, f.fallback.as_deref().unwrap_or("")));
    }
    Ok(out)
}

// ---------- eval ----------

/// Golden: per method path, the expected sibling runs (`["1","2"]`, or `["3"]` for one unit). An
/// empty list means "outline only".
type Golden = BTreeMap<String, Vec<Vec<String>>>;

fn run_set(runs: &[(String, String)]) -> BTreeSet<(String, String)> {
    runs.iter().cloned().collect()
}

fn golden_set(g: &[Vec<String>]) -> BTreeSet<(String, String)> {
    g.iter().filter_map(|r| Some((r.first()?.clone(), r.last()?.clone()))).collect()
}

struct Score {
    hit: usize,
    got: usize,
    want: usize,
    invalid: Vec<String>,
    steps: usize,
    lines: usize,
    long: usize,
    misses: Vec<String>,
}

fn score(src: &str, folded: &[Folded], golden: &Golden) -> Score {
    let mut s = Score { hit: 0, got: 0, want: 0, invalid: Vec::new(), steps: 0, lines: 0, long: 0, misses: Vec::new() };
    for f in folded {
        if let Some(e) = &f.fallback {
            s.invalid.push(format!("{}: {e}", f.path));
        }
        s.long += f.folds.iter().filter(|x| x.placeholder().chars().count() > MAX_LINE).count() + usize::from(f.outline.chars().count() > MAX_LINE);
        s.steps += f.folds.len();
        s.lines += f.folds.iter().map(|x| src[x.start..x.end].lines().count()).sum::<usize>();
        let Some(g) = golden.get(&f.path) else { continue };
        let got = if f.folds.is_empty() { BTreeSet::new() } else { run_set(&f.runs) };
        let want = golden_set(g);
        if got.is_empty() && want.is_empty() {
            s.hit += 1;
            s.got += 1;
            s.want += 1;
            continue;
        }
        s.hit += got.intersection(&want).count();
        s.got += got.len();
        s.want += want.len();
        if got != want {
            let show = |set: &BTreeSet<(String, String)>| {
                if set.is_empty() { "outline only".to_string() } else { set.iter().map(|(a, b)| if a == b { a.clone() } else { format!("{a}..{b}") }).collect::<Vec<_>>().join(" ") }
            };
            s.misses.push(format!("    {}\n      want {}\n      got  {}", f.path, show(&want), show(&got)));
        }
    }
    s
}

fn ratio(a: usize, b: usize) -> f64 {
    if b == 0 { 1.0 } else { a as f64 / b as f64 }
}

/// `wishlang pseudo --eval DIR`: every `X.java` with an `X.golden.json` beside it, `runs` fresh
/// model calls each (no cache), scored against the golden and against the baseline.
pub fn eval(dir: &Path, runs: usize) -> Result<String> {
    let model = model();
    let mut out = format!("model {model}, prompt v{PROMPT_VERSION}, {runs} run(s) per fixture\n");
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)?.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.extension().is_some_and(|x| x == "java")).collect();
    files.sort();
    for file in files {
        let golden_path = file.with_extension("golden.json");
        let Ok(g) = std::fs::read(&golden_path) else { continue };
        let golden: Golden = serde_json::from_slice(&g).with_context(|| format!("reading {}", golden_path.display()))?;
        let src = std::fs::read_to_string(&file)?;
        let ms = methods(&src)?;
        let name = file.file_name().unwrap().to_string_lossy();

        let base: Vec<Answer> = ms.iter().map(|m| Answer { path: m.path.clone(), outline: String::new(), steps: baseline(m, &src) }).collect();
        let b = score(&src, &fold_all(&src, &ms, &base), &golden);
        out.push_str(&format!("\n{name}\n  baseline  P {:.2} R {:.2}  steps {}\n", ratio(b.hit, b.got), ratio(b.hit, b.want), b.steps));

        let mut sets: Vec<BTreeSet<(String, String, String)>> = Vec::new();
        for r in 1..=runs {
            let a = match ask(&src, &ms, &model) {
                Ok(a) => a,
                Err(e) => {
                    out.push_str(&format!("  run {r}    error: {e:#}\n"));
                    continue;
                }
            };
            let folded = fold_all(&src, &ms, &a);
            let s = score(&src, &folded, &golden);
            out.push_str(&format!(
                "  run {r}     P {:.2} R {:.2}  steps {}  lines/step {:.1}  >{MAX_LINE} chars {}  fell back {}\n",
                ratio(s.hit, s.got),
                ratio(s.hit, s.want),
                s.steps,
                ratio(s.lines, s.steps.max(1)),
                s.long,
                s.invalid.len()
            ));
            for i in &s.invalid {
                out.push_str(&format!("    fell back: {i}\n"));
            }
            if r == 1 {
                for m in &s.misses {
                    out.push_str(m);
                    out.push('\n');
                }
            }
            sets.push(folded.iter().flat_map(|f| f.runs.iter().map(|(a, b)| (f.path.clone(), a.clone(), b.clone()))).collect());
        }
        if sets.len() > 1 {
            let inter = sets.iter().skip(1).fold(sets[0].clone(), |acc, s| acc.intersection(s).cloned().collect());
            let union = sets.iter().fold(BTreeSet::new(), |acc, s| acc.union(s).cloned().collect::<BTreeSet<_>>());
            out.push_str(&format!("  stability {:.2} (boundaries shared by every run)\n", ratio(inter.len(), union.len())));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HANDOVER: &str = include_str!("../tests/pseudo/HandOverService.java");

    fn method<'a>(ms: &'a [Method], name: &str) -> &'a Method {
        ms.iter().find(|m| m.path.ends_with(name)).unwrap()
    }

    fn ids(units: &[Unit]) -> Vec<String> {
        let mut all = Vec::new();
        walk_units(units, &mut all);
        all.iter().map(|u| u.id.clone()).collect()
    }

    fn step(from: &str, to: &str, depth: usize, text: &str) -> Step {
        Step { from: from.into(), to: to.into(), depth, text: text.into() }
    }

    #[test]
    fn units_split_try_catch_and_keep_structural_lines_inside_parts() {
        let ms = methods(HANDOVER).unwrap();
        let h = method(&ms, ".handOver");
        assert_eq!(h.path, "HandOverService.handOver");
        let top: Vec<&str> = h.units.iter().map(|u| u.id.as_str()).collect();
        assert_eq!(top, ["1", "2", "3", "4", "5", "6", "7"]);
        let t = &h.units[3];
        let parts: Vec<&str> = t.children.iter().map(|u| u.id.as_str()).collect();
        assert_eq!(parts, ["4.try", "4.catch1"]);
        assert!(HANDOVER[t.children[0].start..t.children[0].end].starts_with("try {"));
        assert!(HANDOVER[t.children[0].start..t.children[0].end].ends_with("ended.outcome());"));
        let catch = HANDOVER[t.children[1].start..t.children[1].end].trim_start();
        assert_eq!(t.children[1].lead, "        ", "the catch line folds from its start, indented like the try");
        assert!(catch.starts_with("} catch (StagingStoreUnavailableException"), "{catch}");
        assert!(catch.ends_with('}'));
        assert_eq!(t.children[1].end, t.end);

        let c = method(&ms, ".handOverClaimed");
        assert_eq!(ids(&c.units), ["1", "2", "2.1", "3", "3.try", "3.try.1", "3.try.2", "3.catch1", "3.catch1.1", "3.catch2", "3.catch2.1"]);
    }

    #[test]
    fn if_chain_and_switch_parts() {
        let src = "class A {\n  int f(int x) {\n    // first\n    if (x > 1) {\n      a();\n    } else if (x < 0) {\n      b();\n    } else {\n      c();\n    }\n    return switch (x) {\n      case 1 -> 2;\n      default -> {\n        yield 3;\n      }\n    };\n  }\n  void a() {} void b() {} void c() {}\n}\n";
        let ms = methods(src).unwrap();
        let f = method(&ms, ".f");
        assert_eq!(ids(&f.units), ["1", "1.if", "1.if.1", "1.elif1", "1.elif1.1", "1.else", "1.else.1", "2", "2.case1", "2.case2", "2.case2.1"]);
        assert!(src[f.units[0].start..].starts_with("// first"), "leading comment belongs to the statement");
        let p = &f.units[0].children;
        assert!(src[p[1].start..p[1].end].trim_start().starts_with("} else if"));
        assert!(src[p[2].start..p[2].end].trim_start().starts_with("} else {") && src[p[2].start..p[2].end].ends_with('}'));
        assert!(src[f.units[1].children[1].start..f.units[1].children[1].end].ends_with("};"));
    }

    #[test]
    fn reference_folding_renders_as_planned() {
        let ms = methods(HANDOVER).unwrap();
        let answers = vec![Answer {
            path: "HandOverService.handOver".into(),
            outline: "claim, hand to handler, record outcome → receipt".into(),
            steps: vec![
                step("1", "2", 0, "claim message RECEIVED|FAILED → PROCESSING; else NotFound | NotAllowed"),
                step("3", "3", 0, "hand to route's handler → HandedOver | Rejected | Failed"),
                step("4.try", "4.try", 0, "record outcome"),
                step("4.catch1", "4.catch1", 1, "store down → stays PROCESSING; log, return"),
                step("5", "7", 0, "log hand-over, return receipt"),
            ],
        }];
        let folded = fold_all(HANDOVER, &ms, &answers);
        let h = folded.iter().find(|f| f.path == "HandOverService.handOver").unwrap();
        assert_eq!(h.fallback, None);
        let text = render(HANDOVER, &steps_only(&folded), 0);
        assert!(text.contains(
            "    public StagedReceipt handOver(UUID id) {\n        claim message RECEIVED|FAILED → PROCESSING; else NotFound | NotAllowed\n        hand to route's handler → HandedOver | Rejected | Failed\n        record outcome\n          store down → stays PROCESSING; log, return\n        log hand-over, return receipt\n    }\n"
        ), "{text}");
        assert!(h.folds.iter().all(|f| f.start > h.body.0 && f.end < h.body.1));
    }

    fn steps_only(folded: &[Folded]) -> Vec<Fold> {
        folded.iter().flat_map(|f| f.folds.clone()).collect()
    }

    #[test]
    fn zoom_levels_class_then_methods_then_steps() {
        let ms = methods(HANDOVER).unwrap();
        let answers = vec![
            Answer { path: "HandOverService".into(), outline: "hands staged messages over".into(), steps: vec![] },
            Answer {
                path: "HandOverService.handOver".into(),
                outline: "claim, hand over, record → receipt".into(),
                steps: vec![step("1", "3", 0, "claim and hand over"), step("4", "7", 0, "record, return receipt")],
            },
        ];
        let folded = fold_all(HANDOVER, &ms, &answers);
        let folds = all_folds(HANDOVER, &folded, &answers);
        let z0 = render(HANDOVER, &folds, 0);
        assert!(z0.starts_with("package com.example.staging.application.service\n\nclass HandOverService: hands staged messages over\n"), "{z0}");
        let z1 = render(HANDOVER, &folds, 1);
        assert!(z1.contains("class HandOverService implements HandOverPort\n\n    constants TECHNICAL_FAILURE, NO_HANDLER\n\n    uses statusPort, handlers, auditCorrelation, log\n\n    handOver(id): claim, hand over, record → receipt\n"), "{z1}");
        assert!(!z1.contains("@Override") && !z1.contains("/**") && !z1.contains("requireNonNull"), "{z1}");
        let z2 = render(HANDOVER, &folds, 2);
        assert!(z2.contains("    handOver(id) → StagedReceipt {\n        claim and hand over\n        record, return receipt\n    }\n"), "{z2}");
        assert!(z2.contains("    record Ended(outcome, cause)\n}"), "{z2}");
        // Every zoomable fold nests inside the class fold; detail folds never open by zooming.
        let class = folds.iter().find(|f| f.kind == Kind::Class).unwrap();
        assert_eq!(zoom_of(&folds, class), 0);
        assert!(folds.iter().filter(|f| f.kind == Kind::Step).all(|f| zoom_of(&folds, f) == 2));
        assert!(folds.iter().filter(|f| f.kind == Kind::Detail).all(|f| zoom_of(&folds, f) == u8::MAX));
    }

    #[test]
    fn bad_steps_fall_back_to_the_baseline() {
        let ms = methods(HANDOVER).unwrap();
        let h = method(&ms, ".handOver");
        let ok = |s: Vec<Step>| folds(h, &s);
        assert!(ok(vec![step("1", "7", 0, "all")]).is_ok());
        assert!(ok(vec![step("1", "9", 0, "x")]).unwrap_err().contains("unknown"));
        assert!(ok(vec![step("1", "4.try", 0, "x"), step("4.catch1", "7", 0, "y")]).unwrap_err().contains("siblings"));
        assert!(ok(vec![step("1", "4", 0, "x"), step("4", "7", 0, "y")]).unwrap_err().contains("two steps"));
        assert!(ok(vec![step("1", "3", 0, "x"), step("5", "7", 0, "y")]).unwrap_err().contains("not covered"));
        assert!(ok(vec![step("1", "4", 0, "x"), step("4.try", "4.try", 0, "y"), step("5", "7", 0, "z")]).unwrap_err().contains("both"));
        assert!(ok(vec![step("3", "1", 0, "x")]).unwrap_err().contains("backwards"));
        assert!(ok(vec![step("1", "3", 0, "x"), step("4", "4.try", 0, "y"), step("4.catch1", "4.catch1", 1, "z"), step("5", "7", 0, "w")]).is_ok(), "4..4.try reads as 4.try");

        let answers = vec![Answer { path: h.path.clone(), outline: "o".into(), steps: vec![step("1", "3", 0, "x")] }];
        let f = fold_all(HANDOVER, &ms, &answers).into_iter().find(|f| f.path == h.path).unwrap();
        assert!(f.fallback.unwrap().contains("not covered"));
        // Baseline: the requireNonNull guard joins the claim, then one step per statement.
        assert_eq!(f.runs, [("1", "2"), ("3", "3"), ("4", "4"), ("5", "5"), ("6", "6"), ("7", "7")].map(|(a, b)| (a.to_string(), b.to_string())));
    }

    #[test]
    fn single_step_methods_show_their_outline_only() {
        let ms = methods(HANDOVER).unwrap();
        let answers = vec![Answer { path: "HandOverService.statusOf".into(), outline: "HandedOver→HANDED_OVER".into(), steps: vec![step("1", "1", 0, "map")] }];
        let f = fold_all(HANDOVER, &ms, &answers);
        let s = f.iter().find(|f| f.path == "HandOverService.statusOf").unwrap();
        assert!(s.folds.is_empty() && s.fallback.is_none());
        assert!(render(HANDOVER, &all_folds(HANDOVER, &f, &answers), 2).contains("statusOf(outcome) → StagedMessageStatus { HandedOver→HANDED_OVER }"));
    }

    #[test]
    fn tokens_follow_the_java_palette() {
        let names = Names {
            params: ["outcome", "cause", "id"].map(String::from).into(),
            fields: ["statusPort"].map(String::from).into(),
            methods: ["report"].map(String::from).into(),
        };
        let kinds = |t: &str| -> Vec<(String, &'static str)> {
            let u: Vec<u16> = t.encode_utf16().collect();
            tokens(t, &names).into_iter().map(|(s, e, k)| (String::from_utf16(&u[s..e]).unwrap(), k)).collect()
        };
        assert_eq!(kinds("claim; record a result"), [(";".to_string(), "operator")]);
        assert_eq!(kinds("class A implements B"), [("class", "keyword"), ("A", "type"), ("implements", "keyword"), ("B", "type")].map(|(a, b)| (a.to_string(), b)));
        assert_eq!(kinds("record Ended(outcome, cause)"), [("record", "keyword"), ("Ended", "type"), ("outcome", "parameter"), ("cause", "parameter")].map(|(a, b)| (a.to_string(), b)));
        assert_eq!(
            kinds("    handOver(id): claim via statusPort → HANDED_OVER | Rejected; report it"),
            [("handOver", "method_decl"), ("id", "parameter"), ("statusPort", "field"), ("→", "operator"), ("HANDED_OVER", "constant"), ("|", "operator"), ("Rejected", "type"), (";", "operator"), ("report", "method_call")].map(|(a, b)| (a.to_string(), b))
        );
        assert_eq!(kinds("@PostMapping(\"/pay\") new A(x)")[..2], [("@PostMapping".to_string(), "annotation"), ("\"/pay\"".to_string(), "string")]);
        // UTF-16: the arrow is one unit, an emoji two.
        let t = tokens("😀 → X", &names);
        assert_eq!(t, [(3, 4, "operator"), (5, 6, "type")]);
    }

    #[test]
    fn wire_offsets_are_utf16() {
        let src = "class A {\n  void f() {\n    String s = \"é😀\";\n    g(s);\n  }\n  void g(String s) {}\n}\n";
        let ms = methods(src).unwrap();
        let a = vec![Answer { path: "A.f".into(), outline: "o".into(), steps: vec![step("1", "1", 0, "x"), step("2", "2", 0, "y")] }];
        let w = wire(src, &[], &fold_all(src, &ms, &a));
        let f = &w["methods"][0]["folds"][1];
        let byte = src.find("g(s)").unwrap();
        assert_eq!(f["start"].as_u64().unwrap() as usize, src[..byte].encode_utf16().count());
        assert_ne!(byte, src[..byte].encode_utf16().count());
    }

    #[test]
    fn cache_and_stdin_through_a_fake_claude() {
        let dir = std::env::temp_dir().join(format!("wish-pseudo-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let count = dir.join("count");
        let fake = dir.join("claude");
        let answer = json!({"structured_output": {"outlines": [{"path": "A.f", "outline": "does f"}], "methods": [{"path": "A.f", "steps": [{"from": "1", "to": "1", "depth": 0, "text": "one"}, {"from": "2", "to": "2", "depth": 0, "text": "two"}]}]}});
        std::fs::write(&fake, format!("#!/bin/sh\ncat > /dev/null\necho x >> '{}'\necho '{}'\n", count.display(), answer)).unwrap();
        std::process::Command::new("chmod").arg("+x").arg(&fake).status().unwrap();
        let src = "class A {\n  void f() {\n    a();\n    b();\n  }\n  void a() {} void b() {}\n}\n";
        let ms = methods(src).unwrap();
        // SAFETY: tests touching these variables only run here.
        unsafe {
            std::env::set_var("WISH_CLAUDE", &fake);
            std::env::set_var("WISHLANG_HOME", &dir);
        }
        let first = answers(src, &ms, "test").unwrap();
        let second = answers(src, &ms, "test").unwrap();
        assert_eq!(first[0].outline, "does f");
        assert_eq!(second[0].steps, first[0].steps);
        assert_eq!(std::fs::read_to_string(&count).unwrap().lines().count(), 1, "second call hits the cache");
        let text = render(src, &steps_only(&fold_all(src, &ms, &first)), 0);
        assert!(text.contains("  void f() {\n    one\n    two\n  }"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
