//! The three-layer rule cascade: global, project, and this node's own words.
//!
//! `docs/07-agent-canvas.md` §7 is the contract. Three layers, each inheriting from the one
//! above unless it overrides:
//!
//! | layer | where it lives |
//! |---|---|
//! | Global | `<data-dir>/rules/global.md` |
//! | Project | `<project>/.velm/rules.md`, or an existing `AGENTS.md`/`CLAUDE.md`/`.cursorrules` |
//! | Agent | [`AgentRules`] on the node |
//!
//! # Provenance is the feature, not a nicety
//!
//! [`resolve`] answers a [`ResolvedRules`] that records, **per field, which layer supplied
//! it**. The inspector renders *inherited* against *set here* from that record rather than
//! re-deriving it from the three inputs, which is the only arrangement in which the display
//! cannot disagree with what the agent actually got. This repo has paid for a second
//! derivation twice — the grid's two controls and `locked: false` — and both times the two
//! sources drifted in the direction nobody was looking.
//!
//! # Why front matter rather than a `rules.json` beside the markdown
//!
//! §7's table names `rules.json` next to `global.md`. It is deliberately not implemented:
//! two files describing the same four settings is a second source of truth for a value a
//! user edits by hand, and the failure mode — the `.md` says one thing and the `.json`
//! another — is unresolvable by anything but a coin toss. The structured half lives in the
//! markdown's own front matter, so one file is the whole layer and there is nothing to keep
//! in step. A `rules.json` holding *application* settings that are not rules is a different
//! concern and this module has no opinion about it.
//!
//! # Leniency is RULE ZERO's posture applied to config
//!
//! An unknown key is kept and ignored, a line with no colon is skipped, an unclosed
//! front-matter block is treated as body, and an absent file at any layer is an empty layer
//! rather than an error. A rules file is hand-written prose with a header on it; refusing to
//! start an agent because someone typed `tone formal` would be a worse outcome than every
//! failure it prevents.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::model::AgentRules;

/// Which layer a resolved setting came from.
///
/// **The derived ordering is the precedence order**, lowest to highest, and the resolution
/// loop relies on it: a later layer overwrites an earlier one because it comes later.
/// Reordering these variants silently changes what an agent is told, which is why
/// `the_layer_order_is_the_precedence_order` pins it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Layer {
    /// Nobody set it; this is the crate's own default. Shown as *not set*.
    Default,
    Global,
    Project,
    /// The node's own [`AgentRules`].
    Agent,
}

impl Layer {
    pub const ALL: [Self; 4] = [Self::Default, Self::Global, Self::Project, Self::Agent];

    pub const fn tag(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Global => "global",
            Self::Project => "project",
            Self::Agent => "agent",
        }
    }

    /// What the inspector puts beside a value.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Default => "Not set",
            Self::Global => "Inherited from your global rules",
            Self::Project => "Inherited from this project",
            Self::Agent => "Set here",
        }
    }

    /// The heading this layer's prose gets in the composed system context.
    pub const fn heading(self) -> &'static str {
        match self {
            Self::Default => "Rules",
            Self::Global => "Global rules",
            Self::Project => "Project rules",
            Self::Agent => "Rules for this agent",
        }
    }

    /// Whether a value from this layer came from above rather than from the node.
    ///
    /// [`Layer::Default`] is neither: nothing supplied it, so there is nothing to inherit
    /// *from* and nothing was set here either.
    pub const fn is_inherited(self) -> bool {
        matches!(self, Self::Global | Self::Project)
    }
}

/// How much an agent asks before acting.
///
/// A closed enum rather than a free string because this one is a **capability boundary**,
/// and the transports have to be able to match on it. The three answers are the three that
/// exist in every agent tool that has this setting; a fourth would be a scope, and scopes
/// belong to the permission request itself rather than to a posture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Permissions {
    /// Ask before anything that changes a file or runs a command. The default, and the only
    /// safe thing to default to.
    #[default]
    Ask,
    /// Read freely; ask before writing or running.
    Reads,
    /// Never ask. The user's own machine, the user's own call.
    All,
}

impl Permissions {
    pub const ALL: [Self; 3] = [Self::Ask, Self::Reads, Self::All];

    pub const fn tag(self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::Reads => "reads",
            Self::All => "all",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Ask => "Ask before changing anything",
            Self::Reads => "Read freely, ask before changing",
            Self::All => "Never ask",
        }
    }

    /// The sentence the agent is actually told. A posture the model never hears is a
    /// posture that does not exist, so the composed context always states one — including
    /// the default, because an agent that was told nothing guesses.
    pub const fn instruction(self) -> &'static str {
        match self {
            Self::Ask => "Ask before running a command or changing a file.",
            Self::Reads => {
                "Read whatever you need without asking. Ask before running a command or \
                 changing a file."
            }
            Self::All => "Act without asking for permission.",
        }
    }

    /// Parse a front-matter value.
    ///
    /// The alias table is four lines and it exists because this file is written by hand and
    /// may also be read by another tool: `allow` and `never_ask` are the spellings people
    /// reach for, and a fall-through on one of them looks like the setting was ignored.
    ///
    /// **An unrecognised value answers `None`, and the caller treats that as *unset at this
    /// layer*** — it falls through to the layer above rather than to any particular posture.
    /// Guessing the nearest match is how `never-allow` gets read as `allow`, and defaulting
    /// a typo to [`Permissions::All`] would be a permission granted by a spelling mistake.
    pub fn parse(value: &str) -> Option<Self> {
        let key: String =
            value.trim().to_lowercase().chars().map(|c| if c == '-' { '_' } else { c }).collect();
        match key.as_str() {
            "ask" | "always_ask" | "prompt" | "confirm" => Some(Self::Ask),
            "reads" | "read" | "read_only" | "allow_reads" => Some(Self::Reads),
            "all" | "allow" | "allow_all" | "never_ask" | "auto" => Some(Self::All),
            _ => None,
        }
    }
}

/// Which structured settings a rules file can carry.
///
/// Four, and the set is deliberately small: every one of these is a thing the *cascade* is
/// for — a house style that a project narrows and one agent overrides. Anything an
/// individual prompt could say belongs in the markdown body instead, where it costs nothing
/// to add and nothing to parse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Field {
    /// How the agent talks: formal, blunt, warm. The commonest thing a user wants to say
    /// once and never again.
    Tone,
    /// The shape of the answer: concise, detailed, bullet points, always show the diff.
    /// Distinct from tone because they move independently — a blunt agent can be verbose.
    Output,
    /// The language answers are written in. Its own field rather than a sentence in the body
    /// because it is the one setting a user checks at a glance, and because an agent given a
    /// paragraph of English house rules will answer in English unless told otherwise.
    Language,
    /// How much it asks before acting. See [`Permissions`] — the one field here that is a
    /// capability rather than a preference.
    Permissions,
}

impl Field {
    pub const ALL: [Self; 4] = [Self::Tone, Self::Output, Self::Language, Self::Permissions];

    /// The front-matter key. Stable: it is what a user typed into a file.
    pub const fn key(self) -> &'static str {
        match self {
            Self::Tone => "tone",
            Self::Output => "output",
            Self::Language => "language",
            Self::Permissions => "permissions",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Tone => "Tone",
            Self::Output => "Output style",
            Self::Language => "Language",
            Self::Permissions => "Permissions",
        }
    }
}

/// The structured half of a rules file: the `---` block at the top.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FrontMatter {
    pub tone: Option<String>,
    pub output: Option<String>,
    pub language: Option<String>,
    pub permissions: Option<Permissions>,
    /// Keys this build does not know, kept verbatim and in file order.
    ///
    /// Kept rather than dropped for the same reason [`crate::AgentModel`]'s token tolerates
    /// unknown JSON fields: a rules file written for a later build, or shared with another
    /// tool, must survive a round trip through this one. They also cascade, so a field added
    /// later needs no migration — it is already being resolved, under its own name.
    pub extra: BTreeMap<String, String>,
}

impl FrontMatter {
    /// Whether anything at all was set.
    pub fn is_empty(&self) -> bool {
        self.tone.is_none()
            && self.output.is_none()
            && self.language.is_none()
            && self.permissions.is_none()
            && self.extra.is_empty()
    }

