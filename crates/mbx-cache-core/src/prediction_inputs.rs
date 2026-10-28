//! Front coding for the sorted input lists adapters record in action
//! predictions.
//!
//! A prediction names every input of one compilation, sorted. A C or C++
//! compilation names hundreds of headers that share a long directory with the
//! entry before them, and a crate names its sources the same way. Writing each
//! entry as the byte length of the prefix it shares with the previous entry, a
//! colon, and the rest keeps a prediction small enough that a whole task run of
//! them fits one task manifest.

use std::fmt;

/// Most bytes a compact input list may expand to.
///
/// The compact form is bounded on the wire by the protocol's payload limit.
/// Its expanded form is bounded too, so a hostile prefix chain cannot turn a
/// small manifest into an unreasonable allocation while it is decoded.
const MAX_EXPANDED_INPUT_BYTES: usize = 16 * 1024 * 1024;

/// Why a compact input list was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompactInputsError {
    /// The entry at `index` has no `<shared>:` prefix length.
    MissingPrefixLength {
        /// Position of the entry in the list.
        index: usize,
    },
    /// The prefix length of the entry at `index` is not a canonical decimal.
    InvalidPrefixLength {
        /// Position of the entry in the list.
        index: usize,
    },
    /// The entry at `index` shares more of the previous entry than it has, or
    /// ends its shared prefix inside a character.
    PrefixOutsidePrevious {
        /// Position of the entry in the list.
        index: usize,
        /// Prefix length the entry claims.
        shared: usize,
        /// Byte length of the previous expanded entry.
        previous: usize,
    },
    /// Expanding up to the entry at `index` passes the decoder's byte limit.
    TooLarge {
        /// Position of the entry in the list.
        index: usize,
        /// Most bytes a list may expand to.
        limit: usize,
    },
}

impl fmt::Display for CompactInputsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingPrefixLength { index } => write!(
                formatter,
                "compact prediction input {index} has no prefix length"
            ),
            Self::InvalidPrefixLength { index } => write!(
                formatter,
                "compact prediction input {index} has an invalid prefix length"
            ),
            Self::PrefixOutsidePrevious {
                index,
                shared,
                previous,
            } => write!(
                formatter,
                "compact prediction input {index} shares {shared} bytes of a {previous}-byte previous input"
            ),
            Self::TooLarge { index, limit } => write!(
                formatter,
                "compact prediction inputs expand past {limit} bytes at input {index}"
            ),
        }
    }
}

impl std::error::Error for CompactInputsError {}

/// Write each input as the byte length of the prefix it shares with the input
/// before it, a colon, and the rest of it.
///
/// [`expand_prediction_inputs`] reverses it. The shared prefix never ends
/// inside a character, so every entry stays valid UTF-8.
pub fn compact_prediction_inputs(inputs: &[String]) -> Vec<String> {
    let mut previous = "";
    inputs
        .iter()
        .map(|input| {
            let mut shared = previous
                .bytes()
                .zip(input.bytes())
                .take_while(|(left, right)| left == right)
                .count();
            while !input.is_char_boundary(shared) {
                shared -= 1;
            }
            let compact = format!("{shared}:{}", &input[shared..]);
            previous = input;
            compact
        })
        .collect()
}

/// Reverse [`compact_prediction_inputs`].
///
/// Refuses a list that is not front-coded: an entry without a canonical
/// decimal prefix length, a prefix reaching past the previous entry or into
/// one of its characters, or a list that expands past the decoder's limit.
pub fn expand_prediction_inputs(inputs: &[String]) -> Result<Vec<String>, CompactInputsError> {
    let mut expanded: Vec<String> = Vec::with_capacity(inputs.len());
    let mut expanded_bytes = 0_usize;
    for (index, input) in inputs.iter().enumerate() {
        let (shared_text, suffix) = input
            .split_once(':')
            .ok_or(CompactInputsError::MissingPrefixLength { index })?;
        let shared = shared_text
            .parse::<usize>()
            .ok()
            .filter(|shared| shared.to_string() == shared_text)
            .ok_or(CompactInputsError::InvalidPrefixLength { index })?;
        let previous = expanded.last().map_or("", String::as_str);
        let prefix = previous
            .get(..shared)
            .ok_or(CompactInputsError::PrefixOutsidePrevious {
                index,
                shared,
                previous: previous.len(),
            })?;
        let length = shared + suffix.len();
        expanded_bytes = expanded_bytes
            .checked_add(length)
            .filter(|bytes| *bytes <= MAX_EXPANDED_INPUT_BYTES)
            .ok_or(CompactInputsError::TooLarge {
                index,
                limit: MAX_EXPANDED_INPUT_BYTES,
            })?;
        let mut path = String::with_capacity(length);
        path.push_str(prefix);
        path.push_str(suffix);
        expanded.push(path);
    }
    Ok(expanded)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn sorted_inputs_share_their_prefixes_and_round_trip() {
        let inputs = strings(&[
            "/sdk/usr/include/c++/v1/algorithm",
            "/sdk/usr/include/c++/v1/vector",
            "${workspace}/src/lib.rs",
            "${workspace}/src/café.rs",
            "${workspace}/src/caféine.rs",
            "${workspace}/src/cafè.rs",
        ]);
        let compact = compact_prediction_inputs(&inputs);
        assert_eq!(
            compact,
            strings(&[
                "0:/sdk/usr/include/c++/v1/algorithm",
                "24:vector",
                "0:${workspace}/src/lib.rs",
                "17:café.rs",
                "22:ine.rs",
                "20:è.rs",
            ])
        );
        assert_eq!(expand_prediction_inputs(&compact), Ok(inputs));
    }

    #[test]
    fn lists_that_are_not_front_coded_are_refused() {
        for (compact, error) in [
            (
                strings(&["path"]),
                CompactInputsError::MissingPrefixLength { index: 0 },
            ),
            (
                strings(&["0:path", "01:other"]),
                CompactInputsError::InvalidPrefixLength { index: 1 },
            ),
            (
                strings(&["0:path", "+1:other"]),
                CompactInputsError::InvalidPrefixLength { index: 1 },
            ),
            (
                strings(&["1:path"]),
                CompactInputsError::PrefixOutsidePrevious {
                    index: 0,
                    shared: 1,
                    previous: 0,
                },
            ),
            (
                strings(&["0:é", "1:x"]),
                CompactInputsError::PrefixOutsidePrevious {
                    index: 1,
                    shared: 1,
                    previous: 2,
                },
            ),
        ] {
            assert_eq!(
                expand_prediction_inputs(&compact),
                Err(error),
                "{compact:?}"
            );
        }
    }

    #[test]
    fn a_prefix_chain_cannot_expand_past_the_limit() {
        let long = "x".repeat(1024 * 1024);
        let mut compact = vec![format!("0:{long}")];
        compact.extend((0..16).map(|_| format!("{}:", long.len())));
        assert_eq!(
            expand_prediction_inputs(&compact),
            Err(CompactInputsError::TooLarge {
                index: 16,
                limit: MAX_EXPANDED_INPUT_BYTES,
            })
        );
    }
}
