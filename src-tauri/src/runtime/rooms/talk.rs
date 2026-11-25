//! What a room says to a guy and what it takes back: the message that gives a guy its turn,
//! and reading its reply. The conventions are OpenClaw's own for group chats: lines carry
//! their sender, a block of what was said since the guy's last turn comes before the line
//! to answer, and the exact reply `NO_REPLY` means "nothing to add".

use serde_json::Value;

use super::floor::Why;

/// What every guy calls you in a room.
pub const YOU: &str = "User";
/// How many lines of the room a guy is shown at most when it gets a turn.
pub const HISTORY_LIMIT: usize = 50;

/// A member as the room's messages name it.
#[derive(Debug, Clone, PartialEq)]
pub struct Who {
    pub uid: String,
    pub name: String,
    /// What follows the `@` to address it: the name without spaces, unique in the room.
    pub handle: String,
}

/// A line of the room, for a guy to read.
pub struct Said<'a> {
    /// None: you.
    pub from: Option<&'a str>,
    pub text: &'a str,
}

/// Handles for `names`, in order: letters, digits, `_` and `-` of the name, made unique.
pub fn handles(names: &[String]) -> Vec<String> {
    let mut taken: Vec<String> = Vec::new();
    for name in names {
        let base: String = name.chars().filter(|c| c.is_alphanumeric() || *c == '_' || *c == '-').collect();
        let base = if base.is_empty() { "guy".to_string() } else { base };
        let mut handle = base.clone();
        let mut n = 2;
        while taken.iter().any(|t| t.eq_ignore_ascii_case(&handle)) || handle.eq_ignore_ascii_case("everyone") || handle.eq_ignore_ascii_case(YOU) {
            handle = format!("{base}{n}");
            n += 1;
        }
        taken.push(handle);
    }
    taken
}

/// The standing rules, sent with a guy's first turn in the room and again when its members change.
pub fn rules(room: &str, members: &[Who], lead: &str, me: &str) -> String {
    let list: Vec<String> = members
        .iter()
        .map(|m| {
            let mut entry = format!("{} (@{})", m.name, m.handle);
            if m.uid == lead {
                entry.push_str(", the lead");
            }
            if m.uid == me {
                entry.push_str(", which is you");
            }
            entry
        })
        .collect();
    format!(
        "[How this room works]\n\
         You are in \"{room}\", a group chat in littleguys with a person ({YOU}) and other AI agents. Each agent works in its own session with its own tools and files. Messages like this one are how the room reaches you.\n\
         Members: {YOU}, who everyone answers to; {}.\n\
         - The last message you write in a turn is posted to the room, for everyone. Write it for them: short, and only what matters. They can't see your files, tools or earlier messages.\n\
         - To ask another member something or hand them work, address them with their handle, like @{}. That gives them a turn, so only do it when you need something from them. Don't thank, greet or acknowledge.\n\
         - When you have nothing that would change the outcome, reply with exactly NO_REPLY and nothing else. Nothing is posted then. When you weren't addressed, that is the usual answer.",
        list.join("; "),
        members.iter().find(|m| m.uid != me).map(|m| m.handle.as_str()).unwrap_or("Name"),
    )
}

/// The message that gives a guy a turn: `[Room "name" #marker]`, the rules when it needs
/// them, what was said since its last turn, and what's asked of it now.
pub fn envelope(room: &str, marker: &str, rules: Option<&str>, said: &[Said], skipped: usize, why: Why, by: Option<&str>) -> String {
    let mut out = format!("[Room \"{room}\" #{marker}]\n");
    if let Some(rules) = rules {
        out.push_str(rules);
        out.push_str("\n\n");
    }
    if !said.is_empty() {
        out.push_str("[Since your last turn]\n");
        if skipped > 0 {
            out.push_str(&format!("({skipped} earlier messages left out)\n"));
        }
        for line in said {
            out.push_str(&format!("{}: {}\n", line.from.unwrap_or(YOU), line.text.trim()));
        }
        out.push('\n');
    }
    out.push_str("[Now] ");
    out.push_str(&match why {
        Why::Addressed => format!(
            "{} addressed you. Answer in the room. If nothing is needed from you, reply exactly NO_REPLY.",
            by.unwrap_or(YOU)
        ),
        Why::Lead => format!(
            "{YOU} wrote to the room without naming anyone, and you are its lead. Answer, or address whoever should."
        ),
        Why::Look => "The room has gone quiet and nobody addressed you. Speak only if you would change the outcome: a mistake, something nobody has said, a blocker. Otherwise reply exactly NO_REPLY.".to_string(),
    });
    out
}

