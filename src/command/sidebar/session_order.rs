//! Fork: pinned session order for the session-grouped sidebar.
//!
//! The pins live in a file kept by an outside tool (`sidebar.session_order_file`),
//! one `top<TAB>name` or `bottom<TAB>name` per line, top to bottom. Sessions
//! pinned to the top open the list in file order, sessions pinned to the bottom
//! close it in file order, and the rest keep their alphabetical place between.

use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Zone {
    Top,
    Mid,
    Bottom,
}

/// Session name to its zone and line number in the order file.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct SessionOrder {
    pins: HashMap<String, (Zone, usize)>,
}

impl SessionOrder {
    /// Pins from the file's text. Unknown zones and blank lines are skipped;
    /// a name listed twice keeps its last line.
    pub fn parse(text: &str) -> Self {
        let mut pins = HashMap::new();
        for (line_no, line) in text.lines().enumerate() {
            let Some((zone, name)) = line.split_once('\t') else {
                continue;
            };
            let zone = match zone {
                "top" => Zone::Top,
                "bottom" => Zone::Bottom,
                _ => continue,
            };
            pins.insert(name.to_string(), (zone, line_no));
        }
        Self { pins }
    }

    /// Pins from `path`. A missing or unreadable file means no pins.
    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .map(|text| Self::parse(&text))
            .unwrap_or_default()
    }

    /// Sort key of a session: its zone, then its line in that zone. Unpinned
    /// sessions share one rank in the middle and fall through to the caller's
    /// next key.
    pub fn rank(&self, session: &str) -> (Zone, usize) {
        self.pins.get(session).copied().unwrap_or((Zone::Mid, 0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_keeps_zones_and_file_order() {
        let order = SessionOrder::parse("bottom\tz\ntop\tb\ntop\ta\n\nmid\tx\nbroken\n");
        assert_eq!(order.rank("b"), (Zone::Top, 1));
        assert_eq!(order.rank("a"), (Zone::Top, 2));
        assert_eq!(order.rank("z"), (Zone::Bottom, 0));
        assert_eq!(order.rank("x"), (Zone::Mid, 0));
        assert_eq!(order.rank("broken"), (Zone::Mid, 0));
        assert!(order.rank("b") < order.rank("a"));
        assert!(order.rank("a") < order.rank("other"));
        assert!(order.rank("other") < order.rank("z"));
    }

    #[test]
    fn names_keep_spaces_and_the_last_line_wins() {
        let order = SessionOrder::parse("top\t__ review __\nbottom\t__ review __\n");
        assert_eq!(order.rank("__ review __"), (Zone::Bottom, 1));
    }

    #[test]
    fn a_missing_file_has_no_pins() {
        let order = SessionOrder::load(Path::new("/nonexistent/workmux/session-order"));
        assert_eq!(order, SessionOrder::default());
    }
}
