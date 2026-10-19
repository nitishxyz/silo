//! Memorable default session names: `first-second`, lowercase and
//! hyphenated ("mellow-otter", "jhatpat-badal").

const FIRST: &[&str] = &[
    "mellow", "fuzzy", "jolly", "breezy", "cosy", "dizzy", "gentle", "happy", "lazy", "lucky", "merry", "nimble", "peppy", "quiet", "rusty",
    "snappy", "sunny", "tiny", "witty", "zesty", "bouncy", "cheeky", "dapper", "fizzy", "giddy", "humble", "jumpy", "plucky", "spry", "toasty",
    "wobbly", "zippy", "chatpata", "jhatpat", "masti", "tez", "chulbul", "nakhra", "dhoom", "jugaadu", "khatta", "meetha", "tadka", "rangeela",
    "chamak", "fataft", "gol", "halka", "mast", "shaant", "jhakkas", "bindaas", "chillax", "nifty", "brisk", "crisp", "dandy", "keen",
];

const SECOND: &[&str] = &[
    "otter", "lantern", "badal", "pakora", "samosa", "panda", "kettle", "pebble", "comet", "mango", "noodle", "pickle", "rocket", "teacup", "walrus",
    "biscuit", "cactus", "dhimkana", "falooda", "jalebi", "kulfi", "ladoo", "chai", "tota", "bandar", "titli", "machli", "chidiya", "patang",
    "bulbul", "dhaba", "rickshaw", "tamasha", "barfi", "gulab", "chandni", "sitara", "toofan", "jhoola", "lassi", "parrot", "badger", "maple",
    "acorn", "harbor", "meadow", "pretzel", "quokka", "sparrow", "tortoise", "waffle", "yak", "zebra", "puffin", "fennel",
];

/// A random name; `taken` rejects names already in use.
pub fn generate(taken: impl Fn(&str) -> bool) -> String {
    for _ in 0..64 {
        let a = FIRST[rand::random_range(0..FIRST.len())];
        let b = SECOND[rand::random_range(0..SECOND.len())];
        let name = format!("{a}-{b}");
        if !taken(&name) {
            return name;
        }
    }
    // Practically unreachable (3000+ combinations); add a suffix.
    loop {
        let name =
            format!("{}-{}-{}", FIRST[rand::random_range(0..FIRST.len())], SECOND[rand::random_range(0..SECOND.len())], rand::random_range(2..1000));
        if !taken(&name) {
            return name;
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn names_are_two_lowercase_words_and_avoid_collisions() {
        let name = super::generate(|_| false);
        let parts: Vec<&str> = name.split('-').collect();
        assert_eq!(parts.len(), 2, "{name}");
        assert!(name.chars().all(|c| c.is_ascii_lowercase() || c == '-'), "{name}");
        let first = name.clone();
        let other = super::generate(|n| n == first);
        assert_ne!(other, first);
    }
}
