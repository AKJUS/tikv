// Copyright 2017 TiKV Project Authors. Licensed under Apache-2.0.

use std::{
    cmp::{Ord, Ordering},
    f64,
};

use super::{super::Result, ERR_CONVERT_FAILED, Json, JsonRef, JsonType, constants::*};
use crate::codec::convert::ToStringValue;

fn compare<T: Ord>(x: T, y: T) -> Ordering {
    x.cmp(&y)
}

fn compare_i64_u64(x: i64, y: u64) -> Ordering {
    if x < 0 {
        Ordering::Less
    } else {
        compare::<u64>(x as u64, y)
    }
}

fn compare_f64_with_epsilon(x: f64, y: f64) -> Option<Ordering> {
    if (x - y).abs() < f64::EPSILON {
        Some(Ordering::Equal)
    } else {
        x.partial_cmp(&y)
    }
}

impl JsonRef<'_> {
    fn get_precedence(&self) -> i32 {
        match self.get_type() {
            JsonType::Object => PRECEDENCE_OBJECT,
            JsonType::Array => PRECEDENCE_ARRAY,
            JsonType::Literal => self
                .get_literal()
                .map_or(PRECEDENCE_NULL, |_| PRECEDENCE_BOOLEAN),
            JsonType::I64 | JsonType::U64 | JsonType::Double => PRECEDENCE_NUMBER,
            JsonType::String => PRECEDENCE_STRING,
            JsonType::Opaque => PRECEDENCE_OPAQUE,
            JsonType::Date => PRECEDENCE_DATE,
            JsonType::Datetime => PRECEDENCE_DATETIME,
            JsonType::Timestamp => PRECEDENCE_DATETIME,
            JsonType::Time => PRECEDENCE_TIME,
        }
    }

    fn as_f64(&self) -> Result<f64> {
        match self.get_type() {
            JsonType::I64 => Ok(self.get_i64() as f64),
            JsonType::U64 => Ok(self.get_u64() as f64),
            JsonType::Double => Ok(self.get_double()),
            JsonType::Literal => {
                let v = self.as_literal().unwrap();
                Ok(v.into())
            }
            _ => Err(invalid_type!(
                "{} from {} to f64",
                ERR_CONVERT_FAILED,
                self.to_string_value()
            )),
        }
    }
}

impl Eq for JsonRef<'_> {}

impl Ord for JsonRef<'_> {
    fn cmp(&self, right: &JsonRef<'_>) -> Ordering {
        self.partial_cmp(right).unwrap()
    }
}

impl PartialEq for JsonRef<'_> {
    fn eq(&self, right: &JsonRef<'_>) -> bool {
        self.partial_cmp(right)
            .is_some_and(|r| r == Ordering::Equal)
    }
}
impl PartialOrd for JsonRef<'_> {
    // See `CompareBinary` in TiDB `types/json/binary_functions.go`
    fn partial_cmp(&self, right: &JsonRef<'_>) -> Option<Ordering> {
        let precedence_diff = self.get_precedence() - right.get_precedence();
        if precedence_diff == 0 {
            if self.get_precedence() == PRECEDENCE_NULL {
                // for JSON null.
                return Some(Ordering::Equal);
            }

            return match self.get_type() {
                JsonType::I64 => match right.get_type() {
                    JsonType::I64 => Some(compare(self.get_i64(), right.get_i64())),
                    JsonType::U64 => Some(compare_i64_u64(self.get_i64(), right.get_u64())),
                    JsonType::Double => {
                        compare_f64_with_epsilon(self.get_i64() as f64, right.as_f64().unwrap())
                    }
                    _ => unreachable!(),
                },
                JsonType::U64 => match right.get_type() {
                    JsonType::I64 => {
                        Some(compare_i64_u64(right.get_i64(), self.get_u64()).reverse())
                    }
                    JsonType::U64 => Some(compare(self.get_u64(), right.get_u64())),
                    JsonType::Double => {
                        compare_f64_with_epsilon(self.get_u64() as f64, right.as_f64().unwrap())
                    }
                    _ => unreachable!(),
                },
                JsonType::Double => {
                    compare_f64_with_epsilon(self.as_f64().unwrap(), right.as_f64().unwrap())
                }
                JsonType::Literal => {
                    // false is less than true.
                    self.get_literal().partial_cmp(&right.get_literal())
                }
                JsonType::Object => {
                    // only equal is defined on two json objects.
                    // larger and smaller are not defined.
                    self.value().partial_cmp(right.value())
                }
                JsonType::String => {
                    if let (Ok(left), Ok(right)) = (self.get_str_bytes(), right.get_str_bytes()) {
                        left.partial_cmp(right)
                    } else {
                        return None;
                    }
                }
                JsonType::Array => {
                    let left_count = self.get_elem_count();
                    let right_count = right.get_elem_count();
                    let mut i = 0;
                    while i < left_count && i < right_count {
                        if let (Ok(left_ele), Ok(right_ele)) =
                            (self.array_get_elem(i), right.array_get_elem(i))
                        {
                            match left_ele.partial_cmp(&right_ele) {
                                order @ None
                                | order @ Some(Ordering::Greater)
                                | order @ Some(Ordering::Less) => return order,
                                Some(Ordering::Equal) => i += 1,
                            }
                        } else {
                            return None;
                        }
                    }
                    Some(left_count.cmp(&right_count))
                }
                JsonType::Opaque => {
                    if let (Ok(left), Ok(right)) =
                        (self.get_opaque_bytes(), right.get_opaque_bytes())
                    {
                        left.partial_cmp(right)
                    } else {
                        return None;
                    }
                }
                JsonType::Date | JsonType::Datetime | JsonType::Timestamp => {
                    // The jsonTypePrecedences guarantees that the DATE is only comparable with the
                    // DATE, and the DATETIME and TIMESTAMP will compare with
                    // each other
                    if let (Ok(left), Ok(right)) = (self.get_time(), right.get_time()) {
                        left.partial_cmp(&right)
                    } else {
                        return None;
                    }
                }
                JsonType::Time => {
                    if let (Ok(left), Ok(right)) = (self.get_duration(), right.get_duration()) {
                        left.partial_cmp(&right)
                    } else {
                        return None;
                    }
                }
            };
        }

        if precedence_diff > 0 {
            Some(Ordering::Greater)
        } else {
            Some(Ordering::Less)
        }
    }
}

