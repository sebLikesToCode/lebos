//! The LeBOS shell.
//!
//! It runs in USER MODE. Everything it does -- parsing a line, deciding what a
//! predicate means, formatting a table, choosing what `hide` is short for --
//! crosses into the kernel only as a validated syscall. Until milestone 20 all
//! of this lived in supervisor mode with unrestricted access to physical
//! memory, because the kernel was the only place with a heap.
//!
//! The design, unchanged from when it was kernel code, because it was never
//! about privilege: every other shell ever written resolves a NAME to a
//! LOCATION. `cat notes.txt` means walk the tree to this leaf. This one cannot,
//! so an argument is one of exactly two things:
//!
//!     a QUERY                      find type=python created_at>100
//!     an INDEX into the last set   hide 2
//!
//! The numbered result list is the path replacement. Ephemeral, contextual, and
//! meaningless a minute later -- which is fine, because you are looking at it
//! while you use it.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::ToString;
use alloc::vec::Vec;

#[path = "../sys.rs"]
mod sys;
use sys::*;

/// The last result set. THIS IS THE PATH REPLACEMENT.
static mut LAST: Vec<u64> = Vec::new();

fn last() -> &'static mut Vec<u64> {
    // Single-threaded process, so no lock is needed -- and there is no lock to
    // reach for anyway, which is a good reminder of how much the kernel gets
    // to assume that a program does not.
    unsafe { &mut *core::ptr::addr_of_mut!(LAST) }
}

/// Parse one predicate: `key=value`, `key~substring`, `key>n`, `key<n`.
///
/// Four operators is not a limitation, it is the retrieval model. Eq narrows by
/// kind, `~` by name, `<` and `>` by time -- and time is the axis that narrows
/// hardest, which is exactly why attribute values are typed.
fn parse_pred(s: &str) -> Option<Cond> {
    for (i, ch) in s.char_indices() {
        // Short names for the attributes worth typing constantly. Aliasing a
        // LABEL is safe in a way aliasing a path never is: expand to an
        // attribute nothing has and the query returns nothing. There is no
        // wrong directory to end up in.
        let key = match &s[..i] {
            "" => continue,
            "t" => "created_at",
            "n" => "name",
            k => k,
        }
        .to_string();
        let rest = &s[i + ch.len_utf8()..];
        return Some(match ch {
            '=' => Cond::Eq(key, rest.to_string()),
            '~' => Cond::Contains(key, rest.to_string()),
            '>' => Cond::Between(key, rest.parse::<i64>().ok()? + 1, i64::MAX),
            '<' => Cond::Between(key, i64::MIN, rest.parse::<i64>().ok()? - 1),
            _ => continue,
        });
    }
    None
}

fn build_conds(words: &[&str], extra: Option<Cond>) -> Option<Vec<Cond>> {
    let mut conds = Vec::new();
    if let Some(c) = extra {
        conds.push(c);
    }
    for w in words {
        match parse_pred(w) {
            Some(c) => conds.push(c),
            None => {
                write(b"  `");
                write(w.as_bytes());
                write(b"` is not a predicate -- try type=python\n");
                return None;
            }
        }
    }
    Some(conds)
}

/// Run a query and remember the answer as the new result set.
fn find(conds: &[Cond]) {
    let ids = query(conds);
    if ids.is_empty() {
        write(b"  nothing matches\n");
    }
    for (i, id) in ids.iter().enumerate() {
        let Some(o) = get(*id, true) else { continue };
        write(b"  ");
        if i < 10 {
            write(b" ");
        }
        print_num(i as i64);
        write(b"  ");
        pad(o.text("name").unwrap_or("(unnamed)"), 18);
        write(b" ");
        pad(o.text("type").unwrap_or("-"), 9);
        write(b" t=");
        let t = o.int("created_at").unwrap_or(-1);
        print_num(t);
        pad("", if t < 0 { 4 } else { 6 - digits(t) });
        print_num(o.len as i64);
        write(b"b  #");
        print_hex(*id & 0xffff_ffff_ffff, 12);
        if o.len == 0 {
            write(b"  [evicted]");
        }
        write(b"\n");
    }
    *last() = ids;
}

fn digits(mut n: i64) -> usize {
    let mut d = 1;
    while n >= 10 {
        n /= 10;
        d += 1;
    }
    d
}

/// Turn "2" into the object it named in the last result set.
///
/// The error matters: an index is only meaningful relative to a query you just
/// ran, so saying so is more useful than "not found".
fn pick(arg: Option<&&str>) -> Option<u64> {
    let Some(n) = arg.and_then(|a| a.parse::<usize>().ok()) else {
        write(b"  that wants a number from the last result list\n");
        return None;
    };
    match last().get(n) {
        Some(id) => Some(*id),
        None => {
            write(b"  no ");
            print_num(n as i64);
            write(b" in the last result list (");
            print_num(last().len() as i64);
            write(b" shown)\n");
            None
        }
    }
}

fn show(id: u64) {
    let Some(o) = get(id, false) else {
        write(b"  gone -- forgotten, not merely hidden\n");
        return;
    };
    write(b"  id         #");
    print_hex(id, 16);
    write(b"\n  content    #");
    print_hex(o.content, 16);
    write(b"\n");
    for (k, v) in &o.attrs {
        write(b"  ");
        pad(k, 10);
        write(b" ");
        match v {
            Val::Int(n) => print_num(*n),
            Val::Text(t) => write(t.as_bytes()),
            Val::Other => write(b"(other)"),
        }
        write(b"\n");
    }
    if o.len == 0 {
        write(b"  ---- bytes evicted; the record still means something\n");
        return;
    }
    write(b"  ----\n  ");
    for &c in o.bytes.iter() {
        let printable = (0x20..0x7f).contains(&c) || c == b'\n';
        write(&[if printable { c } else { b'.' }]);
    }
    write(b"\n");
}

