//! What the operating system is doing about appearance, read live.
//!
//! `docs/05-design-language.md` §3a is specific: the translucent material must honour
//! **Reduce Transparency** and high contrast, and it must *"detect it at runtime and
//! react live — do not read it once at launch"*. Dark mode has the same requirement
//! for `ThemePreference::System`.
//!
//! # How each of the three is read
//!
//! - **Dark mode** comes from `winit`, which already tracks the window's theme and
//!   reports a change as an event. Nothing here is needed for it.
//! - **Reduce Transparency** and **Increase Contrast** have no `winit` equivalent.
//!   The honest options were a macOS binding (`objc2-app-kit`, for
//!   `NSWorkspace.accessibilityDisplayShouldReduceTransparency`) or reading the
//!   preference domain the setting is stored in. This takes the second: it is one
//!   process, no new dependency, and it is *correct on the machine the project runs
//!   on*, which a binding compiled against a framework version is not automatically.
//!
//! # Why it is a thread and not a poll on the frame path
//!
//! Spawning `defaults` takes single-digit milliseconds. Doing that on the frame path,
//! even once every two seconds, drops a frame every two seconds — in an application
//! whose entire justification is that Miro stutters. So the poll runs on its own
//! thread and publishes into an atomic the frame reads for free.
//!
//! # Honest gap
//!
//! Windows has the same two settings (`Transparency effects`, high-contrast themes)
//! in the registry under `HKCU\Software\Microsoft\Windows\CurrentVersion\Themes\
//! Personalize\EnableTransparency`. This file does not read them: the project has no
//! Windows machine to check against yet, and a setting read wrongly is worse than one
//! defaulted honestly. On every platform but macOS the readings are the permissive
//! defaults and the in-app override in Preferences is the way to turn glass off.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

/// How often the accessibility settings are re-read. Slow enough to be free, fast
/// enough that flipping the switch in System Settings visibly changes the app while
/// the user is still looking at it.
const POLL_INTERVAL: Duration = Duration::from_secs(2);

const REDUCE_TRANSPARENCY: u8 = 1 << 0;
const INCREASE_CONTRAST: u8 = 1 << 1;

/// A live reading of the OS accessibility settings.
///
/// Cheap to clone and cheap to read: one relaxed atomic load per frame.
#[derive(Debug, Clone)]
pub struct Appearance {
    flags: Arc<AtomicU8>,
}

impl Appearance {
    /// Takes a first reading and starts watching for changes.
    ///
    /// The first reading is taken on the calling thread so the very first frame is
    /// already correct — a toolbar that is translucent for two seconds and then turns
    /// opaque is exactly the flicker a user who asked for Reduce Transparency does not
    /// want to see.
    pub fn watch() -> Self {
        let flags = Arc::new(AtomicU8::new(read()));
        let watcher = Arc::clone(&flags);
        // Detached deliberately: it owns nothing but an atomic, it has no shutdown to
        // coordinate, and the process exiting is its shutdown.
        std::thread::Builder::new()
            .name("vellum-appearance".to_owned())
            .spawn(move || {
                loop {
                    std::thread::sleep(POLL_INTERVAL);
                    // The last strong reference is the app's. When it goes, so does
                    // the app, and the thread can stop asking.
                    if Arc::strong_count(&watcher) == 1 {
                        return;
                    }
                    watcher.store(read(), Ordering::Relaxed);
                }
            })
            .map_or_else(
                |error| log::warn!("appearance watcher did not start: {error}"),
                |_| (),
            );
        Self { flags }
    }

    /// A reading with nothing switched on, for tests and for the headless paths.
    pub fn inert() -> Self {
        Self { flags: Arc::new(AtomicU8::new(0)) }
    }

    pub fn reduce_transparency(&self) -> bool {
        self.flags.load(Ordering::Relaxed) & REDUCE_TRANSPARENCY != 0
    }

    pub fn increase_contrast(&self) -> bool {
        self.flags.load(Ordering::Relaxed) & INCREASE_CONTRAST != 0
    }

    /// The reading in the shape `vellum-ui` wants.
    ///
    /// **The OS dark setting is not read, and that is the user's instruction:** *"i
    /// want only light mode"*. `dark` is reported as false whatever the machine is
    /// set to, so nothing downstream can pick the dark cut — `ThemePreference::resolve`
    /// would refuse it anyway, and having two places that both have to be right is
    /// how a setting comes back by accident.
    ///
    /// Reduce Transparency and Increase Contrast are a **different** question and are
    /// still honoured live, per `docs/05-design-language.md` §3a.
    pub fn system(&self) -> vellum_ui::SystemAppearance {
        vellum_ui::SystemAppearance {
            dark: false,
            reduce_transparency: self.reduce_transparency(),
            increase_contrast: self.increase_contrast(),
        }
    }
}

#[cfg(target_os = "macos")]
fn read() -> u8 {
    let mut flags = 0;
    if defaults_bool("com.apple.universalaccess", "reduceTransparency") {
        flags |= REDUCE_TRANSPARENCY;
    }
    if defaults_bool("com.apple.universalaccess", "increaseContrast") {
        flags |= INCREASE_CONTRAST;
    }
    flags
}

#[cfg(not(target_os = "macos"))]
fn read() -> u8 {
    0
}

/// Reads one boolean out of a macOS preference domain.
///
/// A missing key is `false`, not an error: the keys only exist once the setting has
/// been touched, so on a machine where the user has never opened the accessibility
/// pane `defaults` exits non-zero and that means "off".
#[cfg(target_os = "macos")]
fn defaults_bool(domain: &str, key: &str) -> bool {
    let output = std::process::Command::new("defaults")
        .args(["read", domain, key])
        .output();
    match output {
        Ok(output) if output.status.success() => {
            matches!(String::from_utf8_lossy(&output.stdout).trim(), "1" | "true" | "YES")
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_inert_reading_switches_nothing_on() {
        let appearance = Appearance::inert();
        assert!(!appearance.reduce_transparency());
        assert!(!appearance.increase_contrast());
    }

    /// *"i want only light mode"*. The OS dark setting is not consulted at all, and
    /// the accessibility settings beside it still are.
    #[test]
    fn the_system_reading_never_reports_dark_and_still_reports_the_rest() {
        let appearance = Appearance::inert();
        assert!(!appearance.system().dark);
        assert!(!appearance.system().reduce_transparency);
        assert!(!appearance.system().increase_contrast);
    }

    /// Reading the real machine must not panic or block for long, whatever the user
    /// has set — including on a machine with no `defaults` at all.
    #[test]
    fn reading_the_real_settings_returns() {
        let flags = read();
        assert_eq!(flags & !(REDUCE_TRANSPARENCY | INCREASE_CONTRAST), 0);
    }

    /// The watcher must survive its own thread failing to start, and must take a
    /// reading before the first frame rather than after the first poll interval.
    #[test]
    fn watching_takes_a_reading_immediately() {
        let appearance = Appearance::watch();
        assert_eq!(
            appearance.reduce_transparency(),
            read() & REDUCE_TRANSPARENCY != 0
        );
    }
}
