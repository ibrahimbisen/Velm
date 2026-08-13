//! The shim an agent invokes to talk back to Velm.
//!
//! An agent runs in its own process and cannot call into the application that placed it on
//! the board. Velm launches it with `VELM_IPC` and `VELM_AGENT_ID` in its environment and
//! this binary on its `PATH`; every call connects to the loopback server, sends one request,
//! prints the answer and exits. See `docs/07-agent-canvas.md` §6 and [`vellum_agent::ipc`].
//!
//! # Three things this file is built around
//!
//! - **`--help` is the only documentation the reader ever sees.** The reader is a language
//!   model that has been told a command exists and will decide from this text alone whether
//!   it applies and how to call it. So the help is written as instructions rather than as a
//!   summary: every verb says what it *does to the board*, every argument says what a real
//!   value looks like, and the exit codes are listed because an agent that shells out reads
//!   the code before it reads the message.
//! - **It must start fast.** It runs once per call, possibly hundreds of times in a session.
//!   Nothing here parses a config file, scans a directory or resolves a host: it reads two
//!   environment variables, opens one socket and exits.
//! - **It never prints the token.** The runtime file is read for a port and a credential; the
//!   credential goes into the request and nowhere else. Not into `--json` output, not into an
//!   error message, not into a diagnostic naming the file it failed to read.

use std::io::Read as _;
use std::process::ExitCode;

use vellum_agent::ipc::{self, Answer, ErrorCode, Request, Response, RuntimeFile, SpawnRequest};
use vellum_agent::transcript::Choice;
use vellum_agent::{AgentError, RoleKind};

/// What the shell gets back.
///
/// An agent shelling out reads the exit code before it reads the message, so these are
/// distinct rather than "0 or 1": *Velm refused you* and *Velm is not running* call for
/// completely different next moves, and an agent that cannot tell them apart will retry the
/// one that will never work.
mod exit {
    /// The request was made and Velm agreed.
    pub const OK: u8 = 0;
    /// The command line was wrong. Nothing was sent.
    pub const USAGE: u8 = 1;
    /// The environment is not an agent's — `VELM_IPC` or `VELM_AGENT_ID` is missing, or the
    /// runtime file it points at cannot be read. Retrying will not help.
    pub const ENVIRONMENT: u8 = 2;
    /// Velm could not be reached: it is not running, or it is wedged. Retrying might help.
    pub const UNREACHABLE: u8 = 3;
    /// Velm understood and refused — no connector, no permission, a cap, a mute. Read the
    /// message and do something else.
    pub const REFUSED: u8 = 4;
    /// It was attempted and it failed.
    pub const FAILED: u8 = 5;
}

const HELP: &str = "\
velm-agent-cli — act on the Velm board you are running on.

You are an agent on an infinite canvas. This command is how you reach the rest of it:
message the agents you are wired to, read and write the shared notes, put pictures and
choices in front of the person watching, and (if you are an orchestrator) ask for help.

USAGE
    velm-agent-cli [--json] <command> [arguments]

