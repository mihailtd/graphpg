//! English singulars and plurals of the names graphpg derives from database names: a table
//! `people` is a type `Person`, `episodes_of_care` an `EpisodeOfCare`, and a `Person`'s rows are
//! `People`. Only the head noun changes: the last word, or the word before `of`.

/// Plurals the suffix rules get wrong, and their singulars. Also matched as the end of a word
/// (`salespeople`, `grandchildren`, `bookshelves`).
const IRREGULAR: &[(&str, &str)] = &[
    ("people", "person"),
    ("children", "child"),
    ("women", "woman"),
    ("feet", "foot"),
    ("teeth", "tooth"),
    ("geese", "goose"),
    ("oxen", "ox"),
    ("criteria", "criterion"),
    ("phenomena", "phenomenon"),
    ("schemata", "schema"),
    ("alumni", "alumnus"),
    ("cacti", "cactus"),
    ("fungi", "fungus"),
    ("nuclei", "nucleus"),
    ("radii", "radius"),
    ("stimuli", "stimulus"),
    ("syllabi", "syllabus"),
    ("indices", "index"),
    ("matrices", "matrix"),
    ("vertices", "vertex"),
    ("appendices", "appendix"),
    ("quizzes", "quiz"),
    ("heroes", "hero"),
    ("potatoes", "potato"),
    ("tomatoes", "tomato"),
    ("echoes", "echo"),
    ("vetoes", "veto"),
    ("torpedoes", "torpedo"),
    ("embargoes", "embargo"),
    ("wives", "wife"),
    ("knives", "knife"),
    ("leaves", "leaf"),
    ("halves", "half"),
    ("selves", "self"),
    ("shelves", "shelf"),
    ("wolves", "wolf"),
    ("calves", "calf"),
    ("thieves", "thief"),
    ("loaves", "loaf"),
    ("scarves", "scarf"),
    ("movies", "movie"),
    ("cookies", "cookie"),
    ("calories", "calorie"),
    ("zombies", "zombie"),
    ("rookies", "rookie"),
    ("smoothies", "smoothie"),
    ("selfies", "selfie"),
    ("goalies", "goalie"),
    ("brownies", "brownie"),
    ("menus", "menu"),
    ("gurus", "guru"),
    ("emus", "emu"),
    ("haikus", "haiku"),
    ("crises", "crisis"),
    ("theses", "thesis"),
    ("analyses", "analysis"),
    ("diagnoses", "diagnosis"),
    ("prognoses", "prognosis"),
    ("synopses", "synopsis"),
    ("ellipses", "ellipsis"),
    ("aliases", "alias"),
    ("biases", "bias"),
    ("canvases", "canvas"),
    ("atlases", "atlas"),
    ("gases", "gas"),
];

/// Irregular plurals that only match a whole word (`specimen` and `olives` are not of these).
const IRREGULAR_WORDS: &[(&str, &str)] = &[
    ("men", "man"),
    ("mice", "mouse"),
    ("lives", "life"),
    ("elves", "elf"),
];

/// Words that are their own plural, or have none.
const INVARIANT: &[&str] = &[
    "series",
    "species",
    "news",
    "data",
    "metadata",
    "information",
    "equipment",
    "software",
    "hardware",
    "feedback",
    "sheep",
    "deer",
    "fish",
    "moose",
    "aircraft",
    "staff",
    "personnel",
    "media",
    "evidence",
    "research",
    "music",
    "money",
    "police",
    "offspring",
    "headquarters",
    "means",
    "corps",
    "physics",
    "mathematics",
    "economics",
    "politics",
    "ethics",
    "logistics",
    "analytics",
    "athletics",
    "kudos",
    "chaos",
    "cosmos",
    "ethos",
    "pathos",
];

/// Singular words ending in `s` that the rules would take for plurals.
const SINGULAR_IN_S: &[&str] = &[
    "alias", "atlas", "bias", "canvas", "gas", "lens", "tennis", "iris",
];

/// Plurals ending in `uses` after a consonant that drop only the `s` (but `statuses`, `buses`).
const USE_WORDS: &[&str] = &[
    "uses", "excuses", "abuses", "fuses", "refuses", "accuses", "ruses", "muses", "recluses",
    "misuses", "reuses",
];

fn is_vowel(c: u8) -> bool {
    matches!(c, b'a' | b'e' | b'i' | b'o' | b'u')
}

