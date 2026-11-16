//! The program menu (the round gear button at the bottom left): categories and the programs
//! in them. Stored per user in `~/.config/appmenu.cfg`:
//!
//! ```text
//! [Media]
//! Player=/apps/player.lua
//! [Mine]
//! Notes=/home/user/notes.lua
//! !/apps/hello.lua
//! ```
//!
//! `[name]` is a category, `Title=path` a program, `!path` — removed from the menu.
//! Programs from /apps that are not in the file appear by themselves: in the category from their
//! `category = "…"` (or a `-- category: …` comment), otherwise in "Other". A removed
//! program from /apps is remembered, otherwise it would come back on the next opening.

use crate::fs;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

pub const CONFIG: &str = "appmenu.cfg";
/// The category for programs that did not say where they belong.
pub const OTHER: &str = "Other";
const APPS: &str = "/apps";
/// How much of the start of a file to look at for the title and category.
const HINT_BYTES: usize = 4096;

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Entry {
    pub title: String,
    pub path: String,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Category {
    pub name: String,
    pub entries: Vec<Entry>,
}

#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct AppMenu {
    pub categories: Vec<Category>,
    /// Programs from /apps removed from the menu.
    pub hidden: Vec<String>,
}

/// The title and category from the program text: `title = "…"`, `category = "…"` in
/// the app table or a `-- category: …` comment.
pub fn hints(path: &str) -> (Option<String>, Option<String>) {
    let Ok((head, _)) = fs::read_range(path, 0, HINT_BYTES) else { return (None, None) };
    let text = String::from_utf8_lossy(&head);
    let quoted = |key: &str| -> Option<String> {
        let mut rest = &text[..];
        while let Some(at) = rest.find(key) {
            let before = rest[..at].chars().next_back();
            let after = rest[at + key.len()..].trim_start();
            rest = &rest[at + key.len()..];
            // A whole word, then "=" and a quoted string.
            if before.is_some_and(|c| c.is_alphanumeric() || c == '_') {
                continue;
            }
            let Some(value) = after.strip_prefix('=').map(str::trim_start) else { continue };
            let quote = value.chars().next()?;
            if quote != '"' && quote != '\'' {
                continue;
            }
            let end = value[1..].find(quote)?;
            let v = value[1..1 + end].trim();
            if !v.is_empty() {
                return Some(String::from(v));
            }
        }
        None
    };
    let comment = text.lines().find_map(|l| l.trim().strip_prefix("--").map(str::trim).and_then(|l| l.strip_prefix("category:")).map(|c| String::from(c.trim()))).filter(|c| !c.is_empty());
    (quoted("title"), quoted("category").or(comment))
}

/// The default program title: from its `title`, otherwise the file name capitalized.
pub fn title_of(path: &str) -> String {
    if let (Some(t), _) = hints(path) {
        return t;
    }
    let name = fs::name(path).trim_end_matches(".lua");
    let mut chars = name.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::from(name),
    }
}

/// A name without whatever would break the menu file.
fn clean(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).map(|c| if matches!(c, '[' | ']' | '=') { '-' } else { c }).collect::<String>().trim().into()
}

impl AppMenu {
    /// The user's menu: the file and everything new from /apps.
    pub fn load() -> AppMenu {
        let path = crate::access::config(CONFIG);
        let mut m = fs::read(&path).map(|b| AppMenu::parse(&String::from_utf8_lossy(&b))).unwrap_or_default();
        // The program's file is gone — remove it from the menu.
        for c in &mut m.categories {
            c.entries.retain(|e| fs::exists(&e.path));
        }
        m.scan();
        m
    }

    pub fn save(&self) {
        let _ = fs::write(&crate::access::config(CONFIG), self.to_text().as_bytes());
    }