COMMANDS
    send <agent> <text>...
        Send a message to another agent. <agent> is the label on its node, as you see it
        on the board (\"Reviewer\"), or its node id. The message is delivered only if a
        connector joins you to it and points your way — that line is how the person
        running this board granted the two of you permission to talk. If there is no line,
        this is refused; ask the user to draw one rather than trying another route.
        Everything after <agent> is the message, so quoting is optional.
            velm-agent-cli send Reviewer \"the parser is done, please look at src/lex.rs\"

    note read <path>
        Print a note's markdown to stdout. Notes are real .md files that you, the other
        agents and the user all read and write; they are the board's memory between turns.

    note write <path> <text>|-  [--append]
        Replace a note, or add to the end of it with --append. Pass - to take the text
        from stdin, which is how you write anything with newlines in it. Prefer --append
        for a running log: a plain write replaces everything the others wrote.
            echo \"## findings\" | velm-agent-cli note write findings.md - --append

    note list
        List the notes you can see: the board's shared notes plus your own private ones.

    spawn <label> [--role worker|orchestrator] [--prompt <text>] [--at <x>,<y>]
        Ask Velm to create another agent and put it on the board. Orchestrators only.
        There is a hard cap on how many you may have at once and a region of the board you
        must stay inside; exceeding either is refused, and the refusal tells you which.
        Give it a label a person can read — five agents called \"agent\" is an unusable
        board.
            velm-agent-cli spawn \"Test writer\" --prompt \"write tests for src/lex.rs\"

    image <file> [--caption <text>]
        Put a picture in your transcript, where the user will see it inline. <file> is a
        path to a PNG or JPEG you have produced. Use this instead of describing a chart,
        a diagram or a screenshot in prose.

    options <prompt> --choice <id>=<title> [--choice ...]
        Ask the user to pick one of several answers, drawn as a row of cards. Use this
        when you have built two or three real alternatives rather than asking an open
        question. The choice they click comes back as your next turn's input.
        For choices with a description or a picture, pass the full form instead:
            --choices '[{\"id\":\"a\",\"title\":\"Warm\",\"body\":\"amber, serif\"}]'

    configure <node> [--set <json>|-]
        Read another node's configuration, or replace it. Meta agents only — every other
        role is refused at Velm's boundary, not by convention. With no --set this prints
        the configuration as JSON; --set takes that same JSON back, so read it, change
        what you mean to change, and write the whole thing.

OPTIONS
    --json      Print Velm's raw JSON response instead of a human-readable line. Use this
                when you are going to parse the answer.
    --help      This text.

ENVIRONMENT
    VELM_IPC        Path to the runtime file describing the running Velm. Set for you.
    VELM_AGENT_ID   Your own node id on the board. Set for you.
    Both are set by Velm when it starts you. If they are missing you are not running
    inside Velm and none of this will work.

EXIT CODES
    0  done
    1  the command line was wrong; nothing was sent
    2  not running inside Velm (missing environment, or an unreadable runtime file)
    3  Velm could not be reached — it may have quit. Retrying may help.
    4  Velm refused: no connector, no permission, a cap, or a muted agent. Read the
       message on stderr and do something else; retrying will be refused again.
    5  it was attempted and it failed
";

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    match run(&arguments) {
        Ok(output) => {
            if !output.is_empty() {
                println!("{output}");
            }
            ExitCode::from(exit::OK)
        }
        Err(failure) => {
            eprintln!("velm-agent-cli: {}", failure.message);
            ExitCode::from(failure.code)
        }
    }
}

/// Anything that stops the shim, with the code the shell should see.
#[derive(Debug)]
struct Failure {
    code: u8,
    message: String,
}

impl Failure {
    fn new(code: u8, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }

    fn usage(message: impl Into<String>) -> Self {
        Self::new(exit::USAGE, message)
    }
}

fn run(arguments: &[String]) -> Result<String, Failure> {
    let parsed = Parsed::of(arguments)?;
    if parsed.wants_help() {
        return Ok(HELP.trim_end().to_owned());
    }

    let request = build_request(&parsed)?;
    let (runtime, agent) = environment()?;

    let response = runtime.call(&agent, request).map_err(|error| match error {
        // A socket that will not open or will not answer means the application is gone or
        // wedged, which is a different situation from a refusal and gets its own code.
        AgentError::Io(_) | AgentError::Transport { .. } => {
            Failure::new(exit::UNREACHABLE, format!("could not reach Velm: {error}"))
        }
        other => Failure::new(exit::FAILED, other.to_string()),
    })?;

    if parsed.json {
        let encoded = serde_json::to_string(&response)
            .map_err(|error| Failure::new(exit::FAILED, error.to_string()))?;
        return if response.ok {
            Ok(encoded)
        } else {
            // Still printed, on stdout, because a caller that asked for JSON is parsing it
            // and a refusal is an answer. The code and the message go to the shell as well.
            println!("{encoded}");
            Err(refusal(&response))
        };
    }

    if !response.ok {
        return Err(refusal(&response));
    }
    Ok(describe(response.answer))
}

/// A refusal, carrying the code that says what kind it was.
fn refusal(response: &Response) -> Failure {
    let code = match response.code {
        Some(ErrorCode::Refused | ErrorCode::NotConnected | ErrorCode::BadToken) => exit::REFUSED,
        Some(ErrorCode::UnknownAgent) => exit::ENVIRONMENT,
        Some(ErrorCode::Malformed) => exit::USAGE,
        _ => exit::FAILED,
    };
    Failure::new(code, response.message().to_owned())
}