    /// Parse the inside of a `---` block. Never fails.
    ///
    /// A line with no colon is skipped, a `#` comment is skipped, an empty value is not a
    /// setting, and a duplicate key takes the **last** value — which is the same rule the
    /// cascade itself follows, so a file and a stack of files behave the same way.
    pub fn parse(block: &str) -> Self {
        let mut front = Self::default();
        for line in block.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some(colon) = line.find(':') else { continue };
            // `colon` is the index of an ASCII byte, so both halves land on a boundary.
            let key = line.get(..colon).unwrap_or_default().trim();
            let value = unquote(line.get(colon + 1..).unwrap_or_default().trim());
            if key.is_empty() || value.is_empty() {
                continue;
            }
            let key: String =
                key.to_lowercase().chars().map(|c| if c == '-' { '_' } else { c }).collect();
            match key.as_str() {
                "tone" => front.tone = Some(value.to_owned()),
                "output" | "output_style" | "style" => front.output = Some(value.to_owned()),
                "language" | "lang" => front.language = Some(value.to_owned()),
                "permissions" | "permission" => match Permissions::parse(value) {
                    Some(posture) => front.permissions = Some(posture),
                    // Unrecognised: not a posture, so not set here — and kept, so the
                    // inspector can show the user what it could not read.
                    None => {
                        front.extra.insert(key, value.to_owned());
                    }
                },
                _ => {
                    front.extra.insert(key, value.to_owned());
                }
            }
        }
        front
    }

    /// The inside of the `---` block, as [`Self::parse`] would read it back.
    ///
    /// # The round trip is the contract, not a nicety
    ///
    /// `parse(write(x)) == x` for every value `parse` can produce, and
    /// `writing_a_layer_and_reading_it_back_is_the_same_layer` pins it. A rules file is
    /// hand-written and may also be read by another tool, so a save that reformatted what
    /// somebody typed into a shape their next `git diff` does not recognise would be a
    /// worse outcome than not offering to save at all.
    ///
    /// Two ordering rules make a file this has touched read the same way twice: the four
    /// named settings in [`Field::ALL`]'s order, then the unknown keys in the `BTreeMap`'s.
    ///
    /// **The unknown keys are written, not dropped.** They are the whole reason a file
    /// written for a later build survives this one, and dropping them here would make the
    /// editor a downgrade — the parser keeps them precisely so that a save does not.
    ///
    /// **An empty value is skipped**, because [`Self::parse`] skips one: `tone: ""` reads
    /// back as *not set*, so writing it would make the round trip fail on a value nobody
    /// can produce by hand anyway.
    ///
    /// **Empty, not blank.** `parse` tests `value.is_empty()` *after* unquoting, so
    /// `tone: "  "` is a setting whose value is two spaces — absurd, and a value the round
    /// trip has to carry all the same. Skipping on `trim().is_empty()` here would be the
    /// writer disagreeing with the parser about what a setting is, which is the one thing
    /// this pair may not do.
    pub fn to_block(&self) -> String {
        let mut out = String::new();
        let mut line = |key: &str, value: &str| {
            if value.is_empty() {
                return;
            }
            out.push_str(key);
            out.push_str(": ");
            out.push_str(&write_value(value));
            out.push('\n');
        };
        if let Some(tone) = &self.tone {
            line(Field::Tone.key(), tone);
        }
        if let Some(output) = &self.output {
            line(Field::Output.key(), output);
        }
        if let Some(language) = &self.language {
            line(Field::Language.key(), language);
        }
        if let Some(permissions) = self.permissions {
            line(Field::Permissions.key(), permissions.tag());
        }
        // After the named four, so a `permissions:` this build could not read — which
        // `parse` files under `extra` beside a posture it *could* — lands on the later line
        // and is refused again on the way back in, leaving both halves exactly where they
        // were. `an_unreadable_permission_survives_a_round_trip` measures that.
        for (key, value) in &self.extra {
            line(key, value);
        }
        out
    }
}

/// A front-matter value, quoted only when a bare one would not read back the same.
///
/// [`FrontMatter::parse`] trims the value and then strips **one** matching pair of quotes,
/// so three shapes need protecting and nothing else does: a value with leading or trailing
/// space, a value that is itself quoted (`"formal"` written bare comes back as `formal`),
/// and a value carrying a newline, which would close the block early and turn the rest of
/// the file into body.
///
/// Wrapping in `"` is always safe, including for a value that already begins and ends with
/// one: `unquote` strips a single pair, so `""x""` comes back as `"x"`. Quoting
/// unconditionally was the alternative and is worse — it would put quotation marks around
/// every setting in a file a person reads and edits, to defend against a case that occurs
/// approximately never.
fn write_value(value: &str) -> String {
    // A newline cannot survive as itself; the block is line-based. Flattened rather than
    // refused, because the caller is a text field and a refusal here would lose the edit.
    let flat: String = value
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect();
    let quoted = flat != flat.trim() || unquote(&flat) != flat;
    if quoted { format!("\"{flat}\"") } else { flat }
}

/// One layer's file: what it set, what it said, and whether it could be read at all.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RuleFile {
    /// Where it came from, when it came from a file. `None` for a layer parsed from a
    /// string — the agent's own text, or a test.
    pub path: Option<PathBuf>,
    pub front: FrontMatter,
    /// The markdown after the front matter, trimmed. Free instructions; nothing here reads
    /// it beyond composing it into the system context.
    pub body: String,
    /// Why this layer is empty when it should not have been.
    ///
    /// A file that exists and cannot be read must not look identical to one that was never
    /// written — that is the difference between *"you have no project rules"* and *"your
    /// project rules are not being applied"*, and only one of those is worth telling
    /// somebody about.
    pub error: Option<String>,
}

impl RuleFile {
    /// Split a rules document into its front matter and its body.
    pub fn parse(text: &str) -> Self {
        let (block, body) = split_front_matter(text);
        Self {
            path: None,
            front: block.map(FrontMatter::parse).unwrap_or_default(),
            body: body.trim().to_owned(),
            error: None,
        }
    }