/// `word` with its last `cut` bytes replaced by `add`, keeping the case of a replaced first
/// letter.
fn replace_end(word: &str, cut: usize, add: &str) -> String {
    let stem = &word[..word.len() - cut];
    if stem.is_empty() && word.starts_with(|c: char| c.is_ascii_uppercase()) {
        let mut chars = add.chars();
        return chars
            .next()
            .map(|c| c.to_ascii_uppercase().to_string() + chars.as_str())
            .unwrap_or_default();
    }
    format!("{stem}{add}")
}

/// Singular of one word: `categories` -> `category`, `addresses` -> `address`,
/// `people` -> `person`. Words not plural are returned as they are.
pub fn singular_word(word: &str) -> String {
    let lower = word.to_ascii_lowercase();
    let w = lower.as_bytes();
    let n = w.len();
    if n < 3 || INVARIANT.contains(&lower.as_str()) || SINGULAR_IN_S.contains(&lower.as_str()) {
        return word.to_string();
    }
    if let Some((plural, single)) = IRREGULAR_WORDS.iter().find(|(p, _)| lower == *p) {
        return replace_end(word, plural.len(), single);
    }
    if let Some((plural, single)) = IRREGULAR.iter().find(|(p, _)| lower.ends_with(p)) {
        return replace_end(word, plural.len(), single);
    }
    if !lower.ends_with('s')
        || lower.ends_with("ss")
        || lower.ends_with("us")
        || lower.ends_with("sis")
        || lower.ends_with("xis")
    {
        return word.to_string();
    }
    let drop_es = if lower.ends_with("ies") {
        // `ties`, `pies`: the `ie` is the word's own
        return if n <= 4 {
            replace_end(word, 1, "")
        } else {
            replace_end(word, 3, "y")
        };
    } else if lower.ends_with("sses")
        || lower.ends_with("xes")
        || lower.ends_with("zzes")
        || lower.ends_with("shes")
    {
        true
    } else if lower.ends_with("ches") {
        // `caches`, `niches`, `headaches` keep their `e`; `beaches`, `matches`, `branches` not
        n < 6 || !(is_vowel(w[n - 5]) && !is_vowel(w[n - 6]))
    } else if lower.ends_with("uses") {
        // `houses`, `causes`, `excuses` keep their `e`; `statuses`, `buses`, `viruses` not
        n >= 5 && !is_vowel(w[n - 5]) && !USE_WORDS.contains(&lower.as_str())
    } else {
        false
    };
    replace_end(word, if drop_es { 2 } else { 1 }, "")
}

/// Plural of one word: `category` -> `categories`, `person` -> `people`.
pub fn plural_word(word: &str) -> String {
    let lower = word.to_ascii_lowercase();
    if lower.is_empty() || INVARIANT.contains(&lower.as_str()) {
        return word.to_string();
    }
    if let Some((plural, single)) = IRREGULAR_WORDS.iter().find(|(_, s)| lower == *s) {
        return replace_end(word, single.len(), plural);
    }
    // A short singular ends too many words (`box` is not an `ox`): matched whole
    if let Some((plural, single)) = IRREGULAR
        .iter()
        .find(|(_, s)| lower == *s || (s.len() >= 5 && lower.ends_with(s)))
    {
        return replace_end(word, single.len(), plural);
    }
    let w = lower.as_bytes();
    let n = w.len();
    if lower.ends_with("sis") || lower.ends_with("xis") {
        return replace_end(word, 2, "es");
    }
    if n >= 2 && w[n - 1] == b'y' && !is_vowel(w[n - 2]) {
        return replace_end(word, 1, "ies");
    }
    if ["s", "x", "z", "sh"].iter().any(|s| lower.ends_with(s))
        || (lower.ends_with("ch") && !lower.ends_with("ache"))
    {
        return format!("{word}es");
    }
    format!("{word}s")
}

/// Singular of a snake_case name: `order_items` -> `order_item`, `episodes_of_care` ->
/// `episode_of_care`, `people` -> `person`.
pub fn singular(name: &str) -> String {
    let mut words: Vec<String> = name.split('_').map(str::to_string).collect();
    let head = head_word(&words, |w| w.eq_ignore_ascii_case("of"), singular_word);
    words[head] = singular_word(&words[head]);
    words.join("_")
}

