//! Vellum's board editor: the window, the input, and the wiring that turns a
//! document into pixels.
//!
//! Every other crate in the workspace is a component with no opinion about the
//! application: `vellum-doc` holds a board, `vellum-scene` indexes boxes,
//! `vellum-render` draws primitives, `vellum-store` persists bytes. This crate is
//! where they meet, and the dependency arrows all point *into* it — nothing in the
//! workspace depends on `vellum-app`.
//!
//! ```text
//!   winit ─► input ─► Camera ─┐
//!                             ├─► draw ─► DrawList ─► vellum-render ─► surface
//!   Board ─► project ─► Scene ┘                 ▲
//!     │                                         │
//!     └─► vellum-store (autosave)      text · assets · connector
//! ```
//!
//! # Why this is a library and not only a binary
//!
//! The two things most likely to be wrong in a canvas app are *how a gesture moves
//! the camera* and *where an item lands on screen*, and neither needs a window to
//! check. Splitting the library out means both are ordinary unit tests, and it means
//! `tests/real_board.rs` can import 596 real Miro widgets and assert on the result
//! without a GPU, a display or a hand on a trackpad.
//!
//! # A frame
//!
//! 1. `input::Input::tick` advances any inertial glide.
//! 2. `project::Projection` is rebuilt if the document changed — never per frame.
//! 3. `draw::Painter::paint` culls through the R-tree, fills the glyph atlas and the
//!    texture cache, and emits one [`vellum_render::DrawList`].
//! 4. `surface::Surface::present` uploads it and draws it in one render pass.
//!
//! Nothing in that list iterates the document, which is the property
//! `docs/01-architecture.md` §3 rests the whole performance argument on.

pub mod actions;
pub mod agent;
pub mod agent_view;
pub mod app;
pub mod appearance;
pub mod assets;
pub mod bench;
pub mod browser;
pub mod capture;
pub mod chart;
pub mod filetree;
pub mod chrome_pass;
pub mod compose;
pub mod connector;
pub mod decode;
pub mod draw;
pub mod edit;
pub mod editor;
pub mod export;
pub mod flight;
pub mod handle;
pub mod hud;
pub mod input;
pub mod inspect;
pub mod kanban;
pub mod library;
pub mod links;
pub mod menubar;
pub mod mesh;
pub mod mindmap;
pub mod note;
pub mod options;
pub mod project;
pub mod session;
pub mod shapes;
pub mod shell;
pub mod snap;
pub mod surface;
pub mod table;
pub mod text;
pub mod theme;
pub mod words;

pub use app::Vellum;
pub use appearance::Appearance;
pub use chrome_pass::ChromePass;
pub use draw::{DrawContext, PaintStats, Painter};
pub use editor::Editor;
pub use input::{Input, InputConfig, Intent, Tool};
pub use library::Library;
pub use options::{Command, HELP, Options, parse_args};
pub use project::{Projected, Projection};
pub use session::{Parked, Session};
pub use shell::Shell;
pub use text::TextCache;
pub use theme::Theme;