    /// Read a rules file. **An absent file is an empty layer, not an error** — which is what
    /// makes every layer optional without a single caller having to ask whether it exists.
    pub fn read(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(text) => Self { path: Some(path.to_path_buf()), ..Self::parse(&text) },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(error) => Self {
                path: Some(path.to_path_buf()),
                error: Some(error.to_string()),
                ..Self::default()
            },
        }
    }

    pub fn is_empty(&self) -> bool {
        self.body.is_empty() && self.front.is_empty()
    }

    /// The whole document, front matter and body, exactly as [`Self::parse`] would read it
    /// back. The inverse of `parse`, and it has to be precisely that — see
    /// [`FrontMatter::to_block`].
    ///
    /// Three things are load-bearing:
    ///
    /// - **The `---` is the very first line.** [`split_front_matter`] answers *no front
    ///   matter* the moment the first non-empty line is anything else, so a leading blank
    ///   line would silently turn every setting into prose.
    /// - **An empty front matter is omitted** — a layer that sets nothing is a plain
    ///   markdown file, which is what somebody who opened it in an editor would write.
    /// - **Except when the body itself opens with `---`.** That is the one case where
    ///   omitting the block would corrupt rather than merely reformat: the body's own
    ///   horizontal rule would be re-read as an opening fence and everything down to the
    ///   next rule would be swallowed as settings. An empty block in front of it costs two
    ///   lines and makes the shape unambiguous.
    ///
    /// `path` and `error` are **not** written. They describe this process's relationship
    /// with a file, not the layer's content, which is why the round-trip test compares
    /// content rather than the whole value.
    pub fn to_markdown(&self) -> String {
        let body = self.body.trim();
        let block = self.front.to_block();

        if block.is_empty() {
            return if body.starts_with("---") {
                format!("---\n---\n\n{body}\n")
            } else if body.is_empty() {
                String::new()
            } else {
                format!("{body}\n")
            };
        }

        let mut out = String::from("---\n");
        out.push_str(&block);
        out.push_str("---\n");
        if !body.is_empty() {
            out.push('\n');
            out.push_str(body);
            out.push('\n');
        }
        out
    }

    /// Whether two layers say the same thing, ignoring where they were read from.
    ///
    /// Content, not bytes. Two files that differ only in the order of their front matter or
    /// in a trailing newline *are* the same layer, and calling that a conflict would refuse
    /// a save because somebody's editor added a final newline.
    pub fn says_the_same_as(&self, other: &Self) -> bool {
        self.front == other.front && self.body == other.body
    }

    /// Write this layer to disk, refusing to overwrite an edit made underneath it.
    ///
    /// # Creating the file is the ordinary case
    ///
    /// `<data-dir>/rules/global.md` does not exist until somebody writes a global rule set,
    /// and the row that offers to is exactly the row a user reaches for to *start* one. So
    /// an absent file is [`Saved::Created`] and not an error, the parent directories are
    /// made, and `seen` is [`RuleFile::default`] for that case — an absent layer and an
    /// empty one are the same layer, which is the rule [`Self::read`] already follows.
    ///
    /// # `seen` is what the editor was opened on
    ///
    /// A rules file is *meant* to be edited by hand and by other tools, so between opening
    /// the editor and pressing Save the file may have moved. This re-reads it and compares
    /// against the layer the caller started from; a difference answers [`Saved::Conflict`]
    /// and **writes nothing**. Blind overwriting is what a modal editor over a shared file
    /// does wrong, and the user still has their text — it is in the editor in front of them.
    ///
    /// That is deliberately *not* [`crate::notes`]'s treatment, which keeps both sides in a
    /// `.velm-conflict.md` beside the file. A note's canvas node has already been replaced
    /// by the time the conflict is discovered, so the buffer would be lost if it were not
    /// written somewhere; a rules dialog can simply stay open. Littering a project with
    /// conflict files for a settings save would be a worse trade.
    ///
    /// The write itself is [`crate::notes::write_atomically`] — a temp file in the same
    /// directory, `sync_all`, then a rename — so a reader sees the old text or the new one
    /// and never a prefix of either.
    pub fn save(&self, path: &Path, seen: &Self) -> crate::Result<Saved> {
        let on_disk = match std::fs::read_to_string(path) {
            Ok(text) => Some(text),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            // Not a conflict and not a create: a file that exists and cannot be read is a
            // file whose content is unknown, and writing over it would be exactly the
            // clobber this function exists to refuse.
            Err(error) => return Err(crate::AgentError::file(path.display().to_string(), &error)),
        };

        let current = on_disk.as_deref().map_or_else(Self::default, Self::parse);
        if !current.says_the_same_as(seen) {
            return Ok(Saved::Conflict { on_disk: Box::new(current) });
        }
        if on_disk.is_some() && current.says_the_same_as(self) {
            return Ok(Saved::Unchanged);
        }

        crate::notes::write_atomically(path, &self.to_markdown())?;
        Ok(if on_disk.is_some() { Saved::Written } else { Saved::Created })
    }
}

/// What [`RuleFile::save`] did.
///
/// Four answers rather than a `bool`, because the caller says something different about
/// each: *created* names a file that did not exist a moment ago and is worth showing,
/// *written* is the quiet case, *unchanged* must not claim a save that did not happen, and
/// *conflict* is the one where nothing was written and the user has to decide.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Saved {
    /// The file did not exist and now does, parent directories and all.
    Created,
    /// An existing file was replaced.
    Written,
    /// The file already said exactly this. Nothing was written.
    Unchanged,
    /// Somebody else changed the file since it was read. **Nothing was written**, and the
    /// layer that is actually on disk comes back so the caller can offer it.
    ///
    /// Boxed because it is much the largest variant and the common answers are unit-sized;
    /// a `Result<Saved>` returned per keystroke-free save should not be a `String`-sized
    /// value in every arm.
    Conflict { on_disk: Box<RuleFile> },
}

impl Saved {
    /// Whether the file on disk now holds what was asked for.
    ///
    /// [`Self::Unchanged`] counts: nothing was written because nothing needed to be. Only a
    /// conflict leaves the user's text unsaved.
    pub const fn is_saved(&self) -> bool {
        matches!(self, Self::Created | Self::Written | Self::Unchanged)
    }
}

/// Where the global layer lives under the app's data directory.
pub const GLOBAL_RULES_PATH: [&str; 2] = ["rules", "global.md"];

/// The project layer, in the order it is looked for.
///
/// - **`.velm/rules.md` first** because it is the only one Velm itself writes: a user who
///   made one meant it, and it must not be shadowed by a file another tool left behind.
/// - **`AGENTS.md`** next as the vendor-neutral convention several agent tools now read.
/// - **`CLAUDE.md`** next — extremely common, and in this very repository.
/// - **`.cursorrules`** last: the oldest of the four, most likely to be a stale artefact of
///   an editor somebody used once.
///
/// First hit wins. They are not merged: a project with both an `AGENTS.md` and a `CLAUDE.md`
/// has two files that say overlapping things, and concatenating them would produce a context
/// that repeats itself and contradicts itself in the same breath.
pub const PROJECT_RULE_FILES: [&str; 4] =
    [".velm/rules.md", "AGENTS.md", "CLAUDE.md", ".cursorrules"];

/// `<data-dir>/rules/global.md`.
pub fn global_rules_path(data_dir: &Path) -> PathBuf {
    let mut path = data_dir.to_path_buf();
    for part in GLOBAL_RULES_PATH {
        path.push(part);
    }
    path
}

pub fn load_global(data_dir: &Path) -> RuleFile {
    RuleFile::read(&global_rules_path(data_dir))
}

/// The project rules file in use, in [`PROJECT_RULE_FILES`] order, or `None`.
///
/// `is_file` rather than `exists`, so a directory that happens to be called `CLAUDE.md` is
/// not selected and then reported as unreadable.
pub fn project_rules_path(project: &Path) -> Option<PathBuf> {
    PROJECT_RULE_FILES
        .iter()
        .map(|name| project.join(name))
        .find(|candidate| candidate.is_file())
}

/// Where a project layer is **written**, which is not always where one is read from.
///
/// [`project_rules_path`] answers *which of four files this project actually uses*, and
/// three of the four belong to other tools. Velm writes exactly one of them —
/// `.velm/rules.md`, the first entry in [`PROJECT_RULE_FILES`] — so a project that has an
/// `AGENTS.md` gets a `.velm/rules.md` that then **shadows** it, which is the documented
/// precedence and the only outcome a user could predict.
///
/// Writing back into a `CLAUDE.md` was the alternative and is the wrong one twice over: that
/// file is usually somebody else's, under version control, and much larger than the four
/// settings this editor knows about — a save composed from a parsed front matter and body
/// would rewrite a document the editor only partly understands.
pub fn project_rules_target(project: &Path) -> PathBuf {
    let mut path = project.to_path_buf();
    for part in PROJECT_RULE_FILES[0].split('/') {
        path.push(part);
    }
    path
}

/// The project layer, or an empty one when the project has no rules file at all.
pub fn load_project(project: &Path) -> RuleFile {
    project_rules_path(project).as_deref().map(RuleFile::read).unwrap_or_default()
}

/// A resolved value and the layer that supplied it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sourced<T> {
    pub value: T,
    pub from: Layer,
}

/// One row of the inspector's rules section.
///
/// Built from the resolution rather than from the three inputs, which is the whole point of
/// the module: *inherited* and *set here* are read off the row's own `from`, so the display
/// cannot say one thing while the agent was told another.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingRow {
    pub field: Field,
    /// `None` when nothing set it — the row still exists, so the inspector shows the whole
    /// set of settings rather than only the ones somebody happened to fill in.
    pub value: Option<String>,
    pub from: Layer,
}

/// One layer's prose, in cascade order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    pub layer: Layer,
    pub text: String,
}

/// What an agent is actually run with, and where every part of it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedRules {
    /// The node's role label, as the caller supplied it. This crate never reads the
    /// document, so the label arrives as a parameter — see [`resolve`].
    pub role: String,
    /// Whether the global and project layers applied at all. `false` for a node with
    /// [`AgentRules::ignore_inherited`] set.
    pub inherits: bool,
    pub tone: Option<Sourced<String>>,
    pub output: Option<Sourced<String>>,
    pub language: Option<Sourced<String>>,
    /// Always resolves: `from == Layer::Default` when nobody set it.
    pub permissions: Sourced<Permissions>,
    /// Keys no layer's parser knew, cascaded exactly as the named fields are.
    pub extra: BTreeMap<String, Sourced<String>>,
    /// The prose, one entry per layer that had any, in the order it is composed.
    pub sections: Vec<Section>,
}

