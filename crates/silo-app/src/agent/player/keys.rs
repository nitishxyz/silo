//! The player's keys, as data.

/// Seconds ←/→ and the skip buttons move.
pub const SKIP: f64 = 5.0;
/// ↑/↓ volume step (0–1).
pub const VOLUME_STEP: f32 = 0.1;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Key {
    Toggle,
    Skip(f64),
    Volume(f32),
    Mute,
    Loop,
    Fill,
    /// The session's previous (`-1`) or next (`1`) image or video.
    Step(i32),
    /// Leaves fill, else closes the viewer.
    Escape,
}

/// What a key does while a video shows; chords go on (⌘C, ⌘S, ⌘W…).
#[must_use]
pub fn map(key: &str, platform: bool, control: bool) -> Option<Key> {
    if platform || control {
        return None;
    }
    Some(match key {
        "space" | "k" => Key::Toggle,
        "left" => Key::Skip(-SKIP),
        "right" => Key::Skip(SKIP),
        "up" => Key::Volume(VOLUME_STEP),
        "down" => Key::Volume(-VOLUME_STEP),
        "m" => Key::Mute,
        "l" => Key::Loop,
        "f" => Key::Fill,
        "[" => Key::Step(-1),
        "]" => Key::Step(1),
        "escape" => Key::Escape,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_map_to_actions() {
        assert_eq!(map("space", false, false), Some(Key::Toggle));
        assert_eq!(map("k", false, false), Some(Key::Toggle));
        assert_eq!(map("left", false, false), Some(Key::Skip(-5.0)));
        assert_eq!(map("right", false, false), Some(Key::Skip(5.0)));
        assert_eq!(map("up", false, false), Some(Key::Volume(0.1)));
        assert_eq!(map("m", false, false), Some(Key::Mute));
        assert_eq!(map("l", false, false), Some(Key::Loop));
        assert_eq!(map("f", false, false), Some(Key::Fill));
        assert_eq!((map("[", false, false), map("]", false, false)), (Some(Key::Step(-1)), Some(Key::Step(1))));
        assert_eq!(map("escape", false, false), Some(Key::Escape));
        assert_eq!(map("x", false, false), None);
    }

    #[test]
    fn chords_go_on() {
        assert_eq!(map("c", true, false), None);
        assert_eq!(map("space", false, true), None);
        assert_eq!(map("left", true, false), None);
    }
}
