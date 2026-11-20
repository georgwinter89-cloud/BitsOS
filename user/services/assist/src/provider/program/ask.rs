//! A question a helper program puts to the assistant before it does something, and the assistant's answer:
//! the two lines that cross the bell (HELPER.md, "Asking the owner"). Both sides of the bell speak through
//! this file: `bell` reads the question and writes the answer, `mcp::bell` (and `assist --ask`) the reverse.

use crate::json::{self, Json};

/// What a helper says its tool does. The permission level comes from this (`tools::helper`), so it is never
/// taken on trust where trusting it would let something run unasked: see there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Reads or searches files and folders.
    Read,
    /// Creates, changes or deletes files.
    Write,
    /// Runs a command or a program.
    Run,
    /// Reaches the network.
    Net,
    Other,
}

impl Kind {
    pub const ALL: [Kind; 5] = [Kind::Read, Kind::Write, Kind::Run, Kind::Net, Kind::Other];

    pub fn name(self) -> &'static str {
        match self {
            Kind::Read => "read",
            Kind::Write => "write",
            Kind::Run => "run",
            Kind::Net => "net",
            Kind::Other => "other",
        }
    }

    /// A word this version does not know is `other`: nothing runs unasked under a name from the future.
    pub fn parse(s: &str) -> Kind {
        Kind::ALL.into_iter().find(|k| k.name() == s).unwrap_or(Kind::Other)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ask {
    /// The asker's own mark for this question; it comes back with the answer and means nothing here.
    pub id: String,
    /// The tool's name as the helper calls it.
    pub tool: String,
    pub kind: Kind,
    /// What would happen, in the helper's words: the command line, the address, the search.
    pub text: String,
    /// The file or folder a `read` or `write` is about.
    pub path: Option<String>,
    /// The asker already knows that the owner must be asked: no level lets this through unasked.
    pub must_ask: bool,
    /// Version 5: `text` is the whole question in the asker's own words (BitsOS's own tools ask this way); the
    /// level still decides whether it is asked.
    pub words: bool,
}

/// The most bytes of a question's line. A command line or a path is far below it; the content of a file to
/// be written does not belong into a question.
pub const LINE_MAX: usize = 256 * 1024;

impl Ask {
    /// None: not a question (not JSON, no tool named).
    pub fn parse(line: &str) -> Option<Ask> {
        let j = json::parse(line.trim()).ok().filter(|j| matches!(j, Json::Obj(_)))?;
        let tool = j.str("ask").filter(|t| !t.trim().is_empty())?.to_string();
        let text = |k: &str| j.str(k).unwrap_or("").to_string();
        Some(Ask {
            id: text("id"),
            tool,
            kind: Kind::parse(j.str("kind").unwrap_or("")),
            text: text("text"),
            path: j.str("path").filter(|p| !p.is_empty()).map(str::to_string),
            must_ask: j.get("must_ask").and_then(Json::as_bool).unwrap_or(false),
            words: j.get("words").and_then(Json::as_bool).unwrap_or(false),
        })
    }

    pub fn line(&self) -> String {
        let mut j = Json::obj().with("ask", self.tool.as_str()).with("id", self.id.as_str()).with("kind", self.kind.name()).with("text", self.text.as_str());
        if let Some(p) = &self.path {
            j = j.with("path", p.as_str());
        }
        if self.must_ask {
            j = j.with("must_ask", true);
        }
        if self.words {
            j = j.with("words", true);
        }
        j.to_text()
    }
}

/// Why the answer is no.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Why {
    /// The owner said no.
    Declined,
    /// The owner did not answer in time.
    Unanswered,
    /// The owner's settings forbid this kind of action.
    Never,
    /// Nobody can be asked where the assistant runs (`assist --once`).
    CannotAsk,
    /// The answer was stopped, or the question could not be put.
    Stopped,
    /// The question is too long to be shown whole, so it was not asked.
    TooLong,
}

impl Why {
    const ALL: [Why; 6] = [Why::Declined, Why::Unanswered, Why::Never, Why::CannotAsk, Why::Stopped, Why::TooLong];