/// Resolve the three layers into one answer, recording where each part of it came from.
///
/// `role` is the node's role label, passed in because this crate does not read the document
/// — `vellum-app` owns that join, exactly as it does for every other token here.
///
/// # What wins
///
/// Agent over project over global, per field, independently. A layer that does not set a
/// field does not clear it: leaving `tone` out of a project's front matter inherits the
/// global tone, which is the difference between a cascade and a stack of complete
/// configurations.
///
/// # `ignore_inherited` is a cliff, not a slope
///
/// It removes the global and project layers **entirely** — their settings as well as their
/// prose — so [`ResolvedRules::permissions`] falls back to the crate default rather than to
/// whatever the global file said. Partial inheritance was considered and rejected: "this
/// agent ignores the house style except for the parts it does not mention" is not a
/// sentence anybody can reason about, and the failure it produces is an agent behaving as
/// though it had been told something nobody can find in a file.
pub fn resolve(
    global: &RuleFile,
    project: &RuleFile,
    agent: &AgentRules,
    role: &str,
) -> ResolvedRules {
    // The agent's own text is parsed with the same parser, so an agent can set a field in
    // its three lines of override rather than only in prose. `agent_dialogs.rs`'s editor
    // must offer the same shape; there is nowhere else a per-node structured setting could
    // live, because `AgentRules` is one string and a list.
    let own = RuleFile::parse(&agent.text);

    let mut layers: Vec<(Layer, &RuleFile)> = Vec::with_capacity(3);
    if !agent.ignore_inherited {
        layers.push((Layer::Global, global));
        layers.push((Layer::Project, project));
    }
    layers.push((Layer::Agent, &own));

    let mut resolved = ResolvedRules {
        role: role.trim().to_owned(),
        inherits: !agent.ignore_inherited,
        tone: None,
        output: None,
        language: None,
        permissions: Sourced { value: Permissions::default(), from: Layer::Default },
        extra: BTreeMap::new(),
        sections: Vec::new(),
    };

    for (layer, file) in layers {
        if let Some(tone) = &file.front.tone {
            resolved.tone = Some(Sourced { value: tone.clone(), from: layer });
        }
        if let Some(output) = &file.front.output {
            resolved.output = Some(Sourced { value: output.clone(), from: layer });
        }
        if let Some(language) = &file.front.language {
            resolved.language = Some(Sourced { value: language.clone(), from: layer });
        }
        if let Some(permissions) = file.front.permissions {
            resolved.permissions = Sourced { value: permissions, from: layer };
        }
        for (key, value) in &file.front.extra {
            resolved.extra.insert(key.clone(), Sourced { value: value.clone(), from: layer });
        }
        if !file.body.is_empty() {
            resolved.sections.push(Section { layer, text: file.body.clone() });
        }
    }

    resolved
}

impl ResolvedRules {
    /// Which layer supplied a field, [`Layer::Default`] when none did.
    pub fn provenance(&self, field: Field) -> Layer {
        match field {
            Field::Tone => self.tone.as_ref().map_or(Layer::Default, |s| s.from),
            Field::Output => self.output.as_ref().map_or(Layer::Default, |s| s.from),
            Field::Language => self.language.as_ref().map_or(Layer::Default, |s| s.from),
            Field::Permissions => self.permissions.from,
        }
    }

    /// Every setting, set or not, for the inspector to draw.
    pub fn rows(&self) -> Vec<SettingRow> {
        Field::ALL
            .iter()
            .map(|&field| {
                let value = match field {
                    Field::Tone => self.tone.as_ref().map(|s| s.value.clone()),
                    Field::Output => self.output.as_ref().map(|s| s.value.clone()),
                    Field::Language => self.language.as_ref().map(|s| s.value.clone()),
                    // Permissions always has one, so the row is never blank — the posture is
                    // the one setting where "not set" would leave the reader guessing which
                    // way the default falls.
                    Field::Permissions => Some(self.permissions.value.label().to_owned()),
                };
                SettingRow { field, value, from: self.provenance(field) }
            })
            .collect()
    }

    /// The front-matter keys this node set itself.
    ///
    /// This is what belongs in [`AgentRules::overrides`]: that vector is a **cache of what
    /// resolution decided**, written back from here, never authored by hand. A hand-written
    /// list is a second source of truth for provenance, and it would be the one the
    /// inspector believed while the agent ran on the other one.
    pub fn override_names(&self) -> Vec<String> {
        Field::ALL
            .iter()
            .filter(|&&field| self.provenance(field) == Layer::Agent)
            .map(|&field| field.key().to_owned())
            .collect()
    }

    /// The system context the agent is launched with.
    ///
    /// # The order is the behaviour
    ///
    /// 1. **The role**, first, because it is identity rather than instruction — an agent
    ///    reads "you are the Reviewer" as the frame the rest is read inside.
    /// 2. **The resolved settings**, as sentences. Already cascaded, so their position says
    ///    nothing about precedence; they lead because they are short.
    /// 3. **Global, then project, then this agent's own words — last.** Later text wins with
    ///    every model there is, which is the same direction the settings cascade, so a user
    ///    who overrides in prose and a user who overrides in front matter get the same
    ///    answer. `the_composition_puts_the_agents_own_words_last` pins the order.
    ///
    /// A layer with nothing in it contributes **no heading**: an empty *Project rules*
    /// section reads as a project whose rules are "none", which is not what an absent file
    /// means.
    pub fn system_context(&self) -> String {
        self.system_context_with(None)
    }

    /// The system context, plus the section that tells the agent it can act on the board.
    ///
    /// # ⚠ This is functional text, not documentation
    ///
    /// Nothing else in the application makes agent-to-agent messaging, notes, spawn or
    /// options *happen*: the shim is on the agent's `PATH` and the MCP tools are registered,
    /// and a model that has not been told either exists will use neither. So the wording is
    /// load-bearing in the way a function body is, and three choices in it are deliberate.
    ///
    /// - **It is conditional on the shim having been found.** An agent told about a command
    ///   it has not got is strictly worse than one told nothing: it will try, fail, and spend
    ///   a turn explaining a tool the user never asked it to use. [`BoardTools`] is `None`
    ///   when nothing was shipped beside the application, and this section vanishes.
    /// - **It points at `--help` rather than reciting every argument.** The shim's own help
    ///   is written for exactly this reader (see `bin/velm_agent_cli.rs`), it is generated
    ///   from the binary that is actually installed, and it cannot drift from it. What is
    ///   here is *which verbs exist and when they are the right move*, which help text is bad
    ///   at.
    /// - **`spawn` appears only for a role that may use it.** Velm refuses it at the IPC
    ///   boundary for everyone else, so listing it for a worker would advertise a capability
    ///   whose only possible outcome is a refusal.
    ///
    /// It is placed **before** the rule sections, not after. `system_context`'s own ordering
    /// note says later text wins, and a house style or a project instruction must be able to
    /// say *"do not spawn helpers on this board"* and be obeyed over this.
    pub fn system_context_with(&self, tools: Option<&BoardTools>) -> String {
        let mut out = String::new();

        if !self.role.is_empty() {
            out.push_str(&format!("You are {}, an agent on a Velm board.\n", self.role));
        }

        let mut settings = Vec::new();
        if let Some(language) = &self.language {
            settings.push(sentence(&format!("Answer in {}", language.value)));
        }
        if let Some(tone) = &self.tone {
            settings.push(sentence(&format!("Tone: {}", tone.value)));
        }
        if let Some(output) = &self.output {
            settings.push(sentence(&format!("Output style: {}", output.value)));
        }
        settings.push(self.permissions.value.instruction().to_owned());

        out.push_str("\n## How to answer\n\n");
        for line in settings {
            out.push_str("- ");
            out.push_str(&line);
            out.push('\n');
        }

        if let Some(tools) = tools {
            out.push_str(&tools.section());
        }

        for section in &self.sections {
            out.push_str(&format!("\n## {}\n\n{}\n", section.layer.heading(), section.text));
        }

        out
    }
}

