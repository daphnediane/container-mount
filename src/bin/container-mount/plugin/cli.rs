/*
 * Copyright (c) 2026 Daphne Pfister
 * SPDX-License-Identifier: BSD-2-Clause
 * See LICENSE file for full license text
 */

//! The `container mount` command line, as pure functions: the verb
//! table, `TARGET:PATH` references, and guest path expansion
//! (docs/design/mount-backends.md "The `container mount` plugin").
//!
//! A first word that is a verb is a subcommand; anything else is the
//! TARGET of an implied `attach`, and `--` makes the rest TARGETs
//! whatever they spell. The implied form is interactive shorthand —
//! scripts and embedders name the verb and use `--`, so a verb added
//! later can't capture a container of that name for them.

/// Verbs implemented in this release.
pub const VERBS: &[&str] = &["cat", "copy", "cp", "help", "list", "ls", "stat"];

/// Verbs reserved for later — refused with "not implemented yet" instead
/// of being taken for a TARGET.
pub const RESERVED: &[&str] = &[
    "attach", "chmod", "chown", "detach", "df", "du", "edit", "events", "export", "find", "import",
    "inspect", "ln", "mkdir", "mv", "open", "prepare", "prune", "rm", "rmdir", "snapshot", "sync",
    "touch", "xattr",
];

/// What the first word names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Word {
    Verb,
    Reserved,
    /// Not a verb: the TARGET of an implied `attach`.
    Target,
}

#[must_use]
pub fn classify(word: &str) -> Word {
    if VERBS.contains(&word) {
        Word::Verb
    } else if RESERVED.contains(&word) {
        Word::Reserved
    } else {
        Word::Target
    }
}

/// Every verb `word` is one edit away from, for "did you mean".
#[must_use]
pub fn near_verbs(word: &str) -> Vec<&'static str> {
    VERBS
        .iter()
        .chain(RESERVED)
        .copied()
        .filter(|v| v.len() > 1 && edit_distance(word, v) == 1)
        .collect()
}

fn edit_distance(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut prev = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cur = row[j + 1];
            row[j + 1] = (prev + usize::from(ca != cb)).min(row[j] + 1).min(cur + 1);
            prev = cur;
        }
    }
    row[b.len()]
}

/// One side of a copy, or the operand of `ls`/`stat`/`cat`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ref {
    /// A host path (`-` is stdin/stdout where the verb allows it).
    Local(String),
    Remote(Remote),
}

/// `[USER@]TARGET:PATH`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Remote {
    /// This reference's own user, overriding `--user`.
    pub user: Option<String>,
    pub target: String,
    /// May be empty, relative, or `~`-prefixed.
    pub path: String,
}

/// `[USER@]TARGET:PATH` when the text before the first `:` is a
/// non-empty, slash-free name — container ids and machine names can't
/// contain `:` or `@` — else a local path (scp's rule: write `./a:b`
/// for a local name with a colon). The first `@` splits off the user,
/// so a gid can't ride along (`--user NAME:GID` can).
#[must_use]
pub fn parse_ref(s: &str) -> Ref {
    let Some((head, path)) = s.split_once(':') else {
        return Ref::Local(s.into());
    };
    if head.is_empty() || head.contains('/') {
        return Ref::Local(s.into());
    }
    let (user, target) = match head.split_once('@') {
        Some((u, t)) if !u.is_empty() && !t.is_empty() => (Some(u.into()), t.into()),
        Some(_) => return Ref::Local(s.into()),
        None => (None, head.into()),
    };
    Ref::Remote(Remote {
        user,
        target,
        path: path.into(),
    })
}

/// How a remote path starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Base<'a> {
    /// Absolute: from the guest root.
    Root(&'a str),
    /// Relative, empty, or `~`/`~/…`: from the session user's home.
    Home(&'a str),
    /// `~NAME` or `~NAME/…`: from NAME's home.
    UserHome { user: &'a str, rest: &'a str },
}

/// Classify a remote path (scp's rules for relative and `~` paths).
#[must_use]
pub fn base(path: &str) -> Base<'_> {
    if path.starts_with('/') {
        return Base::Root(path);
    }
    let Some(tilde) = path.strip_prefix('~') else {
        return Base::Home(path);
    };
    let (user, rest) = tilde.split_once('/').unwrap_or((tilde, ""));
    if user.is_empty() {
        Base::Home(rest)
    } else {
        Base::UserHome { user, rest }
    }
}

/// `home` joined with a relative `rest` (empty `rest` is the home).
#[must_use]
pub fn join(home: &[u8], rest: &str) -> String {
    let home = String::from_utf8_lossy(home);
    let home = home.trim_end_matches('/');
    match rest.trim_start_matches('/') {
        "" if home.is_empty() => "/".into(),
        "" => home.into(),
        r => format!("{home}/{r}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // cspell:ignore lst copi stta

    #[test]
    fn test_verbs_and_reserved_are_disjoint_and_sorted() {
        assert!(VERBS.iter().all(|v| !RESERVED.contains(v)));
        assert!(VERBS.is_sorted() && RESERVED.is_sorted());
        assert_eq!(classify("ls"), Word::Verb);
        assert_eq!(classify("attach"), Word::Reserved);
        assert_eq!(classify("web"), Word::Target);
    }

    #[test]
    fn test_near_verb_catches_typos_only() {
        assert_eq!(near_verbs("lst"), ["list", "ls"]);
        assert_eq!(near_verbs("copi"), ["copy"]);
        assert!(near_verbs("stta").is_empty());
        assert!(near_verbs("webserver").is_empty());
    }

    #[test]
    fn test_parse_ref_follows_scp() {
        let remote = |t: &str, p: &str| {
            Ref::Remote(Remote {
                user: None,
                target: t.into(),
                path: p.into(),
            })
        };
        assert_eq!(parse_ref("web:/etc/hosts"), remote("web", "/etc/hosts"));
        assert_eq!(parse_ref("web:notes.txt"), remote("web", "notes.txt"));
        assert_eq!(parse_ref("web:"), remote("web", ""));
        // Only the first colon splits.
        assert_eq!(parse_ref("web:a:b"), remote("web", "a:b"));
        assert_eq!(
            parse_ref("alice@web:notes.txt"),
            Ref::Remote(Remote {
                user: Some("alice".into()),
                target: "web".into(),
                path: "notes.txt".into(),
            })
        );
        for local in [
            "notes.txt",
            "./a:b",
            "/tmp/a:b",
            ":x",
            "-",
            "@web:x",
            "alice@:x",
        ] {
            assert_eq!(parse_ref(local), Ref::Local(local.into()), "{local}");
        }
    }

    #[test]
    fn test_base_and_join() {
        assert_eq!(base("/etc"), Base::Root("/etc"));
        assert_eq!(base("notes.txt"), Base::Home("notes.txt"));
        assert_eq!(base(""), Base::Home(""));
        assert_eq!(base("~"), Base::Home(""));
        assert_eq!(base("~/a/b"), Base::Home("a/b"));
        assert_eq!(
            base("~bob"),
            Base::UserHome {
                user: "bob",
                rest: ""
            }
        );
        assert_eq!(
            base("~bob/x"),
            Base::UserHome {
                user: "bob",
                rest: "x"
            }
        );
        assert_eq!(join(b"/home/me", "a/b"), "/home/me/a/b");
        assert_eq!(join(b"/home/me/", ""), "/home/me");
        assert_eq!(join(b"/", "x"), "/x");
        assert_eq!(join(b"/", ""), "/");
    }
}