/// What's sent into a turn already under way: only what's new.
pub fn nudge(room: &str, marker: &str, said: &[Said]) -> String {
    let mut out = format!("[Room \"{room}\" #{marker}]\n[While you were working]\n");
    for line in said {
        out.push_str(&format!("{}: {}\n", line.from.unwrap_or(YOU), line.text.trim()));
    }
    out.push_str("\n[Now] Take this into what you're doing. Your reply still goes to the room.");
    out
}

/// A reply that means "nothing to add": empty, or the silent token, give or take quotes,
/// emphasis and a full stop.
pub fn is_silent(text: &str) -> bool {
    let bare = text.trim().trim_matches(|c: char| c.is_whitespace() || "\"'`*_.[]()!".contains(c));
    bare.is_empty() || bare.eq_ignore_ascii_case("NO_REPLY") || bare.eq_ignore_ascii_case("NO REPLY")
}

/// What a reply shows as: OpenClaw's reply directives (`[[reply_to_current]]`) taken out.
pub fn clean(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("[[") {
        let Some(len) = rest[start..].find("]]") else { break };
        let inner = &rest[start + 2..start + len];
        let directive = !inner.is_empty() && inner.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "_:- ".contains(c));
        out.push_str(&rest[..start]);
        if !directive {
            out.push_str(&rest[start..start + len + 2]);
        }
        rest = &rest[start + len + 2..];
    }
    out.push_str(rest);
    out.trim().to_string()
}

/// The members `text` addresses: `@handle` or `@Name`, whatever the case, and `@everyone`.
/// `from` (the writer) is never among them.
pub fn mentions(text: &str, members: &[Who], from: Option<&str>) -> Vec<String> {
    let lower = text.to_lowercase();
    let mut found: Vec<String> = Vec::new();
    let mut add = |uid: &str| {
        if Some(uid) != from && !found.iter().any(|f| f == uid) {
            found.push(uid.to_string());
        }
    };
    for (at, _) in lower.match_indices('@') {
        // An email address or a path isn't a mention.
        if lower[..at].chars().next_back().is_some_and(|c| c.is_alphanumeric()) {
            continue;
        }
        let after = &lower[at + 1..];
        let ends = |len: usize| !after[len..].chars().next().is_some_and(|c| c.is_alphanumeric() || c == '_');
        if ["everyone", "all"].iter().any(|w| after.starts_with(w) && ends(w.len())) {
            members.iter().for_each(|m| add(&m.uid));
            continue;
        }
        // The longest name that fits, so "@Scout Jr" isn't read as "@Scout".
        let mut best: Option<(&Who, usize)> = None;
        for m in members {
            for name in [m.handle.to_lowercase(), m.name.to_lowercase()] {
                if !name.is_empty() && after.starts_with(&name) && ends(name.len()) && best.is_none_or(|(_, len)| name.len() > len) {
                    best = Some((m, name.len()));
                }
            }
        }
        if let Some((m, _)) = best {
            add(&m.uid);
        }
    }
    found
}

fn text_of(message: &Value) -> String {
    match message.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter(|p| p.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n\n"),
        _ => String::new(),
    }
}

