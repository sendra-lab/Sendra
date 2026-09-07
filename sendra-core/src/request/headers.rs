//! Repeated-header (de)serialization: a `headers:`/`query:`/`form:` mapping
//! may repeat a name, which a plain YAML mapping cannot express directly — see
//! [`crate::Request::headers`] for the on-disk shape this supports.

use serde::Deserialize;

/// Deserialize a `headers:` mapping into ordered `(name, value)` pairs.
///
/// A standard YAML mapping cannot have two keys with the same name, so a
/// value may be either a scalar (one header) or a sequence of scalars (one
/// header per entry, expanded in list order) — see the shape documented on
/// [`Request::headers`](crate::Request::headers). Order among distinct names
/// is preserved exactly as the underlying `MapAccess` yields it, which for
/// `serde_yaml` is document order.
pub(crate) fn deserialize_headers<'de, D>(
    deserializer: D,
) -> Result<Vec<(String, String)>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::{MapAccess, SeqAccess, Visitor};

    enum HeaderValue {
        Single(String),
        Multiple(Vec<String>),
    }

    // Hand-written rather than `#[serde(untagged)]`, which reports every
    // mistake as "data did not match any variant of untagged enum
    // HeaderValue". This way a number where a value belongs is serde's own
    // "invalid type: integer `5`, expected a string or a list of strings",
    // naming what was found and what was wanted.
    impl<'de> Deserialize<'de> for HeaderValue {
        fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            struct HeaderValueVisitor;

            impl<'de> Visitor<'de> for HeaderValueVisitor {
                type Value = HeaderValue;

                fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                    f.write_str("a string or a list of strings")
                }

                fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
                    Ok(HeaderValue::Single(value.to_string()))
                }

                // An unquoted `X-Api-Version: 2` read as the header value "2"
                // back when this field was a `BTreeMap<String, String>`, since
                // that is what serde_yaml does for a plain scalar asked for as
                // a string. Kept, so the type change does not quietly start
                // rejecting files that have always worked.
                fn visit_bool<E: serde::de::Error>(self, value: bool) -> Result<Self::Value, E> {
                    Ok(HeaderValue::Single(value.to_string()))
                }

                fn visit_i64<E: serde::de::Error>(self, value: i64) -> Result<Self::Value, E> {
                    Ok(HeaderValue::Single(value.to_string()))
                }

                fn visit_u64<E: serde::de::Error>(self, value: u64) -> Result<Self::Value, E> {
                    Ok(HeaderValue::Single(value.to_string()))
                }

                fn visit_f64<E: serde::de::Error>(self, value: f64) -> Result<Self::Value, E> {
                    Ok(HeaderValue::Single(value.to_string()))
                }

                fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
                where
                    A: SeqAccess<'de>,
                {
                    let mut values = Vec::new();
                    while let Some(value) = seq.next_element::<String>()? {
                        values.push(value);
                    }
                    Ok(HeaderValue::Multiple(values))
                }
            }

            deserializer.deserialize_any(HeaderValueVisitor)
        }
    }

    struct HeadersVisitor;

    impl<'de> Visitor<'de> for HeadersVisitor {
        type Value = Vec<(String, String)>;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("a map of header name to a string or list of strings")
        }

        fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
        where
            A: MapAccess<'de>,
        {
            let mut headers = Vec::new();
            while let Some((name, value)) = map.next_entry::<String, HeaderValue>()? {
                match value {
                    HeaderValue::Single(value) => headers.push((name, value)),
                    HeaderValue::Multiple(values) => {
                        headers.extend(values.into_iter().map(|value| (name.clone(), value)));
                    }
                }
            }
            Ok(headers)
        }
    }

    deserializer.deserialize_map(HeadersVisitor)
}

/// Serialize ordered `(name, value)` pairs back into a `headers:` mapping.
///
/// The inverse of [`deserialize_headers`]: a name that occurs once is written
/// as a scalar, one that occurs more than once is grouped under that name as
/// a list, in the order its values first and subsequently appear. Grouping
/// means two occurrences of the same name that were *not* adjacent in the
/// original `Vec` come back out adjacent — the only place this round trip is
/// lossy, and unobservable in practice since nothing else in Sendra cares
/// where among same-named headers a value sits.
pub(crate) fn serialize_headers<S>(
    headers: &[(String, String)],
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    use serde::ser::SerializeMap;

    let mut order: Vec<&str> = Vec::new();
    let mut grouped: std::collections::HashMap<&str, Vec<&str>> = std::collections::HashMap::new();
    for (name, value) in headers {
        let values = grouped.entry(name.as_str()).or_default();
        if values.is_empty() {
            order.push(name.as_str());
        }
        values.push(value.as_str());
    }

    let mut map = serializer.serialize_map(Some(order.len()))?;
    for name in order {
        let values = &grouped[name];
        if values.len() == 1 {
            map.serialize_entry(name, values[0])?;
        } else {
            map.serialize_entry(name, values)?;
        }
    }
    map.end()
}
