//! 静态守卫：禁止「条件里借用 `RefCell`、块内再借可变引用」的写法。
//!
//! 这种写法会直接 panic「RefCell already borrowed」，因为 `if` / `while` / `match`
//! 条件里创建的临时 `Ref` 会活到**整个块结束**，块内 `borrow_mut()` 立刻撞车。
//! 已经犯过两次（切换默认压缩软件必崩、首次启动居中），所以进测试守住：
//! 要么把判定值先取到局部变量、用独立一条语句结束借用，要么把借用挪进闭包。
//!
//! 扫的是源码文本（`src/**/*.rs`），是启发式检查：只认「同一行条件里出现
//! `x.borrow()`，其块内出现 `x.borrow_mut()`」。跨行条件、宏里生成的代码它
//! 看不见 —— 但能挡住绝大多数手写出来的同类错误。

use std::fs;
use std::path::{Path, PathBuf};

/// 仓库里不许出现这种写法。
#[test]
fn repository_has_no_borrow_in_condition() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut offenders: Vec<String> = Vec::new();
    visit(&root, &mut offenders);
    assert!(
        offenders.is_empty(),
        "以下位置在条件里借用、块内又借可变引用，真机必 panic「RefCell already borrowed」：\n{}\n\
         改法：把判定值先取到局部变量（用一条独立语句结束借用）再进块。",
        offenders.join("\n")
    );
}

/// 检查器自身要有牙齿：真的能认出这种写法，也别把安全的写法算进来。
#[test]
fn scanner_detects_the_panic_pattern() {
    let bad = "\
fn f(app: &Rc<RefCell<App>>) {
    if app.borrow().pending {
        app.borrow_mut().pending = false;
    }
}
";
    let bad_iflet = "\
fn f(app: &Rc<RefCell<App>>) {
    if let Some(x) = app.borrow().thing() {
        let mut a = app.borrow_mut();
        a.use_it(x);
    }
}
";
    let bad_match = "\
fn f(app: &Rc<RefCell<App>>) {
    match app.borrow().state {
        0 => app.borrow_mut().tick(),
        _ => {}
    }
}
";
    let good_local = "\
fn f(app: &Rc<RefCell<App>>) {
    let pending = app.borrow().pending;
    if pending {
        app.borrow_mut().pending = false;
    }
}
";
    let good_closure = "\
fn f(app: &Rc<RefCell<App>>) {
    if SESSION.with(|slot| slot.borrow().is_some()) {
        app.borrow_mut().go();
    }
}
";
    let good_other_cell = "\
fn f(app: &Rc<RefCell<App>>, other: &RefCell<u8>) {
    if app.borrow().pending {
        *other.borrow_mut() = 1;
    }
}
";
    for (name, source, expect) in [
        ("条件里借用 + 块内 borrow_mut", bad, true),
        ("if let 写法", bad_iflet, true),
        ("match 写法", bad_match, true),
        ("先取局部值", good_local, false),
        ("借用只在闭包里", good_closure, false),
        ("块内借的是别的 RefCell", good_other_cell, false),
    ] {
        let hits = scan_source(Path::new("<test>"), source);
        assert_eq!(!hits.is_empty(), expect, "判定错误：{name}\n{hits:?}");
    }
}

fn visit(dir: &Path, out: &mut Vec<String>) {
    let Ok(list) = fs::read_dir(dir) else { return };
    let mut entries: Vec<PathBuf> = list.flatten().map(|e| e.path()).collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            visit(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            // 本文件的测试夹具里就有这种写法（故意写坏），扫自己会自相矛盾。
            if path.file_name().and_then(|n| n.to_str()) == Some("refcell_guard.rs") {
                continue;
            }
            if let Ok(text) = fs::read_to_string(&path) {
                out.extend(scan_source(&path, &text));
            }
        }
    }
}

fn scan_source(path: &Path, source: &str) -> Vec<String> {
    let lines: Vec<&str> = source.lines().collect();
    let mut hits = Vec::new();
    for (index, raw) in lines.iter().enumerate() {
        let code = strip_comment(raw);
        let trimmed = code.trim_start();
        let is_condition = ["if ", "if let ", "while ", "while let ", "match "]
            .iter()
            .any(|prefix| trimmed.starts_with(prefix));
        if !is_condition {
            continue;
        }
        // 借用写在闭包里（`SESSION.with(|slot| slot.borrow()…)`）不会跨到块里。
        if code.contains(".with(|") || code.contains("|slot|") {
            continue;
        }
        let Some(receiver) = borrowed_receiver(&code) else {
            continue;
        };
        let needle = format!("{receiver}.borrow_mut()");

        // 从条件这一行开始按大括号配平找块的范围；条件里的 `{` 就是块的开头。
        let mut depth = 0i32;
        let mut entered = false;
        for (offset, line) in lines.iter().enumerate().skip(index).take(400) {
            let line_code = strip_comment(line);
            if entered && offset > index && line_code.contains(&needle) {
                hits.push(format!(
                    "{}:{}: {}",
                    path.display(),
                    offset + 1,
                    line.trim()
                ));
                break;
            }
            for ch in line_code.chars() {
                match ch {
                    '{' => {
                        depth += 1;
                        entered = true;
                    }
                    '}' => depth -= 1,
                    _ => {}
                }
            }
            if entered && depth <= 0 {
                break;
            }
        }
    }
    hits
}

/// 条件里第一个 `X.borrow()` 的 `X`（含 `self.` / 链式字段）。
fn borrowed_receiver(code: &str) -> Option<String> {
    let at = code.find(".borrow()")?;
    let head = &code[..at];
    let start = head
        .rfind(|c: char| !(c.is_alphanumeric() || c == '_' || c == '.'))
        .map(|i| i + 1)
        .unwrap_or(0);
    let name = head[start..].trim();
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

fn strip_comment(line: &str) -> String {
    match line.find("//") {
        Some(at) => line[..at].to_string(),
        None => line.to_string(),
    }
}