/// What this agent can actually reach on the board, as the system context should describe it.
///
/// Plain data with no dependency on [`crate::transport`], deliberately: the section it
/// produces is the thing worth testing, and a test that had to build a `LaunchSpec` and stat
/// two files to check a sentence is a test nobody writes. `vellum-app` fills it in from the
/// shim it resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoardTools {
    /// The shim's command name, as it will resolve on the agent's `PATH` —
    /// `crate::transport::AGENT_CLI`. Passed rather than hardcoded so the name is spelled
    /// once in the workspace.
    pub command: String,
    /// Whether this node may create other agents. `false` for a worker, and the reason this
    /// is a field rather than a sentence: Velm refuses `spawn` at the IPC boundary for every
    /// role but orchestrator and meta, so offering it to a worker only ever produces a
    /// wasted turn and a refusal.
    pub may_spawn: bool,
    /// Whether the same verbs are also registered as MCP tools. Worth one line, because an
    /// agent that has them will otherwise shell out for something it could call directly —
    /// and because an agent that has *neither* must not be told it has one.
    pub mcp: bool,
}

impl BoardTools {
    /// The whole of what an agent is told about acting on the board.
    ///
    /// Kept to a screen. Every line is a verb and the condition under which it is the right
    /// move; there is no explanation of what a canvas is, because the agent does not need a
    /// model of the application to use a command correctly, and every sentence spent on one
    /// is a sentence competing with the user's own instructions further down.
    pub fn section(&self) -> String {
        let command = &self.command;
        let mut out = String::from("\n## Acting on the board\n\n");
        out.push_str(&format!(
            "You are running inside Velm, on a board a person is looking at. `{command}` is on \
             your PATH and is how you reach that board. Run `{command} --help` before your \
             first call — it is generated by the copy that is installed, so it is right about \
             the arguments and this list is not.\n\n"
        ));

        out.push_str(&format!(
            "- `{command} send <agent> <text>` — say something to another agent. It is \
             delivered only where a connector on the board joins you to it; a refusal means \
             the person has not drawn that line, so ask them for one rather than looking for \
             another route.\n"
        ));
        out.push_str(&format!(
            "- `{command} note read|write|list` — the board's shared markdown notes. Read the \
             relevant note before you start and write what you found back into it. A note is \
             what the other agents and the person read later; your reply is not.\n"
        ));
        out.push_str(&format!(
            "- `{command} image <file>` — put a picture in front of the person. Use it for a \
             chart, a diagram or a screenshot instead of describing one in prose.\n"
        ));
        out.push_str(&format!(
            "- `{command} options <question> --choice a=… --choice b=…` — when you have two or \
             three real alternatives, offer them as choices they can click. Not for open \
             questions, which are better asked in your reply.\n"
        ));
        if self.may_spawn {
            out.push_str(&format!(
                "- `{command} spawn <label> --prompt <text>` — create another agent to take a \
                 piece of this work. You may do this; most agents may not. Give each one a \
                 label a person can read, and prefer delegating a separable piece over doing \
                 everything in this one conversation.\n"
            ));
        }
        if self.mcp {
            out.push_str(
                "\nThe same verbs are also available to you as tools named `velm_send_message`, \
                 `velm_read_note`, `velm_write_note`, `velm_list_notes`, `velm_post_image` and \
                 `velm_post_options`. Prefer the tools if you have them; the command above does \
                 the same thing and is there either way.\n",
            );
        }
        out.push_str(
            "\nUse these when reaching the board is the point of what you were asked. An \
             ordinary answer still goes back as your reply — do not narrate every call, and do \
             not use them to acknowledge instructions.\n",
        );
        out
    }
}

/// Strip one matching pair of surrounding quotes, so `tone: "formal"` and `tone: formal`
/// are the same setting. Only a *matching* pair, so an apostrophe in `tone: don't fuss`
/// survives.
fn unquote(value: &str) -> &str {
    for quote in ['"', '\''] {
        if let Some(inner) = value.strip_prefix(quote).and_then(|v| v.strip_suffix(quote)) {
            return inner;
        }
    }
    value
}

/// End a fragment with a full stop unless it already ends with punctuation.
fn sentence(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.ends_with(['.', '!', '?', ':', ';']) {
        trimmed.to_owned()
    } else {
        format!("{trimmed}.")
    }
}