fn help() {
    write(b"  find <preds>     narrow the store. no preds = everything\n");
    write(b"  cluttered        the hidden ones. a saved query, not a folder\n");
    write(b"  show N           N indexes the LAST result list\n");
    write(b"  new <type> <name> <text...>\n");
    write(b"  run <preds>      a program is an object. running one is a query\n");
    write(b"  hide N | unhide N        clutter    -- reversible\n");
    write(b"  evict N                  space      -- bytes go, record stays\n");
    write(b"  forget N                 privacy    -- both go\n");
    write(b"  save | help | exit\n\n");
    write(b"  preds:  type=python   name~brick   created_at>100   t<200\n");
    write(b"          t = created_at, n = name\n");
    write(b"  there are no filenames to type, because there are no files.\n");
}

#[no_mangle]
extern "C" fn umain(_tag: usize) -> ! {
    write(b"\nLeBOS shell, running in user mode. There are no paths. Type `help`.\n");

    loop {
        write(b"> ");
        let line = read_line();
        let words: Vec<&str> = line.split_whitespace().collect();
        let Some(cmd) = words.first() else { continue };
        let rest = &words[1..];

        match *cmd {
            "help" | "?" => help(),

            "find" | "ls" => {
                if let Some(c) = build_conds(rest, None) {
                    find(&c);
                }
            }

            "cluttered" => find(&[Cond::Hidden(true)]),

            "new" => {
                if rest.len() < 3 {
                    write(b"  new <type> <name> <text...>\n");
                    continue;
                }
                let text = rest[2..].join(" ");
                match create(
                    text.as_bytes(),
                    &[
                        ("name", Val::Text(rest[1].to_string())),
                        ("type", Val::Text(rest[0].to_string())),
                    ],
                ) {
                    Some(id) => {
                        write(b"  #");
                        print_hex(id, 16);
                        write(b"\n");
                    }
                    None => write(b"  refused\n"),
                }
            }

            "show" | "cat" => {
                if let Some(id) = pick(rest.first()) {
                    show(id);
                }
            }

            // Silent on success: hiding destroys nothing, so it should not
            // demand attention. The three verbs get three different volumes,
            // because friction should match consequence.
            "hide" => {
                if let Some(id) = pick(rest.first()) {
                    verb(id, 1);
                }
            }
            "unhide" => {
                if let Some(id) = pick(rest.first()) {
                    verb(id, 0);
                }
            }
            "evict" => {
                if let Some(id) = pick(rest.first()) {
                    verb(id, 2);
                    write(b"  bytes gone. the record still answers questions.\n");
                }
            }
            "forget" => {
                if let Some(id) = pick(rest.first()) {
                    verb(id, 3);
                    write(b"  gone. that was the point.\n");
                }
            }

            // EXEC BY QUERY.
            //
            // Every OS that has shipped resolves a name to a location here.
            // There is no tree, so a program is an object with type=program and
            // running one means running a query. Ambiguity is therefore not an
            // error -- it is a numbered list, the same interface as everything
            // else, because it is the same operation.
            "run" => {
                let Some(conds) = build_conds(
                    rest,
                    Some(Cond::Eq("type".to_string(), "program".to_string())),
                ) else {
                    continue;
                };
                let ids = query(&conds);
                match ids.len() {
                    0 => write(b"  no program matches. `find type=program` to see them\n"),
                    1 => {
                        if spawn(ids[0]) {
                            match wait() {
                                Some(code) => {
                                    write(b"  exited with ");
                                    print_num(code as i64);
                                    write(b"\n");
                                }
                                None => write(b"  nothing to wait for\n"),
                            }
                        } else {
                            write(b"  not a program this machine can run\n");
                        }
                    }
                    n => {
                        write(b"  ");
                        print_num(n as i64);
                        write(b" programs match. narrow it:\n");
                        find(&conds);
                    }
                }
            }

            "save" => write(if save() {
                b"  written to disk\n" as &[u8]
            } else {
                b"  SAVE FAILED\n"
            }),

            "exit" => {
                write(b"  the shell is a program now, so this is a real exit.\n");
                exit(0);
            }

            // The commands people reach for out of muscle memory. Every one is
            // a path operation, and none can mean anything here -- so say why
            // rather than "command not found".
            "cd" | "pwd" | "mkdir" | "rmdir" | "touch" | "mv" | "cp" | "rm" => {
                write(b"  `");
                write(cmd.as_bytes());
                write(b"` needs somewhere to put things. there is nowhere.\n");
                write(b"  nothing is anywhere. describe it instead: find name~todo\n");
            }

            // "if you see me use ubuntu, i might say hi,
            //   but if you see me using arch, i'm a talkative guy"
            //        -- "Too Late I Already Deleted Windows", parody, via Seb
            "ubuntu" => write(b"  hi\n"),
            "arch" => write(b"  blah blah blah\n"),

            _ => {
                write(b"  no such command: ");
                write(cmd.as_bytes());
                write(b". try `help`.\n");
            }
        }
    }
}