/// A guy's reply to the turn `marker` names, from its thread's `chat.history`: the last
/// thing it said after that message. None while the thread doesn't show an answer yet.
pub fn reply_in(messages: &[Value], marker: &str) -> Option<String> {
    let tag = format!("#{marker}]");
    let role = |m: &Value| m.get("role").and_then(Value::as_str).unwrap_or_default().to_string();
    let asked = messages.iter().rposition(|m| role(m) == "user" && text_of(m).contains(&tag))?;
    messages[asked + 1..].iter().rev().filter(|m| role(m) == "assistant").map(text_of).find(|t| !t.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn who(uid: &str, name: &str) -> Who {
        Who { uid: uid.into(), name: name.into(), handle: handles(&[name.to_string()]).remove(0) }
    }

    #[test]
    fn handles_are_the_name_without_spaces_and_unique() {
        let names = ["Scout", "Claude Code", "scout", "🦉", "Everyone"].map(String::from);
        assert_eq!(handles(&names), ["Scout", "ClaudeCode", "scout2", "guy", "Everyone2"]);
    }

    #[test]
    fn finds_who_a_message_addresses() {
        let members = [who("a/scout", "Scout"), who("b/claude", "Claude Code"), who("a/scoutjr", "Scout Jr")];
        let uids = |text: &str, from: Option<&str>| mentions(text, &members, from);
        assert_eq!(uids("@Scout can you check?", None), ["a/scout"]);
        assert_eq!(uids("over to you, @claudecode.", None), ["b/claude"]);
        assert_eq!(uids("@Claude Code and @SCOUT JR: thoughts?", None), ["b/claude", "a/scoutjr"]);
        assert_eq!(uids("@everyone look", Some("a/scout")), ["b/claude", "a/scoutjr"]);
        // Not mentions: an address, a longer word, the writer itself, a stranger.
        assert!(uids("mail scout@scout.dev or @Scouting", None).is_empty());
        assert!(uids("as I (@Scout) said, ask @Nobody", Some("a/scout")).is_empty());
    }

    #[test]
    fn knows_a_silent_reply() {
        for silent in ["", "  \n", "NO_REPLY", "no_reply.", "`NO_REPLY`", "**NO_REPLY**", "\"NO_REPLY\"\n"] {
            assert!(is_silent(silent), "{silent:?}");
        }
        for said in ["NO_REPLY needed, but here's a fix", "No.", "I have no reply to that"] {
            assert!(!is_silent(said), "{said:?}");
        }
    }

    #[test]
    fn takes_reply_directives_out_but_leaves_wiki_links() {
        assert_eq!(clean("[[reply_to_current]] Fixed and pushed."), "Fixed and pushed.");
        assert_eq!(clean("See [[Release Notes]] for more"), "See [[Release Notes]] for more");
        assert_eq!(clean("a [[ b"), "a [[ b");
    }

    #[test]
    fn reads_the_reply_to_its_own_turn() {
        let ask = envelope("Launch", "r7", None, &[Said { from: None, text: "is it deployed?" }], 0, Why::Addressed, None);
        let history = vec![
            json!({ "role": "user", "content": "[Room \"Launch\" #r6]\nolder" }),
            json!({ "role": "assistant", "content": "older answer" }),
            json!({ "role": "user", "content": [{ "type": "text", "text": ask }] }),
            json!({ "role": "assistant", "content": [{ "type": "text", "text": "Let me look." }, { "type": "toolCall", "id": "t", "name": "bash" }] }),
            json!({ "role": "toolResult", "content": "ok" }),
            json!({ "role": "assistant", "content": [{ "type": "text", "text": "Not yet, it needs your approval." }] }),
        ];
        assert_eq!(reply_in(&history, "r7").as_deref(), Some("Not yet, it needs your approval."));
        // Its turn isn't in the thread yet, or it hasn't answered.
        assert_eq!(reply_in(&history, "r8"), None);
        assert_eq!(reply_in(&history[..3], "r7"), None);
    }

    #[test]
    fn the_turn_message_says_who_spoke_and_what_is_asked() {
        let said = [Said { from: None, text: "signups fail on Safari" }, Said { from: Some("Scout"), text: "It's the date parser. @Patch can you fix it?" }];
        let text = envelope("Launch", "r2", Some("[How this room works]\n…"), &said, 3, Why::Addressed, Some("Scout"));
        assert!(text.starts_with("[Room \"Launch\" #r2]\n[How this room works]"));
        assert!(text.contains("(3 earlier messages left out)\nUser: signups fail on Safari\nScout: It's the date parser."));
        assert!(text.ends_with("[Now] Scout addressed you. Answer in the room. If nothing is needed from you, reply exactly NO_REPLY."));
        assert!(envelope("Launch", "r3", None, &[], 0, Why::Look, None).contains("gone quiet"));
    }
}
