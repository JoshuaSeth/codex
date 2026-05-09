use serde::Deserializer;
use serde::Serializer;

pub fn deserialize_double_option<'de, T, D>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    T: serde::Deserialize<'de>,
    D: Deserializer<'de>,
{
    serde_with::rust::double_option::deserialize(deserializer)
}

pub fn serialize_double_option<T, S>(
    value: &Option<Option<T>>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    T: serde::Serialize,
    S: Serializer,
{
    serde_with::rust::double_option::serialize(value, serializer)
}