/// Plural of a PascalCase name: `OrderItem` -> `OrderItems`, `EpisodeOfCare` ->
/// `EpisodesOfCare`, `Person` -> `People`.
pub fn plural(name: &str) -> String {
    let mut words: Vec<String> = Vec::new();
    for (i, c) in name.char_indices() {
        if c.is_ascii_uppercase() && i > 0 || words.is_empty() {
            words.push(String::new());
        }
        words.last_mut().unwrap().push(c);
    }
    if words.is_empty() {
        return String::new();
    }
    let head = head_word(&words, |w| w == "Of", plural_word);
    words[head] = plural_word(&words[head]);
    words.concat()
}

/// The word to inflect: the one before `of` when inflecting it changes it (`episodes_of_care`),
/// else the last.
fn head_word(words: &[String], is_of: impl Fn(&str) -> bool, inflect: fn(&str) -> String) -> usize {
    words
        .iter()
        .position(|w| is_of(w))
        .filter(|&i| i > 0 && inflect(&words[i - 1]) != words[i - 1])
        .map(|i| i - 1)
        .unwrap_or(words.len() - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn singulars() {
        for (plural, single) in [
            ("users", "user"),
            ("categories", "category"),
            ("ties", "tie"),
            ("addresses", "address"),
            ("boxes", "box"),
            ("taxes", "tax"),
            ("batches", "batch"),
            ("branches", "branch"),
            ("beaches", "beach"),
            ("caches", "cache"),
            ("dishes", "dish"),
            ("buzzes", "buzz"),
            ("sizes", "size"),
            ("statuses", "status"),
            ("buses", "bus"),
            ("houses", "house"),
            ("causes", "cause"),
            ("excuses", "excuse"),
            ("cases", "case"),
            ("databases", "database"),
            ("responses", "response"),
            ("archives", "archive"),
            ("olives", "olive"),
            ("lives", "life"),
            ("shoes", "shoe"),
            ("heroes", "hero"),
            ("people", "person"),
            ("salespeople", "salesperson"),
            ("children", "child"),
            ("men", "man"),
            ("women", "woman"),
            ("specimen", "specimen"),
            ("criteria", "criterion"),
            ("indices", "index"),
            ("analyses", "analysis"),
            ("diagnoses", "diagnosis"),
            ("aliases", "alias"),
            ("movies", "movie"),
            ("menus", "menu"),
            ("status", "status"),
            ("analysis", "analysis"),
            ("address", "address"),
            ("alias", "alias"),
            ("series", "series"),
            ("data", "data"),
            ("Users", "User"),
            ("People", "Person"),
        ] {
            assert_eq!(singular_word(plural), single, "{plural}");
        }
    }

    #[test]
    fn plurals() {
        for (single, plural) in [
            ("user", "users"),
            ("category", "categories"),
            ("survey", "surveys"),
            ("address", "addresses"),
            ("box", "boxes"),
            ("batch", "batches"),
            ("cache", "caches"),
            ("dish", "dishes"),
            ("status", "statuses"),
            ("bus", "buses"),
            ("syllabus", "syllabi"),
            ("analysis", "analyses"),
            ("person", "people"),
            ("Person", "People"),
            ("man", "men"),
            ("woman", "women"),
            ("child", "children"),
            ("index", "indices"),
            ("self", "selves"),
            ("leaf", "leaves"),
            ("roof", "roofs"),
            ("hero", "heroes"),
            ("photo", "photos"),
            ("series", "series"),
            ("alias", "aliases"),
        ] {
            assert_eq!(plural_word(single), plural, "{single}");
        }
    }

    #[test]
    fn names() {
        assert_eq!(singular("order_items"), "order_item");
        assert_eq!(singular("episodes_of_care"), "episode_of_care");
        assert_eq!(singular("points_of_interest"), "point_of_interest");
        assert_eq!(singular("terms_of_services"), "term_of_services");
        assert_eq!(singular("series_of_data"), "series_of_data");
        assert_eq!(singular("sign_in_attempts"), "sign_in_attempt");
        assert_eq!(singular("people"), "person");
        assert_eq!(singular("OrderItems"), "OrderItem");
        assert_eq!(plural("OrderItem"), "OrderItems");
        assert_eq!(plural("EpisodeOfCare"), "EpisodesOfCare");
        assert_eq!(plural("Person"), "People");
        assert_eq!(plural("Category"), "Categories");
        assert_eq!(plural("UserSummary"), "UserSummaries");
    }
}