impl Eq for Json {}
impl Ord for Json {
    fn cmp(&self, right: &Json) -> Ordering {
        self.as_ref().partial_cmp(&right.as_ref()).unwrap()
    }
}

impl PartialEq for Json {
    fn eq(&self, right: &Json) -> bool {
        self.as_ref().partial_cmp(&right.as_ref()).unwrap() == Ordering::Equal
    }
}

impl PartialOrd for Json {
    fn partial_cmp(&self, right: &Json) -> Option<Ordering> {
        self.as_ref().partial_cmp(&right.as_ref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        codec::{
            data_type::Duration,
            mysql::{Time, TimeType},
        },
        expr::EvalContext,
    };

    #[test]
    fn test_cmp_json_numberic_type() {
        let cases = vec![
            (
                Json::from_i64(922337203685477581),
                Json::from_i64(922337203685477580),
                Ordering::Greater,
            ),
            (
                Json::from_i64(-1),
                Json::from_u64(18446744073709551615),
                Ordering::Less,
            ),
            (
                Json::from_i64(922337203685477580),
                Json::from_u64(922337203685477581),
                Ordering::Less,
            ),
            (Json::from_i64(2), Json::from_u64(1), Ordering::Greater),
            (
                Json::from_i64(i64::MAX),
                Json::from_u64(i64::MAX as u64),
                Ordering::Equal,
            ),
            (
                Json::from_u64(18446744073709551615),
                Json::from_i64(-1),
                Ordering::Greater,
            ),
            (
                Json::from_u64(922337203685477581),
                Json::from_i64(922337203685477580),
                Ordering::Greater,
            ),
            (Json::from_u64(1), Json::from_i64(2), Ordering::Less),
            (
                Json::from_u64(i64::MAX as u64),
                Json::from_i64(i64::MAX),
                Ordering::Equal,
            ),
            (Json::from_f64(9.0), Json::from_i64(9), Ordering::Equal),
            (Json::from_f64(8.9), Json::from_i64(9), Ordering::Less),
            (Json::from_f64(9.1), Json::from_i64(9), Ordering::Greater),
            (Json::from_f64(9.0), Json::from_u64(9), Ordering::Equal),
            (Json::from_f64(8.9), Json::from_u64(9), Ordering::Less),
            (Json::from_f64(9.1), Json::from_u64(9), Ordering::Greater),
            (Json::from_i64(9), Json::from_f64(9.0), Ordering::Equal),
            (Json::from_i64(9), Json::from_f64(8.9), Ordering::Greater),
            (Json::from_i64(9), Json::from_f64(9.1), Ordering::Less),
            (Json::from_u64(9), Json::from_f64(9.0), Ordering::Equal),
            (Json::from_u64(9), Json::from_f64(8.9), Ordering::Greater),
            (Json::from_u64(9), Json::from_f64(9.1), Ordering::Less),
        ];

        for (left, right, expected) in cases {
            let left = left.unwrap();
            let right = right.unwrap();
            assert_eq!(expected, left.partial_cmp(&right).unwrap());
        }
    }

    #[test]
    fn test_cmp_json_between_same_type() {
        let test_cases = vec![
            ("false", "true"),
            ("-3", "3"),
            ("3", "5"),
            ("3.0", "4.9"),
            (r#""hello""#, r#""hello, world""#),
            (r#"["a", "b"]"#, r#"["a", "c"]"#),
            (r#"{"a": "b"}"#, r#"{"a": "c"}"#),
        ];
        for (left_str, right_str) in test_cases {
            let left: Json = left_str.parse().unwrap();
            let right: Json = right_str.parse().unwrap();
            assert!(left < right);
            assert_eq!(left, left.clone());
        }
        assert_eq!(Json::none().unwrap(), Json::none().unwrap());
    }

    #[test]
    fn test_cmp_json_between_diff_type() {
        let test_cases = vec![
            ("1.5", "2"),
            ("1.5", "false"),
            ("1.5", "true"),
            ("2", "true"),
            ("null", r#"{"a": "b"}"#),
            ("2", r#""hello, world""#),
            (r#""hello, world""#, r#"{"a": "b"}"#),
            (r#"{"a": "b"}"#, r#"["a", "b"]"#),
            (r#"["a", "b"]"#, "false"),
        ];

        for (left_str, right_str) in test_cases {
            let left: Json = left_str.parse().unwrap();
            let right: Json = right_str.parse().unwrap();
            assert!(left < right);
        }
    }

    #[test]
    fn test_cmp_json_between_json_type() {
        let mut ctx = EvalContext::default();

        let cmp = [
            (
                Json::from_time(
                    Time::parse(
                        &mut ctx,
                        "1998-06-13 12:13:14",
                        TimeType::DateTime,
                        0,
                        false,
                    )
                    .unwrap(),
                )
                .unwrap(),
                Json::from_time(
                    Time::parse(
                        &mut ctx,
                        "1998-06-14 13:14:15",
                        TimeType::DateTime,
                        0,
                        false,
                    )
                    .unwrap(),
                )
                .unwrap(),
                Ordering::Less,
            ),
            (
                Json::from_time(
                    Time::parse(
                        &mut ctx,
                        "1998-06-13 12:13:14",
                        TimeType::DateTime,
                        0,
                        false,
                    )
                    .unwrap(),
                )
                .unwrap(),
                Json::from_time(
                    Time::parse(
                        &mut ctx,
                        "1998-06-12 13:14:15",
                        TimeType::DateTime,
                        0,
                        false,
                    )
                    .unwrap(),
                )
                .unwrap(),
                Ordering::Greater,
            ),
            (
                // DateTime is always greater than Date
                Json::from_time(
                    Time::parse(
                        &mut ctx,
                        "1998-06-13 12:13:14",
                        TimeType::DateTime,
                        0,
                        false,
                    )
                    .unwrap(),
                )
                .unwrap(),
                Json::from_time(
                    Time::parse(&mut ctx, "1998-06-14", TimeType::Date, 0, false).unwrap(),
                )
                .unwrap(),
                Ordering::Greater,
            ),
            (
                Json::from_duration(Duration::parse(&mut ctx, "12:13:14", 0).unwrap()).unwrap(),
                Json::from_duration(Duration::parse(&mut ctx, "12:13:16", 0).unwrap()).unwrap(),
                Ordering::Less,
            ),
            (
                Json::from_duration(Duration::parse(&mut ctx, "12:13:16", 0).unwrap()).unwrap(),
                Json::from_duration(Duration::parse(&mut ctx, "12:13:14", 0).unwrap()).unwrap(),
                Ordering::Greater,
            ),
            (
                // Time is always greater than Date
                Json::from_duration(Duration::parse(&mut ctx, "12:13:16", 0).unwrap()).unwrap(),
                Json::from_time(
                    Time::parse(&mut ctx, "1998-06-12", TimeType::Date, 0, false).unwrap(),
                )
                .unwrap(),
                Ordering::Greater,
            ),
            (
                // Time is always less than DateTime
                Json::from_duration(Duration::parse(&mut ctx, "12:13:16", 0).unwrap()).unwrap(),
                Json::from_time(
                    Time::parse(
                        &mut ctx,
                        "1998-06-12 11:11:11",
                        TimeType::DateTime,
                        0,
                        false,
                    )
                    .unwrap(),
                )
                .unwrap(),
                Ordering::Less,
            ),
        ];

        for (l, r, result) in cmp {
            assert_eq!(l.cmp(&r), result)
        }
    }
}
