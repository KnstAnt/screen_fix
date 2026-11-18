use std::{fmt, marker::PhantomData};

use serde::{Deserialize, Deserializer, Serialize, de::{self, SeqAccess, Visitor}};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PayloadLimit(pub usize);

impl fmt::Display for PayloadLimit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Payload exceeds the limit of {} elements or UTF-8 bytes.", self.0)
    }
}

impl std::error::Error for PayloadLimit {}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct BoundedText<const MAX: usize>(Box<str>);

impl<const MAX: usize> BoundedText<MAX> {
    pub fn new(value: &str) -> Result<Self, PayloadLimit> {
        if value.len() > MAX {
            return Err(PayloadLimit(MAX));
        }
        Ok(Self(value.into()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de, const MAX: usize> Deserialize<'de> for BoundedText<MAX> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct TextVisitor<const MAX: usize>;

        impl<const MAX: usize> Visitor<'_> for TextVisitor<MAX> {
            type Value = BoundedText<MAX>;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "at most {MAX} UTF-8 bytes")
            }

            fn visit_str<E: de::Error>(self, text: &str) -> Result<Self::Value, E> {
                BoundedText::new(text).map_err(E::custom)
            }
        }

        deserializer.deserialize_str(TextVisitor::<MAX>)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct BoundedList<T, const MAX: usize>(Box<[T]>);

impl<T, const MAX: usize> BoundedList<T, MAX> {
    pub fn new(values: Vec<T>) -> Result<Self, PayloadLimit> {
        if values.len() > MAX {
            return Err(PayloadLimit(MAX));
        }
        Ok(Self(values.into_boxed_slice()))
    }

    pub fn as_slice(&self) -> &[T] {
        &self.0
    }
}

impl<'de, T: Deserialize<'de>, const MAX: usize> Deserialize<'de> for BoundedList<T, MAX> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ListVisitor<T, const MAX: usize>(PhantomData<T>);

        impl<'de, T: Deserialize<'de>, const MAX: usize> Visitor<'de> for ListVisitor<T, MAX> {
            type Value = BoundedList<T, MAX>;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "at most {MAX} items")
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut values = Vec::with_capacity(seq.size_hint().unwrap_or(0).min(MAX));
                while values.len() < MAX {
                    let Some(value) = seq.next_element()? else {
                        return Ok(BoundedList(values.into_boxed_slice()));
                    };
                    values.push(value);
                }
                if seq.next_element::<de::IgnoredAny>()?.is_some() {
                    return Err(de::Error::custom(PayloadLimit(MAX)));
                }
                Ok(BoundedList(values.into_boxed_slice()))
            }
        }

        deserializer.deserialize_seq(ListVisitor::<T, MAX>(PhantomData))
    }
}