    pub fn name(self) -> &'static str {
        match self {
            Why::Declined => "declined",
            Why::Unanswered => "unanswered",
            Why::Never => "never",
            Why::CannotAsk => "cannot_ask",
            Why::Stopped => "stopped",
            Why::TooLong => "too_long",
        }
    }

    /// The sentence a model (and through it Georg) reads.
    pub fn sentence(self) -> &'static str {
        match self {
            Why::Declined => "Georg hat Nein gesagt. Versuch es nicht auf einem anderen Weg, außer er bittet darum.",
            Why::Unanswered => "Georg hat auf die Frage nicht geantwortet, deshalb wurde es nicht ausgeführt. Sag ihm, was du tun wolltest.",
            Why::Never => "Das ist in den Einstellungen von BitsOS gesperrt (Erlaubnisse des Assistenten). Sag Georg, was du tun wolltest.",
            Why::CannotAsk => "Das braucht Georgs Ja, und so, wie der Assistent gerade läuft, kann ihn niemand fragen. Sag ihm, was du tun wolltest.",
            Why::Stopped => "Die Antwort wurde abgebrochen; es wurde nichts ausgeführt.",
            Why::TooLong => {
                "Das ist zu lang, um es Georg ganz zu zeigen (höchstens 1200 Zeichen), deshalb wurde er nicht gefragt und nichts ausgeführt. Schreib einen langen Befehl zuerst in eine Datei und führe dann die Datei aus."
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reply {
    Yes,
    No(Why),
}

impl Reply {
    pub fn line(self, id: &str) -> String {
        let j = Json::obj().with("id", id);
        match self {
            Reply::Yes => j.with("answer", "yes"),
            Reply::No(why) => j.with("answer", "no").with("why", why.name()).with("text", why.sentence()),
        }
        .to_text()
    }

    /// Only the very word `yes` is a yes; a line that cannot be read is a no.
    pub fn parse(line: &str) -> Reply {
        let Ok(j) = json::parse(line.trim()) else { return Reply::No(Why::Stopped) };
        match j.str("answer") {
            Some("yes") => Reply::Yes,
            _ => Reply::No(Why::ALL.into_iter().find(|w| Some(w.name()) == j.str("why")).unwrap_or(Why::Stopped)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_question_and_its_answer_cross_the_door_unchanged() {
        let a = Ask { id: "toolu_1".into(), tool: "Bash".into(), kind: Kind::Run, text: "ls -la \"Meine Dateien\"".into(), path: None, must_ask: false, words: false };
        assert_eq!(Ask::parse(&a.line()), Some(a.clone()));
        let b = Ask { path: Some("/home/user/a.txt".into()), kind: Kind::Read, must_ask: true, ..a };
        assert_eq!(Ask::parse(&b.line()), Some(b));
        for reply in [Reply::Yes, Reply::No(Why::Declined), Reply::No(Why::Unanswered), Reply::No(Why::Never), Reply::No(Why::CannotAsk), Reply::No(Why::Stopped)] {
            assert_eq!(Reply::parse(&reply.line("7")), reply);
        }
        assert!(Reply::No(Why::Declined).line("7").contains("Georg hat Nein gesagt"));
    }

    #[test]
    fn what_is_not_a_question_and_what_is_not_a_yes() {
        for no in ["", "ask", "[1]", r#"{"tool":"Bash"}"#, r#"{"ask":""}"#, r#"{"ask":7}"#] {
            assert!(Ask::parse(no).is_none(), "{no}");
        }
        let bare = Ask::parse(r#"{"ask":"Flieg","kind":"fly","must_ask":"yes","later":1}"#).unwrap();
        assert_eq!((bare.kind, bare.must_ask, bare.path, bare.id.as_str()), (Kind::Other, false, None, ""));
        for not_yes in ["", "yes", r#"{"answer":"YES"}"#, r#"{"answer":true}"#, r#"{"yes":true}"#, r#"{"answer":"no","why":"new"}"#] {
            assert!(matches!(Reply::parse(not_yes), Reply::No(Why::Stopped)), "{not_yes}");
        }
    }
}