    fn parse(text: &str) -> AppMenu {
        let mut m = AppMenu::default();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                let name = clean(name);
                if !name.is_empty() && m.find(&name).is_none() {
                    m.categories.push(Category { name, entries: Vec::new() });
                }
            } else if let Some(path) = line.strip_prefix('!') {
                m.hidden.push(String::from(path.trim()));
            } else if let Some((title, path)) = line.split_once('=') {
                let (title, path) = (title.trim(), path.trim());
                if path.starts_with('/') && !m.contains(path) {
                    if m.categories.is_empty() {
                        m.categories.push(Category { name: String::from(OTHER), entries: Vec::new() });
                    }
                    let last = m.categories.len() - 1;
                    m.categories[last].entries.push(Entry { title: String::from(title), path: String::from(path) });
                }
            }
        }
        m
    }

    pub fn to_text(&self) -> String {
        let mut out = String::from("# GlassOS app menu: [category], Title=/path, !/path = removed from the menu\n");
        for c in &self.categories {
            out.push_str(&format!("[{}]\n", c.name));
            for e in &c.entries {
                out.push_str(&format!("{}={}\n", clean(&e.title), e.path));
            }
        }
        for h in &self.hidden {
            out.push_str(&format!("!{h}\n"));
        }
        out
    }

    /// New programs from /apps — into the menu.
    fn scan(&mut self) {
        let mut found: Vec<String> = fs::list(APPS)
            .unwrap_or_default()
            .into_iter()
            .filter(|e| !e.is_dir && crate::lua::is_program(&e.name))
            .map(|e| format!("{APPS}/{}", e.name))
            .collect();
        found.sort();
        for path in found {
            if self.contains(&path) || self.hidden.contains(&path) {
                continue;
            }
            let (title, category) = hints(&path);
            let title = title.unwrap_or_else(|| title_of(&path));
            let category = category.map(|c| clean(&c)).filter(|c| !c.is_empty()).unwrap_or_else(|| String::from(OTHER));
            self.category_mut(&category).entries.push(Entry { title, path });
        }
    }

    pub fn find(&self, name: &str) -> Option<usize> {
        self.categories.iter().position(|c| c.name.eq_ignore_ascii_case(name))
    }

    /// A category by name (if missing, it is created at the end).
    pub fn category_mut(&mut self, name: &str) -> &mut Category {
        let i = match self.find(name) {
            Some(i) => i,
            None => {
                self.categories.push(Category { name: clean(name), entries: Vec::new() });
                self.categories.len() - 1
            }
        };
        &mut self.categories[i]
    }

    pub fn contains(&self, path: &str) -> bool {
        self.categories.iter().any(|c| c.entries.iter().any(|e| e.path == path))
    }

    /// Add a program to a category (if it is already somewhere, it is moved).
    pub fn add(&mut self, category: &str, path: &str) {
        let old = self.categories.iter().flat_map(|c| c.entries.iter()).find(|e| e.path == path).map(|e| e.title.clone());
        self.remove_entry(path);
        self.hidden.retain(|h| h != path);
        let title = old.unwrap_or_else(|| title_of(path));
        self.category_mut(category).entries.push(Entry { title, path: String::from(path) });
    }

    fn remove_entry(&mut self, path: &str) {
        for c in &mut self.categories {
            c.entries.retain(|e| e.path != path);
        }
    }

    /// Remove a program from the menu (the file itself stays).
    pub fn remove(&mut self, path: &str) {
        self.remove_entry(path);
        if path.starts_with(&format!("{APPS}/")) && !self.hidden.iter().any(|h| h == path) {
            self.hidden.push(String::from(path));
        }
    }

    /// A new category with an unused name; returns its index.
    pub fn new_category(&mut self, base: &str) -> usize {
        let mut name = clean(base);
        let mut n = 2;
        while self.find(&name).is_some() {
            name = format!("{} {n}", clean(base));
            n += 1;
        }
        self.categories.push(Category { name, entries: Vec::new() });
        self.categories.len() - 1
    }

    /// Rename; false — the name is empty or taken by another category.
    pub fn rename(&mut self, i: usize, name: &str) -> bool {
        let name = clean(name);
        if name.is_empty() || self.find(&name).is_some_and(|j| j != i) || i >= self.categories.len() {
            return false;
        }
        self.categories[i].name = name;
        true
    }

    /// Delete a category: its programs move to "Other" (they are not lost).
    pub fn delete_category(&mut self, i: usize) {
        if i >= self.categories.len() {
            return;
        }
        let c = self.categories.remove(i);
        if !c.entries.is_empty() {
            let other = if c.name.eq_ignore_ascii_case(OTHER) { "Other programs" } else { OTHER };
            self.category_mut(other).entries.extend(c.entries);
        }
    }

    /// Move a category by `step` (−1 up, +1 down). The new index.
    pub fn move_category(&mut self, i: usize, step: i32) -> usize {
        let j = (i as i32 + step).clamp(0, self.categories.len() as i32 - 1) as usize;
        if i < self.categories.len() && i != j {
            self.categories.swap(i, j);
        }
        j
    }

    /// Move a program within its category. The new index.
    pub fn move_entry(&mut self, c: usize, i: usize, step: i32) -> usize {
        let Some(cat) = self.categories.get_mut(c) else { return i };
        let j = (i as i32 + step).clamp(0, cat.entries.len() as i32 - 1) as usize;
        if i < cat.entries.len() && i != j {
            cat.entries.swap(i, j);
        }
        j
    }
}