/// Turn an answer into the one line a human — or an agent reading stdout — wants.
fn describe(answer: Option<Answer>) -> String {
    match answer {
        // A note is printed verbatim: it is the thing that was asked for, and wrapping it in
        // a status line would mean every reader had to strip one off again.
        Some(Answer::Note { text }) => text.trim_end().to_owned(),
        Some(Answer::Notes { notes }) => {
            if notes.is_empty() {
                return "no notes on this board yet".to_owned();
            }
            notes
                .iter()
                .map(|note| {
                    let scope = if note.scope.is_private() { "private" } else { "shared" };
                    if note.title.is_empty() {
                        format!("{}\t{scope}", note.path)
                    } else {
                        format!("{}\t{scope}\t{}", note.path, note.title)
                    }
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
        Some(Answer::Spawned { agent }) => format!("spawned {agent}"),
        Some(Answer::Config { model }) => {
            serde_json::to_string_pretty(&model).unwrap_or_else(|_| "{}".to_owned())
        }
        Some(Answer::Done) | None => "done".to_owned(),
    }
}

/// The two variables Velm sets, and the file one of them points at.
///
/// The error text names the variable rather than the file's contents, and says what the
/// situation *is* — an agent that reads *"not running inside Velm"* stops, where one that
/// reads *"no such file"* tries to create it.
fn environment() -> Result<(RuntimeFile, String), Failure> {
    let path = std::env::var("VELM_IPC").map_err(|_| {
        Failure::new(
            exit::ENVIRONMENT,
            "VELM_IPC is not set — this only works inside an agent that Velm started",
        )
    })?;
    let agent = std::env::var("VELM_AGENT_ID").map_err(|_| {
        Failure::new(
            exit::ENVIRONMENT,
            "VELM_AGENT_ID is not set — this only works inside an agent that Velm started",
        )
    })?;
    let runtime = RuntimeFile::read(std::path::Path::new(&path)).map_err(|error| {
        Failure::new(
            exit::ENVIRONMENT,
            format!("could not read the Velm runtime file at {path}: {error}"),
        )
    })?;
    Ok((runtime, agent))
}

// ---------------------------------------------------------------------------------------
// The command line
// ---------------------------------------------------------------------------------------

/// Options that take a value. Anything else beginning with `-` is a mistake rather than a
/// positional argument — an agent that mistypes `--promt` must be told, not silently obeyed
/// with the prompt dropped.
const VALUED: [&str; 7] = ["--role", "--prompt", "--caption", "--choice", "--choices", "--set", "--at"];

/// Switches that take none.
const SWITCHES: [&str; 4] = ["--json", "--append", "--help", "-h"];

#[derive(Debug, Default, PartialEq, Eq)]
struct Parsed {
    words: Vec<String>,
    /// `(name, value)` in the order given, so `--choice` can be repeated.
    options: Vec<(String, String)>,
    json: bool,
    append: bool,
    help: bool,
}

impl Parsed {
    fn of(arguments: &[String]) -> Result<Self, Failure> {
        let mut parsed = Self::default();
        let mut index = 0;
        let mut only_words = false;

        while let Some(argument) = arguments.get(index) {
            index += 1;
            if only_words {
                parsed.words.push(argument.clone());
                continue;
            }
            // Everything after a bare `--` is text, which is how a message that begins with
            // a dash gets sent at all.
            if argument == "--" {
                only_words = true;
                continue;
            }
            if SWITCHES.contains(&argument.as_str()) {
                match argument.as_str() {
                    "--json" => parsed.json = true,
                    "--append" => parsed.append = true,
                    _ => parsed.help = true,
                }
                continue;
            }
            if VALUED.contains(&argument.as_str()) {
                let value = arguments.get(index).ok_or_else(|| {
                    Failure::usage(format!("{argument} needs a value after it"))
                })?;
                index += 1;
                parsed.options.push((argument.clone(), value.clone()));
                continue;
            }
            if argument.starts_with("--") {
                return Err(Failure::usage(format!(
                    "there is no option called {argument} — run with --help for the list"
                )));
            }
            parsed.words.push(argument.clone());
        }
        Ok(parsed)
    }

    fn wants_help(&self) -> bool {
        self.help || self.words.is_empty()
    }

    fn word(&self, index: usize) -> Option<&str> {
        self.words.get(index).map(String::as_str)
    }

    fn value(&self, name: &str) -> Option<&str> {
        self.options
            .iter()
            .find(|(option, _)| option == name)
            .map(|(_, value)| value.as_str())
    }

    fn values(&self, name: &str) -> Vec<&str> {
        self.options
            .iter()
            .filter(|(option, _)| option == name)
            .map(|(_, value)| value.as_str())
            .collect()
    }

    /// Everything from `index` on, joined — so a message need not be quoted.
    fn rest(&self, index: usize) -> String {
        self.words.get(index..).map(|rest| rest.join(" ")).unwrap_or_default()
    }
}

/// Build the request, or say exactly what is missing.
///
/// Pure, and separated from everything that touches a socket or the environment, so the whole
/// command line is an ordinary unit test.
fn build_request(parsed: &Parsed) -> Result<Request, Failure> {
    let command = parsed.word(0).unwrap_or_default();
    match command {
        "send" => {
            let to = parsed
                .word(1)
                .ok_or_else(|| Failure::usage("send needs an agent to send to"))?;
            let text = parsed.rest(2);
            if text.trim().is_empty() {
                return Err(Failure::usage("send needs a message"));
            }
            Ok(Request::Send { to: to.to_owned(), text })
        }

        "note" => match parsed.word(1).unwrap_or_default() {
            "read" => {
                let path = parsed
                    .word(2)
                    .ok_or_else(|| Failure::usage("note read needs a path"))?;
                Ok(Request::NoteRead { path: path.to_owned() })
            }
            "write" => {
                let path = parsed
                    .word(2)
                    .ok_or_else(|| Failure::usage("note write needs a path"))?
                    .to_owned();
                let text = match parsed.word(3) {
                    // `-` is how a note with newlines in it gets written: an argument list is
                    // a poor place for a paragraph, and quoting one through two shells is
                    // where an agent's output loses its formatting.
                    Some("-") => read_stdin()?,
                    Some(_) => parsed.rest(3),
                    None => {
                        return Err(Failure::usage(
                            "note write needs the text, or - to read it from stdin",
                        ));
                    }
                };
                Ok(Request::NoteWrite { path, text, append: parsed.append })
            }
            "list" => Ok(Request::NoteList),
            other if other.is_empty() => {
                Err(Failure::usage("note needs read, write or list after it"))
            }
            other => Err(Failure::usage(format!(
                "there is no note command called {other} — it is read, write or list"
            ))),
        },

        "spawn" => {
            let label = parsed.rest(1);
            if label.trim().is_empty() {
                return Err(Failure::usage("spawn needs a label for the new agent"));
            }
            let role = match parsed.value("--role") {
                None | Some("worker") => RoleKind::Worker,
                Some("orchestrator") => RoleKind::Orchestrator,
                // Deliberately not offered: an agent that could spawn the board's control
                // plane could grant itself the one capability it does not have.
                Some(other) => {
                    return Err(Failure::usage(format!(
                        "there is no role called {other} — it is worker or orchestrator"
                    )));
                }
            };
            Ok(Request::Spawn(SpawnRequest {
                label,
                role,
                prompt: parsed.value("--prompt").map(str::to_owned),
                at: parsed.value("--at").map(parse_point).transpose()?,
            }))
        }

        "image" => {
            let path = parsed
                .word(1)
                .ok_or_else(|| Failure::usage("image needs the path to a picture"))?;
            let bytes = std::fs::read(path).map_err(|error| {
                Failure::new(exit::USAGE, format!("could not read {path}: {error}"))
            })?;
            if bytes.is_empty() {
                return Err(Failure::usage(format!("{path} is empty")));
            }
            Ok(Request::Image {
                data: ipc::encode_base64(&bytes),
                caption: parsed.value("--caption").map(str::to_owned),
            })
        }

        "options" => {
            let prompt = parsed
                .word(1)
                .ok_or_else(|| Failure::usage("options needs a prompt"))?
                .to_owned();
            let choices = build_choices(parsed)?;
            if choices.is_empty() {
                return Err(Failure::usage(
                    "options needs at least one --choice id=title, or --choices with JSON",
                ));
            }
            Ok(Request::Options { prompt, choices })
        }

        "configure" => {
            let node = parsed
                .word(1)
                .ok_or_else(|| Failure::usage("configure needs the node to configure"))?
                .to_owned();
            let model = match parsed.value("--set") {
                None => None,
                Some("-") => Some(parse_model(&read_stdin()?)?),
                Some(json) => Some(parse_model(json)?),
            };
            Ok(Request::Configure { node, model: model.map(Box::new) })
        }

        other => Err(Failure::usage(format!(
            "there is no command called {other} — run with --help for the list"
        ))),
    }
}

/// `--choice id=title`, repeatable, or one `--choices` carrying the full JSON.
///
/// Both, because the two cases are genuinely different: three plain alternatives is the
/// common one and should not require writing JSON in a shell, and a choice with a body or a
/// picture has more fields than a flag can carry legibly.
fn build_choices(parsed: &Parsed) -> Result<Vec<Choice>, Failure> {
    let simple = parsed.values("--choice");
    let full = parsed.value("--choices");

    if !simple.is_empty() && full.is_some() {
        return Err(Failure::usage(
            "use either --choice or --choices, not both — they would fight over the order",
        ));
    }

    if let Some(json) = full {
        return serde_json::from_str::<Vec<Choice>>(json).map_err(|error| {
            Failure::usage(format!(
                "--choices needs a JSON array like \
                 [{{\"id\":\"a\",\"title\":\"Warm\"}}]: {error}"
            ))
        });
    }

    simple
        .iter()
        .map(|pair| {
            // `split_once`, not an index: an `=` inside the title is legitimate and a byte
            // index into a string with any multi-byte character in it is how this crate has
            // already aborted a release build twice.
            let (id, title) = pair.split_once('=').ok_or_else(|| {
                Failure::usage(format!("--choice {pair} should look like id=title"))
            })?;
            if id.is_empty() || title.is_empty() {
                return Err(Failure::usage(format!(
                    "--choice {pair} needs both an id and a title"
                )));
            }
            Ok(Choice::new(id, title))
        })
        .collect()
}

fn parse_point(value: &str) -> Result<(f64, f64), Failure> {
    let (x, y) = value
        .split_once(',')
        .ok_or_else(|| Failure::usage(format!("--at {value} should look like 120,-40")))?;
    let x = x.trim().parse::<f64>();
    let y = y.trim().parse::<f64>();
    match (x, y) {
        (Ok(x), Ok(y)) => Ok((x, y)),
        _ => Err(Failure::usage(format!("--at {value} should be two numbers, like 120,-40"))),
    }
}

fn parse_model(json: &str) -> Result<vellum_agent::AgentModel, Failure> {
    serde_json::from_str(json).map_err(|error| {
        Failure::usage(format!(
            "--set needs the JSON that `configure <node>` prints: {error}"
        ))
    })
}

fn read_stdin() -> Result<String, Failure> {
    let mut buffer = String::new();
    std::io::stdin()
        .read_to_string(&mut buffer)
        .map_err(|error| Failure::new(exit::USAGE, format!("could not read stdin: {error}")))?;
    Ok(buffer)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(line: &[&str]) -> Result<Parsed, Failure> {
        let arguments: Vec<String> = line.iter().map(|word| (*word).to_owned()).collect();
        Parsed::of(&arguments)
    }

    fn request(line: &[&str]) -> Result<Request, Failure> {
        build_request(&parse(line)?)
    }

    /// The message is everything after the target, joined — so an agent that does not quote
    /// its sentence still sends the whole sentence rather than its first word.
    #[test]
    fn send_takes_the_target_and_everything_after_it_as_the_message() {
        let built = request(&["send", "Reviewer", "the", "parser", "is", "done"]).unwrap();
        assert_eq!(
            built,
            Request::Send {
                to: "Reviewer".into(),
                text: "the parser is done".into()
            }
        );

        assert_eq!(request(&["send", "Reviewer"]).unwrap_err().code, exit::USAGE);
        assert_eq!(request(&["send"]).unwrap_err().code, exit::USAGE);
    }

    /// `--append` is the difference between a running log and destroying what the other
    /// agents wrote, so it has to actually reach the request.
    #[test]
    fn note_write_carries_append_through_to_the_request() {
        let plain = request(&["note", "write", "plan.md", "hello there"]).unwrap();
        assert_eq!(
            plain,
            Request::NoteWrite {
                path: "plan.md".into(),
                text: "hello there".into(),
                append: false
            }
        );

        let appending = request(&["note", "write", "plan.md", "--append", "more"]).unwrap();
        match appending {
            Request::NoteWrite { append, text, .. } => {
                assert!(append, "--append did not reach the request");
                assert_eq!(text, "more", "a switch was swallowed into the note's text");
            }
            other => panic!("{other:?}"),
        }

        assert_eq!(request(&["note", "list"]).unwrap(), Request::NoteList);
        assert_eq!(request(&["note"]).unwrap_err().code, exit::USAGE);
        assert_eq!(request(&["note", "delete", "x"]).unwrap_err().code, exit::USAGE);
    }

    /// A mistyped option is a usage error, **not** a positional argument. Treating `--promt`
    /// as part of the label would spawn an agent named after the typo and silently drop the
    /// prompt — an obedient failure, which is the worst kind for something a model drives.
    #[test]
    fn a_mistyped_option_is_refused_rather_than_obeyed() {
        let failure = request(&["spawn", "Helper", "--promt", "go"]).unwrap_err();
        assert_eq!(failure.code, exit::USAGE);
        assert!(failure.message.contains("--promt"), "{}", failure.message);

        // And an option with nothing after it does not silently take the next command.
        let failure = parse(&["spawn", "Helper", "--prompt"]).unwrap_err();
        assert!(failure.message.contains("--prompt"), "{}", failure.message);
    }

    #[test]
    fn spawn_takes_a_role_a_prompt_and_a_place() {
        let built = request(&[
            "spawn", "Test writer", "--role", "worker", "--prompt", "write tests", "--at",
            "120,-40",
        ])
        .unwrap();
        assert_eq!(
            built,
            Request::Spawn(SpawnRequest {
                label: "Test writer".into(),
                role: RoleKind::Worker,
                prompt: Some("write tests".into()),
                at: Some((120.0, -40.0)),
            })
        );

        // A worker cannot promote itself by spawning the board's control plane.
        assert_eq!(request(&["spawn", "X", "--role", "meta"]).unwrap_err().code, exit::USAGE);
        assert_eq!(request(&["spawn", "X", "--at", "over there"]).unwrap_err().code, exit::USAGE);
    }

    /// An `=` inside a title is legitimate, and `split_once` is what makes it safe. A byte
    /// index is how this crate has already aborted a release build twice (feedback 30).
    #[test]
    fn a_choice_splits_at_the_first_equals_and_survives_multibyte_titles() {
        let built = request(&[
            "options",
            "Which?",
            "--choice",
            "a=Warm — 温かい = yes",
            "--choice",
            "b=Cool",
        ])
        .unwrap();
        match built {
            Request::Options { prompt, choices } => {
                assert_eq!(prompt, "Which?");
                assert_eq!(choices.len(), 2);
                assert_eq!(choices[0].id, "a");
                assert_eq!(choices[0].title, "Warm — 温かい = yes");
                assert_eq!(choices[1].title, "Cool");
            }
            other => panic!("{other:?}"),
        }

        assert_eq!(request(&["options", "Which?"]).unwrap_err().code, exit::USAGE);
        assert_eq!(
            request(&["options", "Which?", "--choice", "nothing"]).unwrap_err().code,
            exit::USAGE
        );
    }

    /// The full form, for a choice with a description. Both forms at once is refused rather
    /// than merged: the order of the cards is the answer's meaning, and two sources for it
    /// is a coin toss.
    #[test]
    fn the_json_form_of_a_choice_carries_what_a_flag_cannot() {
        let built = request(&[
            "options",
            "Which?",
            "--choices",
            r#"[{"id":"a","title":"Warm","body":"amber, serif"}]"#,
        ])
        .unwrap();
        match built {
            Request::Options { choices, .. } => {
                assert_eq!(choices[0].body.as_deref(), Some("amber, serif"));
            }
            other => panic!("{other:?}"),
        }

        let both = request(&[
            "options", "Which?", "--choice", "a=Warm", "--choices", "[]",
        ])
        .unwrap_err();
        assert_eq!(both.code, exit::USAGE);

        assert_eq!(
            request(&["options", "Which?", "--choices", "not json"]).unwrap_err().code,
            exit::USAGE
        );
    }

    /// `configure` with nothing to set is a read, and with `--set` a whole-model write. The
    /// JSON it takes is the JSON it prints, which is the only contract a model can follow.
    #[test]
    fn configure_reads_without_set_and_writes_with_it() {
        assert_eq!(
            request(&["configure", "42@7"]).unwrap(),
            Request::Configure { node: "42@7".into(), model: None }
        );

        let written = request(&["configure", "42@7", "--set", r#"{"role_kind":"orchestrator"}"#])
            .unwrap();
        match written {
            Request::Configure { model: Some(model), .. } => {
                assert_eq!(model.role_kind, RoleKind::Orchestrator);
            }
            other => panic!("{other:?}"),
        }

        assert_eq!(
            request(&["configure", "42@7", "--set", "{{{"]).unwrap_err().code,
            exit::USAGE
        );
    }

    /// Everything after a bare `--` is text. Without it a message that starts with a dash is
    /// unsendable, and an agent quoting a diff hits that on its first try.
    #[test]
    fn a_double_dash_lets_a_message_start_with_one() {
        let built = request(&["send", "Reviewer", "--", "--- a/src/lex.rs"]).unwrap();
        match built {
            Request::Send { text, .. } => assert_eq!(text, "--- a/src/lex.rs"),
            other => panic!("{other:?}"),
        }
    }

    /// `--help` and a bare invocation both print the help rather than an error: a model that
    /// has been told this command exists will run it with no arguments to find out what it
    /// does, and an error at that moment teaches it the command is broken.
    #[test]
    fn help_is_what_an_empty_command_line_gets() {
        assert!(parse(&[]).unwrap().wants_help());
        assert!(parse(&["--help"]).unwrap().wants_help());
        assert!(parse(&["-h"]).unwrap().wants_help());
        assert!(!parse(&["note", "list"]).unwrap().wants_help());
    }

    /// The help is the only documentation the agent inside the process will ever read, so a
    /// verb that is not in it does not exist as far as that agent is concerned. This fails
    /// when a verb is added to the wire protocol and not to the text — which is precisely the
    /// change that would otherwise ship silently.
    #[test]
    fn every_verb_is_documented_in_the_help() {
        let every = [
            Request::Send { to: String::new(), text: String::new() },
            Request::NoteRead { path: String::new() },
            Request::NoteWrite { path: String::new(), text: String::new(), append: false },
            Request::NoteList,
            Request::Spawn(SpawnRequest::default()),
            Request::Image { data: String::new(), caption: None },
            Request::Options { prompt: String::new(), choices: Vec::new() },
            Request::Configure { node: String::new(), model: None },
        ];
        for request in &every {
            let verb = request.verb();
            // `note.read` is documented as `note read`, which is how it is typed.
            let typed = verb.replace('.', " ");
            assert!(
                HELP.contains(&typed),
                "the help does not mention `{typed}`, so an agent has no way to learn it exists"
            );
        }

        // And every exit code is listed, for the same reason: an agent branches on the code.
        for code in [exit::USAGE, exit::ENVIRONMENT, exit::UNREACHABLE, exit::REFUSED, exit::FAILED]
        {
            assert!(HELP.contains(&format!("\n    {code}  ")), "exit code {code} is undocumented");
        }
    }

    /// The response's own refusal code decides the exit code, so an agent can tell *"draw a
    /// connector"* from *"Velm is not running"* without reading English.
    #[test]
    fn a_refusal_keeps_its_kind_in_the_exit_code() {
        let cases = [
            (ErrorCode::NotConnected, exit::REFUSED),
            (ErrorCode::Refused, exit::REFUSED),
            (ErrorCode::BadToken, exit::REFUSED),
            (ErrorCode::UnknownAgent, exit::ENVIRONMENT),
            (ErrorCode::Malformed, exit::USAGE),
            (ErrorCode::Failed, exit::FAILED),
        ];
        for (code, expected) in cases {
            let response = Response::refused(code, "no");
            assert_eq!(refusal(&response).code, expected, "{code:?}");
        }
    }

    /// A note is printed verbatim — it is the thing that was asked for, and a status line
    /// around it would mean every reader had to strip one off.
    #[test]
    fn an_answer_is_described_as_the_thing_that_was_asked_for() {
        assert_eq!(describe(Some(Answer::Note { text: "# plan\n".into() })), "# plan");
        assert_eq!(describe(Some(Answer::Done)), "done");
        assert_eq!(describe(None), "done");
        assert_eq!(describe(Some(Answer::Spawned { agent: "9@1".into() })), "spawned 9@1");
        assert_eq!(
            describe(Some(Answer::Notes { notes: Vec::new() })),
            "no notes on this board yet"
        );

        let listed = describe(Some(Answer::Notes {
            notes: vec![vellum_agent::ipc::NoteEntry {
                path: "plan.md".into(),
                scope: vellum_agent::NoteScope::Private { agent: "1@2".into() },
                title: "The plan".into(),
            }],
        }));
        assert!(listed.contains("plan.md") && listed.contains("private"), "{listed}");
    }
}