/// Split a leading `---` block off a document.
///
/// Returns `(front matter, body)`. **An unclosed block is not front matter** — a document
/// that opens with a horizontal rule is a perfectly ordinary markdown document, and reading
/// the rest of it as `key: value` pairs would swallow the file. The closing delimiter may be
/// `---` or `...`, which is what a YAML writer emits.
fn split_front_matter(text: &str) -> (Option<&str>, &str) {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);

    let mut cursor = 0usize;
    let mut opened = false;
    let mut block_start = 0usize;

    while let Some(rest) = text.get(cursor..) {
        if rest.is_empty() {
            break;
        }
        // `find` answers a byte index of an ASCII byte, so every slice below is on a
        // boundary. `get`, not `[..]`, everywhere regardless — this file reads arbitrary
        // user-written text and `panic = "abort"` in the release profile means a slicing
        // mistake here takes the whole application down (feedback 30).
        let (line, advance) = match rest.find('\n') {
            Some(newline) => (rest.get(..newline).unwrap_or(rest), newline + 1),
            None => (rest, rest.len()),
        };
        let trimmed = line.trim();

        if !opened {
            if trimmed != "---" {
                return (None, text);
            }
            opened = true;
            block_start = cursor + advance;
        } else if trimmed == "---" || trimmed == "..." {
            let block = text.get(block_start..cursor).unwrap_or_default();
            let body = text.get(cursor + advance..).unwrap_or_default();
            return (Some(block), body);
        }
        cursor += advance;
    }

    (None, text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(text: &str) -> RuleFile {
        RuleFile::parse(text)
    }

    /// The derived ordering *is* the precedence order, and the resolution loop relies on it.
    /// Reordering the variants would silently change what an agent is told.
    #[test]
    fn the_layer_order_is_the_precedence_order() {
        assert!(Layer::Default < Layer::Global);
        assert!(Layer::Global < Layer::Project);
        assert!(Layer::Project < Layer::Agent);
        assert!(Layer::Global.is_inherited() && Layer::Project.is_inherited());
        assert!(!Layer::Agent.is_inherited(), "the node's own layer is not inherited");
        assert!(!Layer::Default.is_inherited(), "nothing supplied it, so nothing inherited it");
    }

    /// The feature, exhaustively: for every combination of which layers set `tone`, the
    /// resolution must name the *lowest* layer that set it, and carry that layer's value.
    ///
    /// A resolution that always answered `Agent`, or that recorded the layer it looked at
    /// first rather than the one it took the value from, passes a single-case test and fails
    /// six of these eight.
    #[test]
    fn provenance_names_the_layer_that_actually_supplied_the_value() {
        for global_set in [false, true] {
            for project_set in [false, true] {
                for agent_set in [false, true] {
                    let global = file(if global_set { "---\ntone: g\n---\n" } else { "" });
                    let project = file(if project_set { "---\ntone: p\n---\n" } else { "" });
                    let agent = AgentRules {
                        text: if agent_set { "---\ntone: a\n---\n".into() } else { String::new() },
                        ..AgentRules::default()
                    };

                    let resolved = resolve(&global, &project, &agent, "Reviewer");
                    let expected = if agent_set {
                        Some((Layer::Agent, "a"))
                    } else if project_set {
                        Some((Layer::Project, "p"))
                    } else if global_set {
                        Some((Layer::Global, "g"))
                    } else {
                        None
                    };

                    match (expected, &resolved.tone) {
                        (None, None) => {
                            assert_eq!(resolved.provenance(Field::Tone), Layer::Default);
                        }
                        (Some((layer, value)), Some(got)) => {
                            assert_eq!(got.from, layer, "{global_set}/{project_set}/{agent_set}");
                            assert_eq!(got.value, value, "the wrong layer's value survived");
                            assert_eq!(resolved.provenance(Field::Tone), layer);
                        }
                        (expected, got) => panic!("expected {expected:?}, resolved {got:?}"),
                    }
                }
            }
        }
    }

    /// A layer that is silent about a field inherits it. This is the difference between a
    /// cascade and three complete configurations: leaving `tone` out of a project's front
    /// matter must not clear the global tone.
    #[test]
    fn a_silent_layer_inherits_rather_than_clearing() {
        let global = file("---\ntone: formal\nlanguage: Turkish\npermissions: reads\n---\n");
        let project = file("---\noutput: concise\n---\nUse the repo's own vocabulary.\n");
        let agent = AgentRules { text: "---\ntone: blunt\n---\n".into(), ..AgentRules::default() };

        let resolved = resolve(&global, &project, &agent, "Reviewer");
        assert_eq!(resolved.tone.as_ref().unwrap().value, "blunt");
        assert_eq!(resolved.provenance(Field::Tone), Layer::Agent);
        assert_eq!(resolved.language.as_ref().unwrap().value, "Turkish");
        assert_eq!(resolved.provenance(Field::Language), Layer::Global);
        assert_eq!(resolved.output.as_ref().unwrap().value, "concise");
        assert_eq!(resolved.provenance(Field::Output), Layer::Project);
        assert_eq!(resolved.permissions.value, Permissions::Reads);
        assert_eq!(resolved.provenance(Field::Permissions), Layer::Global);

        // And the derived override list is exactly what this node set itself.
        assert_eq!(resolved.override_names(), vec!["tone".to_owned()]);
    }

    /// The cliff. Both halves matter, and the second is the one a partial implementation
    /// fails: an implementation that skipped only the upper layers' *prose* would leave the
    /// global permission posture in place, so this asserts it falls all the way back to the
    /// crate default rather than to `reads`.
    #[test]
    fn ignore_inherited_cuts_the_upper_layers_entirely() {
        let global = file("---\ntone: formal\npermissions: all\n---\nHouse style.\n");
        let project = file("---\noutput: concise\n---\nProject style.\n");
        let agent = AgentRules {
            text: "Answer in one line.".into(),
            ignore_inherited: true,
            ..AgentRules::default()
        };

        let resolved = resolve(&global, &project, &agent, "Loner");
        assert!(!resolved.inherits);
        assert_eq!(resolved.tone, None, "an inherited setting survived the cliff");
        assert_eq!(resolved.output, None);
        assert_eq!(
            resolved.permissions,
            Sourced { value: Permissions::Ask, from: Layer::Default },
            "the global posture survived a cut that was supposed to remove the layer"
        );

        let context = resolved.system_context();
        assert!(!context.contains("House style."), "{context}");
        assert!(!context.contains("Project style."), "{context}");
        assert!(context.contains("Answer in one line."));
        // And the safe posture is still stated, because an agent told nothing guesses.
        assert!(context.contains("Ask before running a command"), "{context}");
    }

    /// Later layers win by being later, in text as well as in settings.
    #[test]
    fn the_composition_puts_the_agents_own_words_last() {
        let global = file("Global body.");
        let project = file("Project body.");
        let agent = AgentRules { text: "Agent body.".into(), ..AgentRules::default() };

        let context = resolve(&global, &project, &agent, "Reviewer").system_context();
        let role = context.find("You are Reviewer").expect("the role label is missing");
        let g = context.find("Global body.").expect("the global layer is missing");
        let p = context.find("Project body.").expect("the project layer is missing");
        let a = context.find("Agent body.").expect("the agent layer is missing");
        assert!(role < g && g < p && p < a, "composed out of order: {context}");

        // Each layer is named where it appears, so a reader can tell where a rule came from.
        assert!(context.contains("## Global rules"));
        assert!(context.contains("## Project rules"));
        assert!(context.contains("## Rules for this agent"));
    }

    /// An absent layer contributes no heading. An empty *Project rules* section reads as a
    /// project whose rules are "none", which is not what a missing file means.
    #[test]
    fn an_empty_layer_contributes_no_heading() {
        let agent = AgentRules { text: "Only this.".into(), ..AgentRules::default() };
        let context =
            resolve(&RuleFile::default(), &RuleFile::default(), &agent, "Solo").system_context();
        assert!(!context.contains("## Global rules"), "{context}");
        assert!(!context.contains("## Project rules"), "{context}");
        assert!(context.contains("## Rules for this agent"));
    }

    /// Malformed front matter degrades: the good keys still parse, a line with no colon is
    /// skipped rather than fatal, an unknown key is kept, and a comment is ignored.
    #[test]
    fn malformed_front_matter_degrades_rather_than_failing() {
        let parsed = file(
            "---\n\
             # a comment\n\
             this line has no colon\n\
             tone: formal\n\
             : no key\n\
             output:\n\
             telepathy: yes\n\
             LANGUAGE: Turkish\n\
             output-style: bullets\n\
             ---\n\
             The body survives.\n",
        );
        assert_eq!(parsed.front.tone.as_deref(), Some("formal"));
        assert_eq!(parsed.front.output.as_deref(), Some("bullets"), "a `-` key was not normalised");
        assert_eq!(parsed.front.language.as_deref(), Some("Turkish"), "a key was case-sensitive");
        assert_eq!(parsed.front.extra.get("telepathy").map(String::as_str), Some("yes"));
        assert_eq!(parsed.body, "The body survives.");
        assert_eq!(parsed.error, None, "a malformed line was reported as a read failure");
    }

    /// A duplicate key takes the last value — the same rule the cascade follows, so a file
    /// and a stack of files behave the same way.
    #[test]
    fn a_duplicate_key_takes_the_last_value() {
        let parsed = file("---\ntone: formal\ntone: blunt\n---\n");
        assert_eq!(parsed.front.tone.as_deref(), Some("blunt"));
    }

    /// A document that opens with a horizontal rule is an ordinary markdown document.
    /// Reading the rest of it as `key: value` would swallow the file.
    #[test]
    fn an_unclosed_front_matter_block_is_treated_as_body() {
        let parsed = file("---\nNot front matter at all.\n\nJust a document.\n");
        assert!(parsed.front.is_empty(), "an unclosed block was parsed as settings");
        assert!(parsed.body.starts_with("---"), "the opening rule was eaten: {:?}", parsed.body);

        // A block that opens on a later line is not front matter either.
        let later = file("A title\n\n---\ntone: formal\n---\n");
        assert!(later.front.is_empty());
    }

    /// An unrecognised posture is not a posture. It falls through to the layer above rather
    /// than to any particular value — a permission granted by a spelling mistake is the one
    /// failure this field cannot have — and it is kept so the inspector can show it.
    #[test]
    fn an_unrecognised_permission_never_grants_anything() {
        let global = file("---\npermissions: ask\n---\n");
        let project = file("---\npermissions: whenever-you-like\n---\n");
        let resolved = resolve(&global, &project, &AgentRules::default(), "Reviewer");
        assert_eq!(resolved.permissions.value, Permissions::Ask);
        assert_eq!(resolved.permissions.from, Layer::Global, "a typo took the setting over");
        assert_eq!(
            resolved.extra.get("permissions").map(|s| s.value.as_str()),
            Some("whenever-you-like"),
            "the value nobody could read was thrown away instead of shown"
        );

        // Alone, it leaves the safe default in place.
        let only_typo = resolve(&project, &RuleFile::default(), &AgentRules::default(), "R");
        assert_eq!(only_typo.permissions.value, Permissions::Ask);
        assert_eq!(only_typo.permissions.from, Layer::Default);
    }

    #[test]
    fn permission_spellings_people_actually_type_are_understood() {
        assert_eq!(Permissions::parse("allow"), Some(Permissions::All));
        assert_eq!(Permissions::parse("never_ask"), Some(Permissions::All));
        assert_eq!(Permissions::parse("Never-Ask"), Some(Permissions::All));
        assert_eq!(Permissions::parse("read_only"), Some(Permissions::Reads));
        assert_eq!(Permissions::parse("ASK"), Some(Permissions::Ask));
        assert_eq!(Permissions::parse("never-allow"), None, "a near-miss was guessed at");
    }

    /// An absent file at any layer is an empty layer, not an error — and a file that exists
    /// and cannot be read is *not* silently the same thing.
    #[test]
    fn an_absent_file_is_not_an_error_and_an_unreadable_one_is_not_silent() {
        let dir = tempfile::tempdir().unwrap();

        let missing = RuleFile::read(&dir.path().join("nothing.md"));
        assert!(missing.is_empty() && missing.error.is_none() && missing.path.is_none());

        // A directory where a file should be: `read_to_string` fails with something other
        // than NotFound, and that must be reported rather than looking like an empty layer.
        let as_dir = dir.path().join("global.md");
        std::fs::create_dir(&as_dir).unwrap();
        let unreadable = RuleFile::read(&as_dir);
        assert!(unreadable.is_empty());
        assert!(unreadable.error.is_some(), "an unreadable rules file looked like an absent one");

        // And with nothing anywhere, resolution still produces a usable context.
        let resolved = resolve(
            &load_global(dir.path()),
            &load_project(dir.path()),
            &AgentRules::default(),
            "Worker",
        );
        assert!(resolved.sections.is_empty());
        assert!(resolved.system_context().contains("You are Worker"));
    }

    /// A project that already has an `AGENTS.md` or a `CLAUDE.md` is usable as-is, and the
    /// order is the documented one: Velm's own file wins, then the neutral convention, then
    /// Claude's, then the oldest.
    #[test]
    fn the_project_layer_falls_back_to_a_file_the_project_already_has() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        assert_eq!(project_rules_path(root), None, "an empty project claimed a rules file");

        std::fs::write(root.join(".cursorrules"), "Cursor says so.").unwrap();
        assert_eq!(project_rules_path(root), Some(root.join(".cursorrules")));
        assert_eq!(load_project(root).body, "Cursor says so.");

        std::fs::write(root.join("CLAUDE.md"), "Claude says so.").unwrap();
        assert_eq!(project_rules_path(root), Some(root.join("CLAUDE.md")));

        std::fs::write(root.join("AGENTS.md"), "Agents say so.").unwrap();
        assert_eq!(project_rules_path(root), Some(root.join("AGENTS.md")));

        std::fs::create_dir_all(root.join(".velm")).unwrap();
        std::fs::write(root.join(".velm").join("rules.md"), "---\ntone: house\n---\nVelm's own.")
            .unwrap();
        assert_eq!(project_rules_path(root), Some(root.join(".velm/rules.md")));

        let project = load_project(root);
        assert_eq!(project.front.tone.as_deref(), Some("house"));
        assert_eq!(project.body, "Velm's own.");
        assert!(project.path.is_some(), "the layer forgot which file it came from");
    }

    /// A `.cursorrules` is plain text with no front matter at all, which the same parser has
    /// to handle without inventing settings.
    #[test]
    fn a_plain_text_rules_file_is_all_body() {
        let parsed = file("Always run the tests.\nNever push to main.\n");
        assert!(parsed.front.is_empty());
        assert_eq!(parsed.body, "Always run the tests.\nNever push to main.");
    }

    /// A rules file is arbitrary user text and `panic = "abort"` means a slicing mistake
    /// here takes the application down (feedback 30). None of these may panic.
    #[test]
    fn parsing_never_panics_on_multibyte_or_truncated_input() {
        let cases = [
            "",
            "-",
            "--",
            "---",
            "---\n",
            "---\n---",
            "---\n---\n",
            ":",
            "---\n:\n---\n",
            "---\ntone: 日本語で答えて\n---\n本文です。",
            "---\nトーン: 丁寧\n---\n",
            "---\ntone: café\u{0}\n---\n🙂🙂🙂",
            "\u{feff}---\ntone: formal\n---\nbody",
        ];
        for case in cases {
            let parsed = file(case);
            let agent = AgentRules { text: case.to_owned(), ..AgentRules::default() };
            let resolved = resolve(&parsed, &parsed, &agent, "日本語のエージェント");
            let _ = resolved.system_context();
            let _ = resolved.rows();
        }

        // The BOM case is worth an assertion rather than only a survival check.
        let bom = file("\u{feff}---\ntone: formal\n---\nbody");
        assert_eq!(bom.front.tone.as_deref(), Some("formal"), "a BOM hid the front matter");
    }

    /// The inspector draws a row per setting whether or not it was set, so the user can see
    /// the whole surface rather than only the parts somebody filled in.
    #[test]
    fn every_setting_gets_a_row_even_when_nothing_set_it() {
        let resolved =
            resolve(&RuleFile::default(), &RuleFile::default(), &AgentRules::default(), "Worker");
        let rows = resolved.rows();
        assert_eq!(rows.len(), Field::ALL.len());
        for row in &rows {
            match row.field {
                Field::Permissions => {
                    assert!(row.value.is_some(), "the posture row was blank");
                    assert_eq!(row.from, Layer::Default);
                }
                _ => {
                    assert_eq!(row.value, None);
                    assert_eq!(row.from, Layer::Default);
                    assert!(!row.from.is_inherited());
                }
            }
        }
    }

    /// Unknown keys cascade under their own names, so a field added in a later build needs
    /// no migration — it is already being resolved.
    #[test]
    fn an_unknown_key_cascades_under_its_own_name() {
        let global = file("---\nverbosity: high\nmood: sunny\n---\n");
        let project = file("---\nverbosity: low\n---\n");
        let resolved = resolve(&global, &project, &AgentRules::default(), "Worker");
        let verbosity = Sourced { value: "low".to_owned(), from: Layer::Project };
        let mood = Sourced { value: "sunny".to_owned(), from: Layer::Global };
        assert_eq!(resolved.extra["verbosity"], verbosity);
        assert_eq!(resolved.extra["mood"], mood);
    }

    fn tools(may_spawn: bool, mcp: bool) -> BoardTools {
        BoardTools { command: crate::transport::AGENT_CLI.to_owned(), may_spawn, mcp }
    }

    /// ⚠ **The section exists only when the shim does, and that is the whole point of it.**
    ///
    /// An agent told about a command it has not got spends a turn discovering that, explains
    /// the failure to a user who never asked for the tool, and is *worse* than an agent told
    /// nothing. So the default call — every existing caller, and every test above — must be
    /// byte for byte what it was.
    #[test]
    fn the_board_verbs_are_described_only_when_the_shim_was_found() {
        let resolved =
            resolve(&RuleFile::default(), &RuleFile::default(), &AgentRules::default(), "Worker");

        let silent = resolved.system_context();
        assert!(!silent.contains(crate::transport::AGENT_CLI), "{silent}");
        assert!(!silent.contains("Acting on the board"), "{silent}");
        assert_eq!(silent, resolved.system_context_with(None), "the default is not the None case");

        let told = resolved.system_context_with(Some(&tools(false, false)));
        assert!(told.contains("## Acting on the board"), "{told}");
        assert!(told.contains(crate::transport::AGENT_CLI), "{told}");
        // The two verbs every agent has, and the one that makes a wrong recollection of the
        // arguments self-correcting.
        assert!(told.contains("send"), "{told}");
        assert!(told.contains("note read"), "{told}");
        assert!(told.contains("--help"), "the agent was not told where the real arguments are");
    }

    /// `spawn` is refused at Velm's IPC boundary for anything but an orchestrator or the meta
    /// agent, so describing it to a worker advertises a capability whose only outcome is a
    /// refusal — and an agent that has been told it may delegate will try to.
    #[test]
    fn only_an_agent_that_may_spawn_is_told_it_can() {
        let resolved =
            resolve(&RuleFile::default(), &RuleFile::default(), &AgentRules::default(), "Worker");

        let worker = resolved.system_context_with(Some(&tools(false, false)));
        assert!(!worker.contains("spawn"), "a worker was offered a verb Velm refuses: {worker}");

        let orchestrator = resolved.system_context_with(Some(&tools(true, false)));
        assert!(orchestrator.contains("spawn"), "{orchestrator}");

        // The MCP line is conditional for the same reason, one layer down: an agent with no
        // MCP server must not be told it has tools.
        assert!(!worker.contains("velm_send_message"), "{worker}");
        let with_mcp = resolved.system_context_with(Some(&tools(false, true)));
        assert!(with_mcp.contains("velm_send_message"), "{with_mcp}");
    }

    /// The user's own words still win.
    ///
    /// `system_context`'s ordering note is that later text wins with every model there is, so
    /// a project that says *"do not spawn helpers on this board"* has to come **after** the
    /// paragraph telling the agent it may. Putting the tools last would make Velm's own
    /// boilerplate outrank the instruction the user wrote by hand.
    #[test]
    fn the_rule_layers_still_come_after_the_tools() {
        let project = file("Never spawn helpers on this board.\n");
        let resolved =
            resolve(&RuleFile::default(), &project, &AgentRules::default(), "Orchestrator");
        let context = resolved.system_context_with(Some(&tools(true, true)));

        let tools_at = context.find("## Acting on the board").expect("the section is present");
        let rules_at = context.find("Never spawn helpers").expect("the project's words survive");
        assert!(tools_at < rules_at, "Velm's boilerplate was placed after the user's rules");
    }

    #[test]
    fn quoted_values_and_stray_apostrophes_both_survive() {
        let parsed = file("---\ntone: \"formal\"\noutput: 'concise'\nlanguage: don't fuss\n---\n");
        assert_eq!(parsed.front.tone.as_deref(), Some("formal"));
        assert_eq!(parsed.front.output.as_deref(), Some("concise"));
        assert_eq!(parsed.front.language.as_deref(), Some("don't fuss"));
    }

    // ----- writing a layer back ------------------------------------------------------

    /// The writer's whole contract: `parse(write(x)) == x`.
    ///
    /// Seven shapes in one table, each of which broke a draft of [`FrontMatter::to_block`]:
    /// the four named settings, an **unknown key** (kept by the parser, so a writer that
    /// dropped it would silently downgrade a file written for a later build), a body with no
    /// front matter at all, a body that opens with `---`, and a value that is itself quoted.
    ///
    /// The comparison is content — front matter and body — not the whole [`RuleFile`]:
    /// `path` and `error` describe this process's relationship with a file rather than the
    /// layer, and neither is written.
    ///
    /// **A/B**: dropping `front.extra` from the writer fails the fourth case with
    /// `left: {}` against `right: {"reviewers": "two"}`.
    #[test]
    fn writing_a_layer_and_reading_it_back_is_the_same_layer() {
        for text in [
            "",
            "Just prose, no settings at all.",
            "---\ntone: formal\noutput: concise\nlanguage: English\npermissions: reads\n---\n",
            "---\ntone: blunt\nreviewers: two\n---\n\nHouse style.\n",
            "---\nlanguage: Türkçe\n---\n\n# Heading\n\nWith a body.\n",
            // A body whose first line is a horizontal rule. Written with no front matter it
            // would be re-read as an opening fence, and everything to the next rule would
            // become settings — the one way this function can corrupt rather than reformat.
            "---\n---\n\n---\n\nA document that opens with a rule.\n",
            // The value is `"formal"`, quotation marks and all: `unquote` strips one pair,
            // so writing it bare would lose them.
            "---\ntone: \"\"formal\"\"\n---\n",
            // Two spaces. `parse` tests emptiness *after* unquoting, so this is a setting;
            // a writer that trimmed, or that skipped on `trim().is_empty()`, loses it.
            "---\ntone: \"  \"\n---\n",
        ] {
            let parsed = RuleFile::parse(text);
            let written = parsed.to_markdown();
            let back = RuleFile::parse(&written);
            assert_eq!(back.front, parsed.front, "front matter changed by\n{written}");
            assert_eq!(back.body, parsed.body, "body changed by\n{written}");
        }
    }

    /// A `permissions:` value this build cannot read is filed under `extra` beside a posture
    /// it *can*, and the writer has to put them back in an order that reproduces both.
    ///
    /// The named four are written first and the unknown keys after, so the unreadable value
    /// lands on the later line and is refused again on the way in — leaving the posture from
    /// the earlier line standing. Reversing the two blocks loses the posture.
    #[test]
    fn an_unreadable_permission_survives_a_round_trip() {
        let parsed = file("---\npermissions: sometimes\npermissions: ask\n---\n");
        assert_eq!(parsed.front.permissions, Some(Permissions::Ask));
        assert_eq!(parsed.front.extra.get("permissions").map(String::as_str), Some("sometimes"));

        let back = RuleFile::parse(&parsed.to_markdown());
        assert_eq!(back.front, parsed.front, "{}", parsed.to_markdown());
    }

    /// The common case is creating a file that was never there — that is the whole point of
    /// the row that offers to write a global rule set.
    #[test]
    fn saving_a_layer_that_has_no_file_yet_creates_it_directories_and_all() {
        let dir = tempfile::tempdir().unwrap();
        // Two levels that do not exist, which is exactly `<data-dir>/rules/global.md` on a
        // machine that has never had one.
        let path = global_rules_path(dir.path());
        assert!(!path.exists());

        let layer = file("---\ntone: warm\n---\n\nBe brief.\n");
        assert_eq!(layer.save(&path, &RuleFile::default()).unwrap(), Saved::Created);

        assert!(path.is_file(), "the file was not created");
        let read_back = RuleFile::read(&path);
        assert!(read_back.says_the_same_as(&layer), "{:?}", read_back);
        assert_eq!(read_back.path.as_deref(), Some(path.as_path()));
    }

    /// Saving the same text twice must not claim a second write, and must not report a
    /// conflict against itself.
    #[test]
    fn saving_what_the_file_already_says_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("global.md");
        let layer = file("---\ntone: warm\n---\n");
        assert_eq!(layer.save(&path, &RuleFile::default()).unwrap(), Saved::Created);
        assert_eq!(layer.save(&path, &layer).unwrap(), Saved::Unchanged);
    }

    /// Somebody edited the file in their own editor while the dialog was open. Nothing may
    /// be written, and the caller has to be handed what is actually there.
    ///
    /// **A/B**: with the freshness check removed this reports `Written` and the hand-edited
    /// `tone: blunt` is gone.
    #[test]
    fn a_file_that_changed_underneath_the_editor_is_not_clobbered() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rules.md");
        let opened_on = file("---\ntone: warm\n---\n");
        opened_on.save(&path, &RuleFile::default()).unwrap();

        // The user's other editor, mid-dialog.
        std::fs::write(&path, "---\ntone: blunt\n---\n\nHand written.\n").unwrap();

        let edited = file("---\ntone: formal\n---\n");
        match edited.save(&path, &opened_on).unwrap() {
            Saved::Conflict { on_disk } => {
                assert_eq!(on_disk.front.tone.as_deref(), Some("blunt"));
                assert_eq!(on_disk.body, "Hand written.");
            }
            other => panic!("expected a conflict, got {other:?}"),
        }
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "---\ntone: blunt\n---\n\nHand written.\n",
            "the hand-written file was overwritten"
        );
    }

    /// A reformat is not a conflict. The same four settings in a different order, or with a
    /// trailing newline somebody's editor added, *is* the layer the dialog was opened on —
    /// refusing there would make Save fail for no reason a user could see.
    #[test]
    fn a_reformat_underneath_the_editor_is_not_a_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rules.md");
        let opened_on = file("---\ntone: warm\noutput: concise\n---\n\nBody.\n");
        opened_on.save(&path, &RuleFile::default()).unwrap();
        std::fs::write(&path, "---\noutput:   concise\ntone: warm\n---\n\nBody.\n\n\n").unwrap();

        let edited = file("---\ntone: blunt\n---\n");
        assert_eq!(edited.save(&path, &opened_on).unwrap(), Saved::Written);
        assert_eq!(RuleFile::read(&path).front.tone.as_deref(), Some("blunt"));
    }

    /// Velm writes one of the four project spellings and reads any of them. A project with
    /// somebody else's `CLAUDE.md` must not have that file rewritten by this editor.
    #[test]
    fn a_project_layer_is_written_to_velm_s_own_file_and_never_to_someone_else_s() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("CLAUDE.md"), "Somebody else's instructions.\n").unwrap();
        assert_eq!(project_rules_path(dir.path()), Some(dir.path().join("CLAUDE.md")));

        let target = project_rules_target(dir.path());
        assert_eq!(target, dir.path().join(".velm").join("rules.md"));
        file("---\ntone: blunt\n---\n").save(&target, &RuleFile::default()).unwrap();

        assert_eq!(
            std::fs::read_to_string(dir.path().join("CLAUDE.md")).unwrap(),
            "Somebody else's instructions.\n",
            "the project's own file was rewritten"
        );
        // And it now shadows it, which is `PROJECT_RULE_FILES`' documented precedence.
        assert_eq!(project_rules_path(dir.path()), Some(target));
    }
}
