//! The evidence ladder, said beside every level the documents carry.
//!
//! A join and a decomposition row carry their evidence as one upper-case word,
//! and to a reader of prose `TEMPORAL` and `IDENTITY` are two words of the same
//! kind. They are not: one is a guess that happens to fit, the other a key the
//! upstream event carried and the downstream one read back. So wherever a
//! document says `evidence`, the answer says beside it what that level is, where
//! it stands on the ladder, and what it does not establish.
//!
//! The match below is exhaustive over `omn::join::Evidence`, so a level added to
//! the engine does not compile here until someone has said what it means.

use omn::join::Evidence;
use serde_json::{json, Map, Value};

/// Every level, weakest first.
pub const LADDER: [Evidence; 4] = [
    Evidence::Temporal,
    Evidence::Wakeup,
    Evidence::Key,
    Evidence::Identity,
];

/// The key added beside an `evidence` field. Never present in a document omn
/// writes; the server refuses to overwrite one if it ever is.
pub const ANNOTATION: &str = "evidence_means";

/// What a level establishes.
pub fn establishes(level: Evidence) -> &'static str {
    match level {
        Evidence::Temporal => {
            "the two sides overlapped in time, and nothing else ties them: a hypothesis, nothing more"
        }
        Evidence::Wakeup => {
            "a wakeup chain links them: the one kind of tie that crosses a block"
        }
        Evidence::Key => "both sides carry the same typed key (a task, a device, a request pointer)",
        Evidence::Identity => {
            "the downstream event carries an identity the upstream one propagated and it read back"
        }
    }
}

/// What a level does not establish, which is the half a summary drops.
pub fn does_not_establish(level: Evidence) -> &'static str {
    match level {
        Evidence::Temporal => {
            "that one side caused, served or contains the other: anything else that ran in the \
             same interval fits the same way"
        }
        Evidence::Wakeup => {
            "what the waker was doing on behalf of the woken task beyond releasing it"
        }
        Evidence::Key => {
            "that the two events belong to the same unit of work: a key is shared by every \
             event of that task or device, and a recycled key by events of different units"
        }
        Evidence::Identity => {
            "anything outside the propagated identity: the row is exact about whose it is, not \
             about why it took what it took"
        }
    }
}

/// Position on the ladder, 1 being the weakest.
pub fn rank(level: Evidence) -> usize {
    LADDER
        .iter()
        .position(|candidate| *candidate == level)
        .map_or(0, |index| index + 1)
}

/// One level, as the answer states it.
pub fn describe(level: Evidence) -> Value {
    json!({
        "level": level.as_str(),
        "rank": format!("{} of {}, weakest first", rank(level), LADDER.len()),
        "establishes": establishes(level),
        "does_not_establish": does_not_establish(level),
    })
}

/// The whole ladder, weakest first.
pub fn ladder() -> Value {
    Value::Array(LADDER.iter().copied().map(describe).collect())
}

/// Puts the meaning beside every `evidence` string in `value`, recursively.
///
/// Returns what could not be annotated: a level this server does not know is
/// said to be unknown rather than guessed at, and a document that already has
/// the annotation key is left alone and reported.
pub fn annotate(value: &mut Value) -> Vec<String> {
    let mut problems = Vec::new();
    walk(value, &mut problems);
    problems
}

fn walk(value: &mut Value, problems: &mut Vec<String>) {
    match value {
        Value::Object(object) => {
            annotate_object(object, problems);
            for (_, child) in object.iter_mut() {
                walk(child, problems);
            }
        }
        Value::Array(items) => {
            for item in items {
                walk(item, problems);
            }
        }
        _ => {}
    }
}

fn annotate_object(object: &mut Map<String, Value>, problems: &mut Vec<String>) {
    let Some(Value::String(word)) = object.get("evidence") else {
        return;
    };
    let word = word.clone();
    if object.contains_key(ANNOTATION) {
        problems.push(format!(
            "a document already carries `{ANNOTATION}` beside evidence `{word}`; it was left as \
             written and not annotated"
        ));
        return;
    }
    let meaning = match serde_json::from_value::<Evidence>(Value::String(word.clone())) {
        Ok(level) => describe(level),
        Err(_) => {
            problems.push(format!(
                "evidence level `{word}` is not one this server knows; it is passed on unread"
            ));
            json!({
                "level": word,
                "rank": "unknown",
                "establishes": "unknown: this server does not know this level, so read nothing into it",
            })
        }
    };
    object.insert(ANNOTATION.to_owned(), meaning);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_and_temporal_never_read_alike() {
        let temporal = describe(Evidence::Temporal);
        let identity = describe(Evidence::Identity);
        for field in ["level", "rank", "establishes", "does_not_establish"] {
            assert_ne!(temporal[field], identity[field], "{field}");
        }
        assert!(establishes(Evidence::Temporal).contains("hypothesis"));
    }

    #[test]
    fn the_ladder_is_the_engine_order() {
        let mut sorted = LADDER;
        sorted.sort();
        assert_eq!(
            sorted, LADDER,
            "the ladder is weakest first, as `Ord` orders it"
        );
        for level in LADDER {
            let word = serde_json::to_value(level).unwrap();
            assert_eq!(word, Value::String(level.as_str().to_owned()));
        }
    }

    #[test]
    fn an_unknown_level_is_said_to_be_unknown() {
        let mut value = json!({"joins": [{"evidence": "CERTAINTY", "matched": 3}]});
        let problems = annotate(&mut value);
        assert_eq!(problems.len(), 1);
        assert_eq!(value["joins"][0]["matched"], 3);
        assert_eq!(value["joins"][0][ANNOTATION]["rank"], "unknown");
    }

    #[test]
    fn an_existing_annotation_is_not_overwritten() {
        let mut value = json!({"evidence": "KEY", ANNOTATION: "theirs"});
        let problems = annotate(&mut value);
        assert_eq!(problems.len(), 1);
        assert_eq!(value[ANNOTATION], "theirs");
    }
}
