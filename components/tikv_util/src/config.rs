// Copyright 2016 TiKV Project Authors. Licensed under Apache-2.0.

use std::{
    error::Error,
    fmt::{self, Write},
    fs,
    net::{SocketAddrV4, SocketAddrV6},
    ops::{Add, AddAssign, Div, DivAssign, Mul, MulAssign, Sub, SubAssign},
    path::{Path, PathBuf},
    str::{self, FromStr},
    sync::{
        Arc, RwLock, RwLockReadGuard,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use chrono::{
    DateTime, FixedOffset, Local, NaiveTime, TimeZone, Timelike,
    format::{self, Fixed, Item, Parsed},
};
pub use heck::KebabCase;
use online_config::ConfigValue;
use serde::{
    Deserialize, Deserializer, Serialize, Serializer,
    de::{self, Unexpected, Visitor},
};
use serde_json::Value;
use thiserror::Error;

use super::time::Instant;
use crate::slow_log;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("{0}")]
    Limit(String),
    #[error("config address error: {0}")]
    Address(String),
    #[error("store label error: {0}")]
    StoreLabels(String),
    #[error("config value error: {0}")]
    Value(String),
    #[error("config fs: {0}")]
    FileSystem(String),
}

const UNIT: u64 = 1;

const BINARY_DATA_MAGNITUDE: u64 = 1024;
pub const B: u64 = UNIT;
pub const KIB: u64 = UNIT * BINARY_DATA_MAGNITUDE;
pub const MIB: u64 = KIB * BINARY_DATA_MAGNITUDE;
pub const GIB: u64 = MIB * BINARY_DATA_MAGNITUDE;
pub const TIB: u64 = GIB * BINARY_DATA_MAGNITUDE;
pub const PIB: u64 = TIB * BINARY_DATA_MAGNITUDE;

const TIME_MAGNITUDE_1: u64 = 1000;
const TIME_MAGNITUDE_2: u64 = 60;
const TIME_MAGNITUDE_3: u64 = 24;
const US: u64 = UNIT;
const MS: u64 = US * TIME_MAGNITUDE_1;
const SECOND: u64 = MS * TIME_MAGNITUDE_1;
const MINUTE: u64 = SECOND * TIME_MAGNITUDE_2;
const HOUR: u64 = MINUTE * TIME_MAGNITUDE_2;
const DAY: u64 = HOUR * TIME_MAGNITUDE_3;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum LogFormat {
    Text,
    Json,
}

#[derive(Clone, Debug, Copy, PartialEq, PartialOrd, Default)]
pub struct ReadableSize(pub u64);

impl From<ReadableSize> for ConfigValue {
    fn from(size: ReadableSize) -> ConfigValue {
        ConfigValue::Size(size.0)
    }
}

impl From<ConfigValue> for ReadableSize {
    fn from(c: ConfigValue) -> ReadableSize {
        if let ConfigValue::Size(s) = c {
            ReadableSize(s)
        } else {
            panic!("expect: ConfigValue::Size, got: {:?}", c);
        }
    }
}

impl ReadableSize {
    pub const fn kb(count: u64) -> ReadableSize {
        ReadableSize(count * KIB)
    }

    pub const fn mb(count: u64) -> ReadableSize {
        ReadableSize(count * MIB)
    }

    pub const fn gb(count: u64) -> ReadableSize {
        ReadableSize(count * GIB)
    }

    pub const fn as_mb(self) -> u64 {
        self.0 / MIB
    }

    pub fn as_mb_f64(self) -> f64 {
        self.0 as f64 / MIB as f64
    }
}

impl Div<u64> for ReadableSize {
    type Output = ReadableSize;

    fn div(self, rhs: u64) -> ReadableSize {
        ReadableSize(self.0 / rhs)
    }
}

impl Div<ReadableSize> for ReadableSize {
    type Output = u64;

    fn div(self, rhs: ReadableSize) -> u64 {
        self.0 / rhs.0
    }
}

impl Sub<ReadableSize> for ReadableSize {
    type Output = ReadableSize;
    fn sub(self, rhs: ReadableSize) -> Self::Output {
        ReadableSize(self.0 - rhs.0)
    }
}

impl Mul<u64> for ReadableSize {
    type Output = ReadableSize;

    fn mul(self, rhs: u64) -> ReadableSize {
        ReadableSize(self.0 * rhs)
    }
}

impl fmt::Display for ReadableSize {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let size = self.0;
        if size == 0 {
            write!(f, "{}KiB", size)
        } else if size % PIB == 0 {
            write!(f, "{}PiB", size / PIB)
        } else if size % TIB == 0 {
            write!(f, "{}TiB", size / TIB)
        } else if size % GIB == 0 {
            write!(f, "{}GiB", size / GIB)
        } else if size % MIB == 0 {
            write!(f, "{}MiB", size / MIB)
        } else if size % KIB == 0 {
            write!(f, "{}KiB", size / KIB)
        } else {
            write!(f, "{}B", size)
        }
    }
}

impl Serialize for ReadableSize {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut buffer = String::new();
        write!(buffer, "{}", self).unwrap();
        serializer.serialize_str(&buffer)
    }
}

impl FromStr for ReadableSize {
    type Err = String;

    // This method parses value in binary unit.
    fn from_str(s: &str) -> Result<ReadableSize, String> {
        let size_str = s.trim();
        if size_str.is_empty() {
            return Err(format!("{s:?} is not a valid size."));
        }

        if !size_str.is_ascii() {
            return Err(format!("ASCII string is expected, but got {s:?}"));
        }

        // size: digits and '.' as decimal separator
        let size_len = size_str
            .to_string()
            .chars()
            .take_while(|c| char::is_ascii_digit(c) || ['.', 'e', 'E', '-', '+'].contains(c))
            .count();

        // unit: alphabetic characters
        let (size, unit) = size_str.split_at(size_len);

        let unit = match unit.trim() {
            "K" | "KB" | "KiB" => KIB,
            "M" | "MB" | "MiB" => MIB,
            "G" | "GB" | "GiB" => GIB,
            "T" | "TB" | "TiB" => TIB,
            "P" | "PB" | "PiB" => PIB,
            "B" | "" => {
                if size.chars().all(|c| char::is_ascii_digit(&c)) {
                    return size
                        .parse::<u64>()
                        .map(|n| ReadableSize(n))
                        .map_err(|_| format!("invalid size string: {:?}", s));
                }
                UNIT
            }
            _ => {
                return Err(format!(
                    "only B, KB, KiB, MB, MiB, GB, GiB, TB, TiB, PB, and PiB are supported: {s:?}"
                ));
            }
        };

        match size.parse::<f64>() {
            Ok(n) => Ok(ReadableSize((n * unit as f64) as u64)),
            Err(_) => Err(format!("invalid size string: {s:?}")),
        }
    }
}

impl<'de> Deserialize<'de> for ReadableSize {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct SizeVisitor;

        impl Visitor<'_> for SizeVisitor {
            type Value = ReadableSize;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("valid size")
            }

            fn visit_i64<E>(self, size: i64) -> Result<ReadableSize, E>
            where
                E: de::Error,
            {
                if size >= 0 {
                    self.visit_u64(size as u64)
                } else {
                    Err(E::invalid_value(Unexpected::Signed(size), &self))
                }
            }

            fn visit_u64<E>(self, size: u64) -> Result<ReadableSize, E>
            where
                E: de::Error,
            {
                Ok(ReadableSize(size))
            }

            fn visit_str<E>(self, size_str: &str) -> Result<ReadableSize, E>
            where
                E: de::Error,
            {
                size_str.parse().map_err(E::custom)
            }
        }

        deserializer.deserialize_any(SizeVisitor)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Ord, PartialOrd, Default)]
pub struct ReadableDuration(pub Duration);

impl Add for ReadableDuration {
    type Output = ReadableDuration;

    fn add(self, rhs: ReadableDuration) -> ReadableDuration {
        Self(self.0 + rhs.0)
    }
}

impl AddAssign for ReadableDuration {
    fn add_assign(&mut self, rhs: ReadableDuration) {
        *self = *self + rhs;
    }
}

impl Sub for ReadableDuration {
    type Output = ReadableDuration;

    fn sub(self, rhs: ReadableDuration) -> ReadableDuration {
        Self(self.0 - rhs.0)
    }
}

impl SubAssign for ReadableDuration {
    fn sub_assign(&mut self, rhs: ReadableDuration) {
        *self = *self - rhs;
    }
}

impl Mul<u32> for ReadableDuration {
    type Output = ReadableDuration;

    fn mul(self, rhs: u32) -> Self::Output {
        Self(self.0 * rhs)
    }
}

impl MulAssign<u32> for ReadableDuration {
    fn mul_assign(&mut self, rhs: u32) {
        *self = *self * rhs;
    }
}

impl Div<u32> for ReadableDuration {
    type Output = ReadableDuration;

    fn div(self, rhs: u32) -> ReadableDuration {
        Self(self.0 / rhs)
    }
}

impl DivAssign<u32> for ReadableDuration {
    fn div_assign(&mut self, rhs: u32) {
        *self = *self / rhs;
    }
}

impl From<ReadableDuration> for Duration {
    fn from(readable: ReadableDuration) -> Duration {
        readable.0
    }
}

impl From<ReadableDuration> for ConfigValue {
    fn from(duration: ReadableDuration) -> ConfigValue {
        ConfigValue::Duration(duration.0.as_millis() as u64)
    }
}

impl From<ConfigValue> for ReadableDuration {
    fn from(d: ConfigValue) -> ReadableDuration {
        if let ConfigValue::Duration(d) = d {
            ReadableDuration(Duration::from_millis(d))
        } else {
            panic!("expect: ConfigValue::Duration, got: {:?}", d);
        }
    }
}

impl FromStr for ReadableDuration {
    type Err = String;

    fn from_str(dur_str: &str) -> Result<ReadableDuration, String> {
        let dur_str = dur_str.trim();
        if !dur_str.is_ascii() {
            return Err(format!("unexpect ascii string: {}", dur_str));
        }
        let err_msg = "valid duration, only d, h, m, s, ms, us are supported.".to_owned();
        let mut left = dur_str.as_bytes();
        let mut last_unit = DAY + 1;
        let mut dur = 0f64;
        while let Some(idx) = left.iter().position(|c| b"dhmsu".contains(c)) {
            let (first, second) = left.split_at(idx);
            let unit = if second.starts_with(b"ms") {
                left = &left[idx + 2..];
                MS
            } else if second.starts_with(b"us") {
                left = &left[idx + 2..];
                US
            } else {
                let u = match second[0] {
                    b'd' => DAY,
                    b'h' => HOUR,
                    b'm' => MINUTE,
                    b's' => SECOND,
                    _ => return Err(err_msg),
                };
                left = &left[idx + 1..];
                u
            };
            if unit >= last_unit {
                return Err("d, h, m, s, ms, us should occur in given order.".to_owned());
            }
            // do we need to check 12h360m?
            let number_str = unsafe { str::from_utf8_unchecked(first) };
            dur += match number_str.trim().parse::<f64>() {
                Ok(n) => n * unit as f64,
                Err(_) => return Err(err_msg),
            };
            last_unit = unit;
        }
        if !left.is_empty() {
            return Err(err_msg);
        }
        if dur.is_sign_negative() {
            return Err("duration should be positive.".to_owned());
        }
        let secs = dur as u64 / SECOND;
        let micros = (dur as u64 % SECOND) as u32 * 1_000;
        Ok(ReadableDuration(Duration::new(secs, micros)))
    }
}

impl ReadableDuration {
    pub const ZERO: ReadableDuration = ReadableDuration(Duration::ZERO);

    pub const fn micros(micros: u64) -> ReadableDuration {
        ReadableDuration(Duration::from_micros(micros))
    }

    pub const fn millis(millis: u64) -> ReadableDuration {
        ReadableDuration(Duration::from_millis(millis))
    }

    pub const fn secs(secs: u64) -> ReadableDuration {
        ReadableDuration(Duration::from_secs(secs))
    }

    pub const fn minutes(minutes: u64) -> ReadableDuration {
        ReadableDuration::secs(minutes * 60)
    }

    pub const fn hours(hours: u64) -> ReadableDuration {
        ReadableDuration::minutes(hours * 60)
    }

    pub const fn days(days: u64) -> ReadableDuration {
        ReadableDuration::hours(days * 24)
    }

    pub fn as_secs(&self) -> u64 {
        self.0.as_secs()
    }

    pub fn as_secs_f64(&self) -> f64 {
        self.0.as_secs_f64()
    }

    pub fn as_millis(&self) -> u64 {
        crate::time::duration_to_ms(self.0)
    }

    pub fn as_micros(&self) -> u64 {
        crate::time::duration_to_us(self.0)
    }

    pub fn is_zero(&self) -> bool {
        self.0.as_nanos() == 0
    }
}

impl fmt::Display for ReadableDuration {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut dur = crate::time::duration_to_us(self.0);
        let mut written = false;
        if dur >= DAY {
            written = true;
            write!(f, "{}d", dur / DAY)?;
            dur %= DAY;
        }
        if dur >= HOUR {
            written = true;
            write!(f, "{}h", dur / HOUR)?;
            dur %= HOUR;
        }
        if dur >= MINUTE {
            written = true;
            write!(f, "{}m", dur / MINUTE)?;
            dur %= MINUTE;
        }
        if dur >= SECOND {
            written = true;
            write!(f, "{}s", dur / SECOND)?;
            dur %= SECOND;
        }
        if dur >= MS {
            written = true;
            write!(f, "{}ms", dur / MS)?;
            dur %= MS;
        }
        if dur > 0 {
            written = true;
            write!(f, "{}us", dur)?;
        }
        if !written {
            write!(f, "0s")?;
        }
        Ok(())
    }
}

impl Serialize for ReadableDuration {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut buffer = String::new();
        write!(buffer, "{}", self).unwrap();
        serializer.serialize_str(&buffer)
    }
}

impl<'de> Deserialize<'de> for ReadableDuration {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct DurVisitor;

        impl Visitor<'_> for DurVisitor {
            type Value = ReadableDuration;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("valid duration")
            }

            fn visit_str<E>(self, dur_str: &str) -> Result<ReadableDuration, E>
            where
                E: de::Error,
            {
                dur_str.parse().map_err(E::custom)
            }
        }

        deserializer.deserialize_str(DurVisitor)
    }
}

#[derive(Clone, Debug, Copy, PartialEq)]
pub struct ReadableOffsetTime(pub NaiveTime, pub FixedOffset);

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
pub struct ReadableSchedule(pub Vec<ReadableOffsetTime>);

impl From<ReadableSchedule> for ConfigValue {
    fn from(otv: ReadableSchedule) -> ConfigValue {
        ConfigValue::Schedule(
            otv.0
                .into_iter()
                .map(|offset_time| offset_time.to_string())
                .collect(),
        )
    }
}

impl From<ConfigValue> for ReadableSchedule {
    fn from(c: ConfigValue) -> ReadableSchedule {
        if let ConfigValue::Schedule(otv) = c {
            ReadableSchedule(
                otv.into_iter()
                    .map(|s| ReadableOffsetTime::from_str(s.as_str()).unwrap())
                    .collect::<Vec<_>>(),
            )
        } else {
            panic!("expect: ConfigValue::Schedule, got: {:?}", c)
        }
    }
}

impl ReadableSchedule {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn is_scheduled_this_hour<Tz: TimeZone>(&self, datetime: &DateTime<Tz>) -> bool {
        self.0.iter().any(|time| time.hour_matches(datetime))
    }

    pub fn is_scheduled_this_hour_minute<Tz: TimeZone>(&self, datetime: &DateTime<Tz>) -> bool {
        self.0
            .iter()
            .any(|time| time.hour_minutes_matches(datetime))
    }
}

fn parse_string_to_vec(input: &str) -> Result<Vec<String>, String> {
    let value: Value = serde_json::from_str(input).unwrap();

    if let Value::Array(arr) = value {
        return Ok(arr
            .into_iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect());
    }

    Err(format!("{:?} cannot be parsed to vec", input))
}

impl FromStr for ReadableSchedule {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        Ok(ReadableSchedule(
            parse_string_to_vec(s)?
                .into_iter()
                .map(|s| ReadableOffsetTime::from_str(s.as_str()))
                .try_collect()?,
        ))
    }
}

impl FromStr for ReadableOffsetTime {
    type Err = String;

    fn from_str(ot_str: &str) -> Result<ReadableOffsetTime, String> {
        let (time, offset) = if let Some((time_str, offset_str)) = ot_str.split_once(' ') {
            let time = NaiveTime::parse_from_str(time_str, "%H:%M").map_err(|e| e.to_string())?;
            let offset = parse_offset(offset_str)?;
            (time, offset)
        } else {
            let time = NaiveTime::parse_from_str(ot_str, "%H:%M").map_err(|e| e.to_string())?;
            (time, local_offset())
        };
        Ok(ReadableOffsetTime(time, offset))
    }
}

/// Returns the `FixedOffset` for the timezone this `tikv` server has been
/// configured to use.
fn local_offset() -> FixedOffset {
    let &offset = Local::now().offset();
    offset
}

/// Parses the offset specified by `str`.
/// Note: `FixedOffset` in latest `chrono` implements `FromStr`. Once we are
/// able to upgrade to it (`components/tidb_query_datatype` requires a large
/// refactoring that is outside the scope of this PR), we can remove this
/// method.
fn parse_offset(offset_str: &str) -> Result<FixedOffset, String> {
    let mut parsed = Parsed::new();
    format::parse(
        &mut parsed,
        offset_str,
        [Item::Fixed(Fixed::TimezoneOffsetZ)].iter(),
    )
    .map_err(|e| e.to_string())?;
    parsed.to_fixed_offset().map_err(|e| e.to_string())
}

impl fmt::Display for ReadableOffsetTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.0.format("%H:%M"), self.1)
    }
}

impl ReadableOffsetTime {
    /// Converts `datetime` from `Tz` to the same timezone as this instance and
    /// returns `true` if the hour of the day is matches hour of this
    /// instance.
    pub fn hour_matches<Tz: TimeZone>(&self, datetime: &DateTime<Tz>) -> bool {
        self.convert_to_this_offset(datetime).hour() == self.0.hour()
    }

    /// Converts `datetime` from `Tz` to the same timezone as this instance and
    /// returns `true` if hours and minutes match this instance.
    pub fn hour_minutes_matches<Tz: TimeZone>(&self, datetime: &DateTime<Tz>) -> bool {
        let time = self.convert_to_this_offset(datetime);
        time.hour() == self.0.hour() && time.minute() == self.0.minute()
    }

    fn convert_to_this_offset<Tz: TimeZone>(&self, datetime: &DateTime<Tz>) -> NaiveTime {
        datetime.with_timezone(&self.1).time()
    }
}

impl Serialize for ReadableOffsetTime {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut buffer = String::new();
        write!(buffer, "{}", self).unwrap();
        serializer.serialize_str(&buffer)
    }
}

impl<'de> Deserialize<'de> for ReadableOffsetTime {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct OffTimeVisitor;

        impl Visitor<'_> for OffTimeVisitor {
            type Value = ReadableOffsetTime;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("valid duration")
            }

            fn visit_str<E>(self, off_time_str: &str) -> Result<ReadableOffsetTime, E>
            where
                E: de::Error,
            {
                off_time_str.parse().map_err(E::custom)
            }
        }

        deserializer.deserialize_str(OffTimeVisitor)
    }
}

pub fn normalize_path<P: AsRef<Path>>(path: P) -> PathBuf {
    use std::path::Component;
    let mut components = path.as_ref().components().peekable();
    let mut ret = PathBuf::new();

    while let Some(c @ (Component::Prefix(..) | Component::RootDir)) = components.peek().cloned() {
        components.next();
        ret.push(c.as_os_str());
    }

    for component in components {
        match component {
            Component::Prefix(..) | Component::RootDir => unreachable!(),
            Component::CurDir => {}
            c @ Component::ParentDir => {
                if !ret.pop() {
                    ret.push(c.as_os_str());
                }
            }
            Component::Normal(c) => ret.push(c),
        }
    }
    ret
}

/// Normalizes the path and canonicalizes its longest physically existing
/// sub-path.
fn canonicalize_non_existing_path<P: AsRef<Path>>(path: P) -> std::io::Result<PathBuf> {
    fn try_canonicalize_normalized_path(path: &Path) -> std::io::Result<PathBuf> {
        use std::path::Component;
        let mut components = path.components().peekable();
        let mut should_canonicalize = true;
        let mut ret = if path.is_relative() {
            Path::new(".").canonicalize()?
        } else {
            PathBuf::new()
        };

        while let Some(c @ (Component::Prefix(..) | Component::RootDir)) =
            components.peek().cloned()
        {
            components.next();
            ret.push(c.as_os_str());
        }
        // normalize() will only preserve leading ParentDir.
        while let Some(Component::ParentDir) = components.peek().cloned() {
            components.next();
            ret.pop();
        }

        for component in components {
            match component {
                Component::Normal(c) => {
                    ret.push(c);
                    // We try to canonicalize a longest path based on fs info.
                    if should_canonicalize {
                        match ret.as_path().canonicalize() {
                            Ok(path) => {
                                ret = path;
                            }
                            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                                should_canonicalize = false;
                            }
                            other => return other,
                        }
                    }
                }
                Component::Prefix(..)
                | Component::RootDir
                | Component::ParentDir
                | Component::CurDir => unreachable!(),
            }
        }
        Ok(ret)
    }
    try_canonicalize_normalized_path(&normalize_path(path))
}

/// Normalizes the path and canonicalizes its longest physically existing
/// sub-path.
fn canonicalize_imp<P: AsRef<Path>>(path: P) -> std::io::Result<PathBuf> {
    match path.as_ref().canonicalize() {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => canonicalize_non_existing_path(path),
        other => other,
    }
}

pub fn canonicalize_path(path: &str) -> Result<String, Box<dyn Error>> {
    canonicalize_sub_path(path, "")
}

pub fn canonicalize_sub_path(path: &str, sub_path: &str) -> Result<String, Box<dyn Error>> {
    let path = canonicalize_imp(Path::new(path).join(Path::new(sub_path)))?;
    if path.exists() && path.is_file() {
        return Err(format!("{}/{} is not a directory!", path.display(), sub_path).into());
    }
    Ok(format!("{}", path.display()))
}

pub fn canonicalize_log_dir(path: &str, filename: &str) -> Result<String, Box<dyn Error>> {
    let mut path = canonicalize_imp(Path::new(path))?;
    if path.is_file() {
        return Ok(format!("{}", path.display()));
    }
    if !filename.is_empty() {
        path = path.join(Path::new(filename));
    }
    if path.is_dir() {
        return Err(format!("{} is a directory!", path.display()).into());
    }
    Ok(format!("{}", path.display()))
}

pub fn ensure_dir_exist(path: &str) -> Result<(), Box<dyn Error>> {
    if !path.is_empty() {
        let p = Path::new(path);
        if !p.exists() {
            fs::create_dir_all(p)?;
        }
    }
    Ok(())
}

#[cfg(unix)]
pub fn check_max_open_fds(expect: u64) -> Result<(), ConfigError> {
    #[cfg(target_os = "freebsd")]
    let expect = expect as i64;

    use std::mem;

    unsafe {
        let mut fd_limit = mem::zeroed();
        let mut err = libc::getrlimit(libc::RLIMIT_NOFILE, &mut fd_limit);
        if err != 0 {
            return Err(ConfigError::Limit("check_max_open_fds failed".to_owned()));
        }
        if fd_limit.rlim_cur >= expect {
            return Ok(());
        }

        let prev_limit = fd_limit.rlim_cur;
        fd_limit.rlim_cur = expect;
        if fd_limit.rlim_max < expect {
            // If the process is not started by privileged user, this will fail.
            fd_limit.rlim_max = expect;
        }
        err = libc::setrlimit(libc::RLIMIT_NOFILE, &fd_limit);
        if err == 0 {
            return Ok(());
        }
        Err(ConfigError::Limit(format!(
            "the maximum number of open file descriptors is too \
             small, got {}, expect greater or equal to {}",
            prev_limit, expect
        )))
    }
}

#[cfg(not(unix))]
pub fn check_max_open_fds(_: u64) -> Result<(), ConfigError> {
    Ok(())
}

#[cfg(target_os = "linux")]
mod check_kernel {
    use std::fs;

    use super::ConfigError;

    // pub for tests.
    pub type Checker = dyn Fn(i64, i64) -> bool;

    // pub for tests.
    pub fn check_kernel_params(
        param_path: &str,
        expect: i64,
        checker: Box<Checker>,
    ) -> Result<(), ConfigError> {
        let buffer = fs::read_to_string(param_path)
            .map_err(|e| ConfigError::Limit(format!("check_kernel_params failed {}", e)))?;

        let got = buffer
            .trim_matches('\n')
            .parse::<i64>()
            .map_err(|e| ConfigError::Limit(format!("check_kernel_params failed {}", e)))?;

        let mut param = String::new();
        // skip 3, ["", "proc", "sys", ...]
        for path in param_path.split('/').skip(3) {
            param.push_str(path);
            param.push('.');
        }
        param.pop();

        if !checker(got, expect) {
            return Err(ConfigError::Limit(format!(
                "kernel parameters {} got {}, expect {}",
                param, got, expect
            )));
        }

        info!("kernel parameters"; "param" => param, "value" => got);
        Ok(())
    }

    /// `check_kernel_params` checks kernel parameters, following are checked so
    /// far:
    ///   - `net.core.somaxconn` should be greater or equal to 32768.
    ///   - `net.ipv4.tcp_syncookies` should be 0
    ///   - `vm.swappiness` shoud be 0
    ///
    /// Note that: It works on **Linux** only.
    pub fn check_kernel() -> Vec<ConfigError> {
        let params: Vec<(&str, i64, Box<Checker>)> = vec![
            // Check net.core.somaxconn.
            (
                "/proc/sys/net/core/somaxconn",
                32768,
                Box::new(|got, expect| got >= expect),
            ),
            // Check net.ipv4.tcp_syncookies.
            (
                "/proc/sys/net/ipv4/tcp_syncookies",
                0,
                Box::new(|got, expect| got == expect),
            ),
            // Check vm.swappiness.
            (
                "/proc/sys/vm/swappiness",
                0,
                Box::new(|got, expect| got == expect),
            ),
        ];

        let mut errors = Vec::with_capacity(params.len());
        for (param_path, expect, checker) in params {
            if let Err(e) = check_kernel_params(param_path, expect, checker) {
                errors.push(e);
            }
        }

        errors
    }
}

#[cfg(target_os = "linux")]
pub use self::check_kernel::check_kernel;

#[cfg(not(target_os = "linux"))]
pub fn check_kernel() -> Vec<ConfigError> {
    Vec::new()
}

#[cfg(target_os = "linux")]
mod check_data_dir {
    use std::{
        ffi::{CStr, CString},
        fs,
        path::Path,
        sync::Mutex,
    };

    use lazy_static::lazy_static;

    use super::{ConfigError, canonicalize_path};

    #[derive(Debug, Default)]
    struct FsInfo {
        tp: String,
        opts: String,
        mnt_dir: String,
        fsname: String,
    }

    fn get_fs_info(path: &str, mnt_file: &str) -> Result<FsInfo, ConfigError> {
        lazy_static! {
            // According `man 3 getmntent`, The pointer returned by `getmntent` points
            // to a static area of memory which is overwritten by subsequent calls.
            // So we use a lock to protect it in order to avoid `make dev` fail.
            static ref GETMNTENT_LOCK: Mutex<()> = Mutex::new(());
        }

        let op = "data-dir.fsinfo.get";

        unsafe {
            let _lock = GETMNTENT_LOCK.lock().unwrap();

            let profile = CString::new(mnt_file).unwrap();
            let retype = CString::new("r").unwrap();
            let afile = libc::setmntent(profile.as_ptr(), retype.as_ptr());
            if afile.is_null() {
                return Err(ConfigError::FileSystem("error opening fstab".to_string()));
            }
            let mut fs = FsInfo::default();
            loop {
                let ent = libc::getmntent(afile);
                if ent.is_null() {
                    break;
                }
                let ent = &*ent;
                let cur_dir = CStr::from_ptr(ent.mnt_dir).to_str().unwrap();
                if path.starts_with(cur_dir) && cur_dir.len() >= fs.mnt_dir.len() {
                    fs.tp = CStr::from_ptr(ent.mnt_type).to_str().unwrap().to_owned();
                    fs.opts = CStr::from_ptr(ent.mnt_opts).to_str().unwrap().to_owned();
                    fs.fsname = CStr::from_ptr(ent.mnt_fsname).to_str().unwrap().to_owned();
                    fs.mnt_dir = cur_dir.to_owned();
                }
            }

            libc::endmntent(afile);
            if fs.mnt_dir.is_empty() {
                return Err(ConfigError::FileSystem(format!(
                    "{}: path: {:?} not find in mountable",
                    op, path
                )));
            }
            Ok(fs)
        }
    }

    fn get_rotational_info(fsname: &str) -> Result<String, ConfigError> {
        let op = "data-dir.rotation.get";
        // get device path
        let device = match fs::canonicalize(fsname) {
            Ok(path) => format!("{}", path.display()),
            Err(_) => String::from(fsname),
        };
        let dev = device.trim_start_matches("/dev/");
        let block_dir = "/sys/block";
        let mut device_dir = format!("{}/{}", block_dir, dev);
        if !Path::new(&device_dir).exists() {
            let dir = fs::read_dir(block_dir).map_err(|e| {
                ConfigError::FileSystem(format!(
                    "{}: read block dir {:?} failed: {:?}",
                    op, block_dir, e
                ))
            })?;
            let mut find = false;
            for entry in dir {
                if entry.is_err() {
                    continue;
                }
                let entry = entry.unwrap();
                let mut cur_path = entry.path();
                cur_path.push(dev);
                if cur_path.exists() {
                    device_dir = entry.path().to_str().unwrap().to_owned();
                    find = true;
                    break;
                }
            }
            if !find {
                return Err(ConfigError::FileSystem(format!(
                    "{}: {:?} no device find in block",
                    op, fsname
                )));
            }
        }

        let rota_path = format!("{}/queue/rotational", device_dir);
        if !Path::new(&rota_path).exists() {
            return Err(ConfigError::FileSystem(format!(
                "{}: block {:?} has no rotational file",
                op, device_dir
            )));
        }

        let buffer = fs::read_to_string(&rota_path).map_err(|e| {
            ConfigError::FileSystem(format!("{}: {:?} failed: {:?}", op, rota_path, e))
        })?;
        Ok(buffer.trim_matches('\n').to_owned())
    }

    // check device && fs
    pub fn check_data_dir(data_path: &str, mnt_file: &str) -> Result<(), ConfigError> {
        let op = "data-dir.check";
        let real_path = match canonicalize_path(data_path) {
            Ok(path) => path,
            Err(e) => {
                return Err(ConfigError::FileSystem(format!(
                    "{}: path: {:?} canonicalize failed: {:?}",
                    op, data_path, e
                )));
            }
        };

        // TODO check ext4 nodelalloc
        let fs_info = get_fs_info(&real_path, mnt_file)?;
        info!("data dir"; "data_path" => data_path, "mount_fs" => ?fs_info);

        if get_rotational_info(&fs_info.fsname)? != "0" {
            warn!("not on SSD device"; "data_path" => data_path);
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use std::{fs::File, io::Write, os::unix::fs::symlink};

        use tempfile::Builder;

        use super::*;

        fn create_file(fpath: &str, buf: &[u8]) {
            let mut file = File::create(fpath).unwrap();
            file.write_all(buf).unwrap();
            file.flush().unwrap();
        }

        #[test]
        fn test_get_fs_info() {
            let tmp_dir = Builder::new().prefix("test-get-fs-info").tempdir().unwrap();
            let mninfo = br#"tmpfs /home tmpfs rw,nosuid,noexec,relatime,size=1628744k,mode=755 0 0
/dev/sda4 /home/shirly ext4 rw,relatime,errors=remount-ro,data=ordered 0 0
/dev/sdb /data1 ext4 rw,relatime,errors=remount-ro,data=ordered 0 0
securityfs /sys/kernel/security securityfs rw,nosuid,nodev,noexec,relatime 0 0
"#;
            let mnt_file = format!("{}", tmp_dir.into_path().join("mnt.txt").display());
            create_file(&mnt_file, mninfo);
            let f = get_fs_info("/home/shirly/1111", &mnt_file).unwrap();
            assert_eq!(f.fsname, "/dev/sda4");
            assert_eq!(f.mnt_dir, "/home/shirly");

            // not found
            let f2 = get_fs_info("/tmp", &mnt_file);
            f2.unwrap_err();
        }

        #[test]
        fn test_get_rotational_info() {
            // test device not exist
            let ret = get_rotational_info("/dev/invalid");
            ret.unwrap_err();
        }

        #[test]
        fn test_check_data_dir() {
            // test invalid data_path
            check_data_dir("/sys/invalid", "/proc/mounts").unwrap_err();
            // get real path's fs_info
            let tmp_dir = Builder::new()
                .prefix("test-check-data-dir")
                .tempdir()
                .unwrap();
            let data_path = format!("{}/data1", tmp_dir.path().display());
            ::std::fs::create_dir(&data_path).unwrap();
            let fs_info = get_fs_info(&data_path, "/proc/mounts").unwrap();

            // data_path may not mounted on a normal device on container
            // /proc/mounts may contain host's device, which is not accessible in container.
            if Path::new("/.dockerenv").exists()
                && (!fs_info.fsname.starts_with("/dev") || !Path::new(&fs_info.fsname).exists())
            {
                return;
            }

            // test with real path
            check_data_dir(&data_path, "/proc/mounts").unwrap();

            // test with device mapper
            // get real_path's rotational info
            let expect = get_rotational_info(&fs_info.fsname).unwrap();
            // ln -s fs_info.fsname tmp_device
            let tmp_device = format!("{}/tmp_device", tmp_dir.path().display());
            // /dev/xxx may not exists in container.
            if !Path::new(&fs_info.fsname).exists() {
                return;
            }
            symlink(&fs_info.fsname, &tmp_device).unwrap();
            // mount info: data_path=>tmp_device
            let mninfo = format!(
                "{} {} ext4 rw,relatime,errors=remount-ro,data=ordered 0 0",
                &tmp_device, &data_path
            );
            let mnt_file = format!("{}/mnt.txt", tmp_dir.path().display());
            create_file(&mnt_file, mninfo.as_bytes());
            // check info
            check_data_dir(&data_path, &mnt_file).unwrap();
            // check rotational info
            let get = get_rotational_info(&tmp_device).unwrap();
            assert_eq!(expect, get);
        }
    }
}

#[cfg(target_os = "linux")]
pub fn check_data_dir(data_path: &str) -> Result<(), ConfigError> {
    self::check_data_dir::check_data_dir(data_path, "/proc/mounts")
}

#[cfg(not(target_os = "linux"))]
pub fn check_data_dir(_data_path: &str) -> Result<(), ConfigError> {
    Ok(())
}

fn get_file_count(data_path: &str, extension: &str) -> Result<usize, ConfigError> {
    let op = "data-dir.file-count.get";
    let dir = fs::read_dir(data_path).map_err(|e| {
        ConfigError::FileSystem(format!(
            "{}: read file dir {:?} failed: {:?}",
            op, data_path, e
        ))
    })?;
    let mut file_count = 0;
    for entry in dir {
        let entry = entry.map_err(|e| {
            ConfigError::FileSystem(format!(
                "{}: read file in file dir {:?} failed: {:?}",
                op, data_path, e
            ))
        })?;
        let path = entry.path();
        if path.is_file() {
            if let Some(ext) = path.extension() {
                if extension.is_empty() || extension == ext {
                    file_count += 1;
                }
            } else if extension.is_empty() {
                file_count += 1;
            }
        }
    }
    Ok(file_count)
}

// check dir is empty of file with certain extension, empty string for any
// extension.
pub fn check_data_dir_empty(data_path: &str, extension: &str) -> Result<(), ConfigError> {
    let op = "data-dir.empty.check";
    let dir = Path::new(data_path);
    if dir.exists() && !dir.is_file() {
        let count = get_file_count(data_path, extension)?;
        if count > 0 {
            return Err(ConfigError::Limit(format!(
                "{}: the number of file with extension {} in directory {} is non-zero, \
                 got {}, expect 0.",
                op, extension, data_path, count,
            )));
        }
    }
    Ok(())
}

/// `check_addr` validates an address. Addresses are formed like "Host:Port".
/// More details about **Host** and **Port** can be found in WHATWG URL
/// Standard.
///
/// Return whether the address is unspecified, i.e. `0.0.0.0` or `::0`
pub fn check_addr(addr: &str) -> Result<bool, ConfigError> {
    // Try to validate "IPv4:Port" and "[IPv6]:Port".
    if let Ok(a) = SocketAddrV4::from_str(addr) {
        return Ok(a.ip().is_unspecified());
    }
    if let Ok(a) = SocketAddrV6::from_str(addr) {
        return Ok(a.ip().is_unspecified());
    }

    let parts: Vec<&str> = addr
        .split(':')
        .filter(|s| !s.is_empty()) // "Host:" or ":Port" are invalid.
        .collect();

    // ["Host", "Port"]
    if parts.len() != 2 {
        return Err(ConfigError::Address(format!("invalid addr: {:?}", addr)));
    }

    // Check Port.
    let port: u16 = parts[1].parse().map_err(|_| {
        ConfigError::Address(format!("invalid addr, parse port failed: {:?}", addr))
    })?;
    // Port = 0 is invalid.
    if port == 0 {
        return Err(ConfigError::Address(format!(
            "invalid addr, port can not be 0: {:?}",
            addr
        )));
    }

    // Check Host.
    if let Err(e) = url::Host::parse(parts[0]) {
        return Err(ConfigError::Address(format!("invalid addr: {:?}", e)));
    }

    Ok(false)
}

#[derive(Default)]
pub struct VersionTrack<T> {
    value: RwLock<T>,
    version: AtomicU64,
}

impl<T> VersionTrack<T> {
    pub fn new(value: T) -> Self {
        VersionTrack {
            value: RwLock::new(value),
            version: AtomicU64::new(1),
        }
    }

    pub fn update<F, O, E>(&self, f: F) -> Result<O, E>
    where
        F: FnOnce(&mut T) -> Result<O, E>,
    {
        let res = f(&mut self.value.write().unwrap());
        if res.is_ok() {
            self.version.fetch_add(1, Ordering::Release);
        }
        res
    }

    pub fn value(&self) -> RwLockReadGuard<'_, T> {
        self.value.read().unwrap()
    }

    pub fn tracker(self: Arc<Self>, tag: String) -> Tracker<T> {
        Tracker {
            tag,
            version: self.version.load(Ordering::Relaxed),
            inner: self,
        }
    }
}

#[derive(Clone, Default)]
pub struct Tracker<T> {
    tag: String,
    inner: Arc<VersionTrack<T>>,
    version: u64,
}

impl<T> Tracker<T> {
    // The update of `value` and `version` is not atomic
    // so there maybe false positive.
    pub fn any_new(&mut self) -> Option<RwLockReadGuard<'_, T>> {
        let v = self.inner.version.load(Ordering::Acquire);
        if self.version < v {
            self.version = v;
            match self.inner.value.try_read() {
                Ok(value) => Some(value),
                Err(_) => {
                    let t = Instant::now_coarse();
                    let value = self.inner.value.read().unwrap();
                    slow_log!(
                        t.saturating_elapsed(),
                        "{} tracker get updated value",
                        self.tag
                    );
                    Some(value)
                }
            }
        } else {
            None
        }
    }
}

use std::collections::HashMap;

/// TomlLine use to parse one line content of a toml file
#[derive(Debug)]
enum TomlLine {
    // the `Keys` from "[`Keys`]"
    Table(String),
    // the `Keys` from "`Keys` = value"
    KvPair(String),
    // Comment, empty line, etc.
    Unknown,
}

impl TomlLine {
    fn encode_kv(key: &str, val: &str) -> String {
        format!("{} = {}", key, val)
    }

    // parse kv pair from format of "`Keys` = value"
    fn parse_kv(s: &str) -> TomlLine {
        let mut v: Vec<_> = s.split('=').map(|s| s.trim().to_owned()).rev().collect();
        if v.is_empty() || v.len() > 2 || TomlLine::parse_key(v[v.len() - 1].as_str()).is_none() {
            return TomlLine::Unknown;
        }
        TomlLine::KvPair(v.pop().unwrap())
    }

    fn parse(s: &str) -> TomlLine {
        let s = s.trim();
        // try to parse table from format of "[`Keys`]"
        if let Some(k) = s.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            return match TomlLine::parse_key(k) {
                Some(k) => TomlLine::Table(k),
                None => TomlLine::Unknown,
            };
        }
        // remove one prefix of '#' if exist
        let kv = s.strip_prefix('#').unwrap_or(s);
        TomlLine::parse_kv(kv)
    }

    // Parse `Keys`, only bare keys and dotted keys are supportted
    // bare keys only contains chars of A-Za-z0-9_-
    // dotted keys are a sequence of bare key joined with a '.'
    fn parse_key(s: &str) -> Option<String> {
        if s.is_empty() || s.starts_with('.') || s.ends_with('.') {
            return None;
        }
        let ks: Vec<_> = s.split('.').map(str::trim).collect();
        let is_valid_key = |s: &&str| -> bool {
            s.chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_".contains(c))
        };
        if ks.iter().all(is_valid_key) {
            return Some(ks.join("."));
        }
        None
    }

    fn concat_key(k1: &str, k2: &str) -> String {
        match (k1.is_empty(), k2.is_empty()) {
            (false, false) => format!("{}.{}", k1, k2),
            (_, true) => k1.to_owned(),
            (true, _) => k2.to_owned(),
        }
    }

    // get the prefix keys: "`Keys`.bare_key" -> Keys
    fn get_prefix(key: &str) -> String {
        key.rsplitn(2, '.').last().unwrap().to_owned()
    }
}

/// TomlWriter use to update the config file and only cover the most common toml
/// format that used by tikv config file, toml format like: quoted keys,
/// multi-line value, inline table, etc, are not supported, see <https://github.com/toml-lang/toml>
/// for more detail.
pub struct TomlWriter {
    dst: Vec<u8>,
    current_table: String,
}

impl Default for TomlWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl TomlWriter {
    pub fn new() -> TomlWriter {
        TomlWriter {
            dst: Vec::new(),
            current_table: "".to_owned(),
        }
    }

    pub fn write_change(&mut self, src: String, mut change: HashMap<String, String>) {
        for line in src.lines() {
            match TomlLine::parse(line) {
                TomlLine::Table(keys) => {
                    self.write_current_table(&mut change);
                    self.write(line.as_bytes());
                    self.current_table = keys;
                }
                TomlLine::KvPair(keys) => {
                    match change.remove(&TomlLine::concat_key(&self.current_table, &keys)) {
                        None => self.write(line.as_bytes()),
                        Some(chg) => self.write(TomlLine::encode_kv(&keys, &chg).as_bytes()),
                    }
                }
                TomlLine::Unknown => self.write(line.as_bytes()),
            }
        }
        if change.is_empty() {
            return;
        }
        self.write_current_table(&mut change);
        while !change.is_empty() {
            self.current_table = TomlLine::get_prefix(change.keys().last().unwrap());
            self.write(format!("[{}]", self.current_table).as_bytes());
            self.write_current_table(&mut change);
        }
        self.new_line();
    }

    fn write_current_table(&mut self, change: &mut HashMap<String, String>) {
        let keys: Vec<_> = change
            .keys()
            .filter_map(|k| k.split('.').next_back())
            .map(str::to_owned)
            .collect();
        for k in keys {
            if let Some(chg) = change.remove(&TomlLine::concat_key(&self.current_table, &k)) {
                self.write(TomlLine::encode_kv(&k, &chg).as_bytes());
            }
        }
    }

    fn write(&mut self, s: &[u8]) {
        self.dst.extend_from_slice(s);
        self.new_line();
    }

    fn new_line(&mut self) {
        self.dst.push(b'\n');
    }

    pub fn finish(self) -> Vec<u8> {
        self.dst
    }
}

#[macro_export]
macro_rules! numeric_enum_serializing_mod {
    ($name:ident $enum:ident { $($variant:ident = $value:expr, )* }) => {
        pub mod $name {
            use std::fmt;

            use serde::{Serializer, Deserializer};
            use serde::de::{self, Unexpected, Visitor};
            use super::$enum;

            pub fn serialize<S>(mode: &$enum, serializer: S) -> Result<S::Ok, S::Error>
                where S: Serializer
            {
                match mode {
                    $( $enum::$variant => serializer.serialize_i64($value as i64), )*
                }
            }

            pub fn deserialize<'de, D>(deserializer: D) -> Result<$enum, D::Error>
                where D: Deserializer<'de>
            {
                struct EnumVisitor;

                impl Visitor<'_> for EnumVisitor {
                    type Value = $enum;

                    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                        write!(formatter, concat!("valid ", stringify!($enum)))
                    }

                    fn visit_i64<E>(self, value: i64) -> Result<$enum, E>
                        where E: de::Error
                    {
                        match value {
                            $( $value => Ok($enum::$variant), )*
                            _ => Err(E::invalid_value(Unexpected::Signed(value), &self))
                        }
                    }

                    fn visit_str<E>(self, value: &str) -> Result<$enum, E>
                        where E: de::Error
                    {
                        use $crate::config::KebabCase;
                        $(
                            if value == stringify!($variant).to_kebab_case() {
                                return Ok($enum::$variant)
                            }
                        )*
                        Err(E::invalid_value(Unexpected::Str(value), &self))
                    }
                }

                deserializer.deserialize_any(EnumVisitor)
            }

            #[cfg(test)]
            mod tests {
                use toml;
                use super::$enum;
                use serde::{Deserialize, Serialize};
                use $crate::config::KebabCase;

                #[test]
                fn test_serde() {
                    #[derive(Serialize, Deserialize, PartialEq, Debug)]
                    struct EnumHolder {
                        #[serde(with = "super")]
                        e: $enum,
                    }

                    let cases = vec![
                        $(($enum::$variant, $value), )*
                    ];
                    for (e, v) in cases {
                        let holder = EnumHolder { e };
                        let res = toml::to_string(&holder).unwrap();
                        let exp = format!("e = {}\n", v);
                        assert_eq!(res, exp);
                        let h: EnumHolder = toml::from_str(&exp).unwrap();
                        assert!(h == holder);
                    }
                    $({
                        let s = stringify!($variant);
                        let res = format!("e = \"{}\"\n", s.to_kebab_case());
                        let h: EnumHolder = toml::from_str(&res).unwrap();
                        assert!(h.e == $enum::$variant);
                    })*
                    let s = format!("{}-bad-variant---", stringify!($enum));
                    let res = format!("e = \"{}\"\n", s.to_kebab_case());
                    toml::from_str::<EnumHolder>(&res).unwrap_err();
                }
            }
        }
    }
}

/// Helper for migrating Raft data safely. Such migration is defined as
/// multiple states that can be uniquely distinguished. And the transitions
/// between these states are atomic.
///
/// States:
///   1. Init - Only source directory contains Raft data.
///   2. Migrating - A marker file contains the path of source directory. The
///      source directory contains a complete copy of Raft data. Target
///      directory may exist.
///   3. Completed - Only target directory contains Raft data. Marker file may
///      exist.
pub struct RaftDataStateMachine {
    root: PathBuf,
    in_progress_marker: PathBuf,
    source: PathBuf,
    target: PathBuf,
}

impl RaftDataStateMachine {
    pub fn new(root: &str, source: &str, target: &str) -> Self {
        let root = PathBuf::from(root);
        let in_progress_marker = root.join("MIGRATING-RAFT");
        let source = PathBuf::from(source);
        let target = PathBuf::from(target);
        Self {
            root,
            in_progress_marker,
            source,
            target,
        }
    }

    /// Checks if the current condition is a valid state.
    pub fn validate(&self, should_exist: bool) -> std::result::Result<(), String> {
        if Self::data_exists(&self.source)
            && Self::data_exists(&self.target)
            && !self.in_progress_marker.exists()
        {
            return Err(format!(
                "Found multiple raft data sets: {}, {}",
                self.source.display(),
                self.target.display()
            ));
        }
        let exists = Self::data_exists(&self.source) || Self::data_exists(&self.target);
        if exists != should_exist {
            if should_exist {
                return Err("Cannot find raft data set.".to_owned());
            } else {
                return Err("Found raft data set when it should not exist.".to_owned());
            }
        }
        Ok(())
    }

    /// Returns whether a migration is needed. When it's needed, enters the
    /// `Migrating` state. Otherwise prepares the target directory for
    /// opening.
    pub fn before_open_target(&mut self) -> bool {
        // Clean up trash directory if there is any.
        for p in [&self.source, &self.target] {
            let trash = p.with_extension("REMOVE");
            if trash.exists() {
                fs::remove_dir_all(&trash).unwrap();
            }
        }
        if !Self::data_exists(&self.source) {
            // Recover from Completed state.
            if self.in_progress_marker.exists() {
                Self::must_remove(&self.in_progress_marker);
            }
            return false;
        } else if self.in_progress_marker.exists() {
            if let Some(real_source) = self.read_marker() {
                // Recover from Migrating state.
                if real_source == self.target {
                    if Self::data_exists(&self.target) {
                        Self::must_remove(&self.source);
                        return false;
                    }
                    // It's actually in Completed state, just in the reverse
                    // direction. Equivalent to Init state.
                } else {
                    assert!(real_source == self.source);
                    Self::must_remove(&self.target);
                    return true;
                }
            } else {
                // Halfway between Init and Migrating.
                assert!(!Self::data_exists(&self.target));
            }
        }
        // Init -> Migrating.
        self.write_marker();
        true
    }

    /// Exits the `Migrating` state and enters the `Completed` state.
    pub fn after_dump_data(&mut self) {
        assert!(Self::data_exists(&self.source));
        assert!(Self::data_exists(&self.target));
        Self::must_remove_except(&self.source, &self.target); // Enters the `Completed` state.
        Self::must_remove(&self.in_progress_marker);
    }

    // `after_dump_data` involves two atomic operations, insert a check point
    // between them to test crash safety.
    #[cfg(test)]
    fn after_dump_data_with_check<F: Fn()>(&mut self, check: &F) {
        assert!(Self::data_exists(&self.source));
        assert!(Self::data_exists(&self.target));
        Self::must_remove(&self.source); // Enters the `Completed` state.
        check();
        Self::must_remove(&self.in_progress_marker);
    }

    fn write_marker(&self) {
        use std::io::Write;
        let mut f = fs::File::create(&self.in_progress_marker).unwrap();
        f.write_all(self.source.to_str().unwrap().as_bytes())
            .unwrap();
        f.sync_all().unwrap();
        f.write_all(b"//").unwrap();
        f.sync_all().unwrap();
        Self::sync_dir(&self.root);
    }

    // Assumes there is a marker file. Returns None when the content of marker file
    // is incomplete.
    fn read_marker(&self) -> Option<PathBuf> {
        let marker = fs::read_to_string(&self.in_progress_marker).unwrap();
        if marker.ends_with("//") {
            Some(PathBuf::from(&marker[..marker.len() - 2]))
        } else {
            None
        }
    }

    fn must_remove(path: &Path) {
        if path.exists() {
            if path.is_dir() {
                info!("Removing directory"; "path" => %path.display());
                let trash = path.with_extension("REMOVE");
                Self::must_rename_dir(path, &trash);
                fs::remove_dir_all(&trash).unwrap();
            } else {
                info!("Removing file"; "path" => %path.display());
                fs::remove_file(path).unwrap();
                Self::sync_dir(path.parent().unwrap());
            }
        }
    }

    // Remove all files and directories under `remove_path` except `retain_path`.
    fn must_remove_except(remove_path: &Path, retain_path: &Path) {
        if !remove_path.exists() {
            info!("Path not exists"; "path" => %remove_path.display());
            return;
        }
        if !remove_path.is_dir() {
            info!("Path is not a directory, so remove directly"; "path" => %remove_path.display());
            Self::must_remove(remove_path);
            return;
        }
        if !retain_path.starts_with(remove_path) {
            info!("Removing directory as retain path is not under remove path"; "retain path" => %retain_path.display(), "remove path" => %remove_path.display());
            Self::must_remove(remove_path);
            return;
        }

        for entry in fs::read_dir(remove_path).unwrap() {
            let sub_path = entry.unwrap().path();
            if sub_path != retain_path {
                Self::must_remove(&sub_path);
            }
        }
    }

    fn must_rename_dir(from: &Path, to: &Path) {
        fs::rename(from, to).unwrap();
        let mut dir = to.to_path_buf();
        assert!(dir.pop());
        Self::sync_dir(&dir);
    }

    #[inline]
    fn dir_exists(path: &Path) -> bool {
        path.exists() && path.is_dir()
    }

    pub fn raftengine_exists(path: &Path) -> bool {
        if !Self::dir_exists(path) {
            return false;
        }
        fs::read_dir(path).unwrap().any(|entry| {
            if let Ok(e) = entry {
                let p = e.path();
                p.is_file() && p.extension().is_some_and(|ext| ext == "raftlog")
            } else {
                false
            }
        })
    }

    pub fn raftdb_exists(path: &Path) -> bool {
        if !Self::dir_exists(path) {
            return false;
        }
        let current_file_path = path.join("CURRENT");
        current_file_path.exists() && current_file_path.is_file()
    }

    pub fn data_exists(path: &Path) -> bool {
        Self::raftengine_exists(path) || Self::raftdb_exists(path)
    }

    fn sync_dir(dir: &Path) {
        fs::File::open(dir).and_then(|d| d.sync_all()).unwrap();
    }
}

#[cfg(test)]
mod tests {
    use std::{fs::File, io::Write, path::Path};

    use tempfile::Builder;

    use super::*;

    #[test]
    fn test_readable_size() {
        let s = ReadableSize::kb(2);
        assert_eq!(s.0, 2048);
        assert_eq!(s.as_mb(), 0);
        let s = ReadableSize::mb(2);
        assert_eq!(s.0, 2 * 1024 * 1024);
        assert_eq!(s.as_mb(), 2);
        let s = ReadableSize::gb(2);
        assert_eq!(s.0, 2 * 1024 * 1024 * 1024);
        assert_eq!(s.as_mb(), 2048);

        assert_eq!((ReadableSize::mb(2) / 2).0, MIB);
        assert_eq!((ReadableSize::mb(1) / 2).0, 512 * KIB);
        assert_eq!(ReadableSize::mb(2) / ReadableSize::kb(1), 2048);
    }

    #[test]
    fn test_parse_readable_size() {
        #[derive(Serialize, Deserialize)]
        struct SizeHolder {
            s: ReadableSize,
        }

        let legal_cases = vec![
            (0, "0KiB"),
            (2 * KIB, "2KiB"),
            (4 * MIB, "4MiB"),
            (5 * GIB, "5GiB"),
            (7 * TIB, "7TiB"),
            (11 * PIB, "11PiB"),
        ];
        for (size, exp) in legal_cases {
            let c = SizeHolder {
                s: ReadableSize(size),
            };
            let res_str = toml::to_string(&c).unwrap();
            let exp_str = format!("s = {:?}\n", exp);
            assert_eq!(res_str, exp_str);
            let res_size: SizeHolder = toml::from_str(&exp_str).unwrap();
            assert_eq!(res_size.s.0, size);
        }

        let c = SizeHolder {
            s: ReadableSize((isize::MAX) as u64),
        };
        let res_str = toml::to_string(&c).unwrap();
        assert_eq!(res_str, "s = \"9223372036854775807B\"\n");
        let res_size: SizeHolder = toml::from_str(&res_str).unwrap();
        assert_eq!(res_size.s.0, c.s.0);

        let decode_cases = vec![
            (" 0.5 PB", PIB / 2),
            ("0.5 TB", TIB / 2),
            ("0.5GB ", GIB / 2),
            ("0.5MB", MIB / 2),
            ("0.5KB", KIB / 2),
            ("0.5P", PIB / 2),
            ("0.5T", TIB / 2),
            ("0.5G", GIB / 2),
            ("0.5M", MIB / 2),
            ("0.5K", KIB / 2),
            ("23", 23),
            ("1", 1),
            ("1024B", KIB),
            // units with binary prefixes
            (" 0.5 PiB", PIB / 2),
            ("1PiB", PIB),
            ("0.5 TiB", TIB / 2),
            ("2 TiB", TIB * 2),
            ("0.5GiB ", GIB / 2),
            ("787GiB ", GIB * 787),
            ("0.5MiB", MIB / 2),
            ("3MiB", MIB * 3),
            ("0.5KiB", KIB / 2),
            ("1 KiB", KIB),
            // scientific notation
            ("0.5e6 B", B * 500000),
            ("0.5E6 B", B * 500000),
            ("1e6B", B * 1000000),
            ("8E6B", B * 8000000),
            ("8e7", B * 80000000),
            ("1e-1MB", MIB / 10),
            ("1e+1MB", MIB * 10),
            ("0e+10MB", 0),
        ];
        for (src, exp) in decode_cases {
            let src = format!("s = {:?}", src);
            let res: SizeHolder = toml::from_str(&src).unwrap();
            assert_eq!(res.s.0, exp);
        }

        let illegal_cases = vec![
            "0.5kb", "0.5kB", "0.5Kb", "0.5k", "0.5g", "b", "gb", "1b", "B", "1K24B", " 5_KB",
            "4B7", "5M_",
        ];
        for src in illegal_cases {
            let src_str = format!("s = {:?}", src);
            assert!(toml::from_str::<SizeHolder>(&src_str).is_err(), "{}", src);
        }
    }

    #[test]
    fn test_duration_construction() {
        let mut dur = ReadableDuration::micros(2_010_010);
        assert_eq!(dur.0, Duration::new(2, 10_010_000));
        assert_eq!(dur.as_secs(), 2);
        assert!((dur.as_secs_f64() - 2.010_010).abs() < f64::EPSILON);
        assert_eq!(dur.as_millis(), 2_010);
        dur = ReadableDuration::millis(1001);
        assert_eq!(dur.0, Duration::new(1, 1_000_000));
        assert_eq!(dur.as_secs(), 1);
        assert!((dur.as_secs_f64() - 1.001).abs() < f64::EPSILON);
        assert_eq!(dur.as_millis(), 1001);
        dur = ReadableDuration::secs(1);
        assert_eq!(dur.0, Duration::new(1, 0));
        assert_eq!(dur.as_secs(), 1);
        assert_eq!(dur.as_millis(), 1000);
        dur = ReadableDuration::minutes(2);
        assert_eq!(dur.0, Duration::new(2 * 60, 0));
        assert_eq!(dur.as_secs(), 120);
        assert_eq!(dur.as_millis(), 120000);
        dur = ReadableDuration::hours(2);
        assert_eq!(dur.0, Duration::new(2 * 3600, 0));
        assert_eq!(dur.as_secs(), 7200);
        assert_eq!(dur.as_millis(), 7200000);
    }

    #[test]
    fn test_parse_readable_duration() {
        #[derive(Serialize, Deserialize)]
        struct DurHolder {
            d: ReadableDuration,
        }

        let legal_cases = vec![
            (0, 0, "0s"),
            (0, 1_000, "1ms"),
            (0, 1, "1us"),
            (2, 0, "2s"),
            (24 * 3600, 0, "1d"),
            (2 * 24 * 3600, 10_020, "2d10ms20us"),
            (4 * 60, 0, "4m"),
            (5 * 3600, 0, "5h"),
            (3600 + 2 * 60, 0, "1h2m"),
            (5 * 24 * 3600 + 3600 + 2 * 60, 0, "5d1h2m"),
            (3600 + 2, 5_600, "1h2s5ms600us"),
            (3 * 24 * 3600 + 7 * 3600 + 2, 5_004, "3d7h2s5ms4us"),
        ];
        for (secs, us, exp) in legal_cases {
            let d = DurHolder {
                d: ReadableDuration(Duration::new(secs, us * 1_000)),
            };
            let res_str = toml::to_string(&d).unwrap();
            let exp_str = format!("d = {:?}\n", exp);
            assert_eq!(res_str, exp_str);
            let res_dur: DurHolder = toml::from_str(&exp_str).unwrap();
            assert_eq!(res_dur.d.0, d.d.0);
        }

        let decode_cases = vec![(" 0.5 h2m ", 3600 / 2 + 2 * 60, 0)];
        for (src, secs, ms) in decode_cases {
            let src = format!("d = {:?}", src);
            let res: DurHolder = toml::from_str(&src).unwrap();
            assert_eq!(res.d.0, Duration::new(secs, ms * 1_000_000));
        }

        let illegal_cases = vec!["1H", "1M", "1S", "1MS", "1h1h", "h"];
        for src in illegal_cases {
            let src_str = format!("d = {:?}", src);
            assert!(toml::from_str::<DurHolder>(&src_str).is_err(), "{}", src);
        }
        assert!(toml::from_str::<DurHolder>("d = 23").is_err());
    }

    #[test]
    fn test_readable_offset_time() {
        let decode_cases = vec![
            (
                "23:00 +0000",
                ReadableOffsetTime(
                    NaiveTime::from_hms_opt(23, 00, 00).unwrap(),
                    FixedOffset::east_opt(0).unwrap(),
                ),
            ),
            (
                "03:00",
                ReadableOffsetTime(NaiveTime::from_hms_opt(3, 00, 00).unwrap(), local_offset()),
            ),
            (
                "13:23 +09:30",
                ReadableOffsetTime(
                    NaiveTime::from_hms_opt(13, 23, 00).unwrap(),
                    FixedOffset::east_opt(3600 * 9 + 1800).unwrap(),
                ),
            ),
            (
                "09:30 -08:00",
                ReadableOffsetTime(
                    NaiveTime::from_hms_opt(9, 30, 00).unwrap(),
                    FixedOffset::west_opt(3600 * 8).unwrap(),
                ),
            ),
        ];
        for (encoded, expected) in decode_cases {
            let actual = encoded.parse::<ReadableOffsetTime>().unwrap_or_else(|e| {
                panic!(
                    "error parsing encoded={} expected={} error={}",
                    encoded, expected, e
                )
            });
            assert_eq!(actual, expected);
            let actual = format!("{}", expected)
                .parse::<ReadableOffsetTime>()
                .unwrap();
            assert_eq!(actual, expected);
        }
        let (encoded, actual) = (
            "23:00 +00:00",
            ReadableOffsetTime(
                NaiveTime::from_hms_opt(23, 00, 00).unwrap(),
                FixedOffset::east_opt(0).unwrap(),
            ),
        );
        let actual = format!("{}", actual);
        let expected = encoded.to_owned();
        assert_eq!(actual, expected);

        let time = ReadableOffsetTime(
            NaiveTime::from_hms_opt(9, 30, 00).unwrap(),
            FixedOffset::west_opt(0).unwrap(),
        );
        assert_eq!(format!("{}", time), "09:30 +00:00");
        let dt = DateTime::parse_from_rfc3339("2023-10-27T09:39:57-00:00").unwrap();
        assert!(time.hour_matches(&dt));
        assert!(!time.hour_minutes_matches(&dt));
        let dt = DateTime::parse_from_rfc3339("2023-10-27T09:30:57-00:00").unwrap();
        assert!(time.hour_minutes_matches(&dt));
    }

    #[test]
    fn test_readable_schedule() {
        // Tests HHMM offsets for timezones.
        let schedule = ReadableSchedule(
            vec!["09:30 +0000", "11:15 +0530", "23:00 +0000"]
                .into_iter()
                .flat_map(ReadableOffsetTime::from_str)
                .collect::<Vec<_>>(),
        );

        let time_a = DateTime::parse_from_rfc3339("2023-10-27T09:30:57-00:00").unwrap();
        let time_b = DateTime::parse_from_rfc3339("2023-10-28T09:00:57-00:00").unwrap();
        let time_c = DateTime::parse_from_rfc3339("2023-10-27T23:15:00-00:00").unwrap();
        let time_d = DateTime::parse_from_rfc3339("2023-10-27T23:00:00-00:00").unwrap();
        let time_e = DateTime::parse_from_rfc3339("2023-10-27T20:00:00-00:00").unwrap();
        let time_f = DateTime::parse_from_rfc3339("2024-05-29T05:45:00-00:00").unwrap();

        // positives for schedule by hour
        assert!(schedule.is_scheduled_this_hour(&time_a));
        assert!(schedule.is_scheduled_this_hour(&time_b));
        assert!(schedule.is_scheduled_this_hour(&time_c));
        assert!(schedule.is_scheduled_this_hour(&time_d));
        assert!(schedule.is_scheduled_this_hour(&time_f));

        // negatives for schedule by hour
        assert!(!schedule.is_scheduled_this_hour(&time_e));

        // positives for schedule by hour and minute
        assert!(schedule.is_scheduled_this_hour_minute(&time_a));
        assert!(schedule.is_scheduled_this_hour_minute(&time_d));
        assert!(schedule.is_scheduled_this_hour_minute(&time_f));

        // negatives for schedule by hour and minute
        assert!(!schedule.is_scheduled_this_hour_minute(&time_b));
        assert!(!schedule.is_scheduled_this_hour_minute(&time_c));
        assert!(!schedule.is_scheduled_this_hour_minute(&time_e));
    }

    #[test]
    fn test_readable_schedule_col_separated_offsets() {
        // Test that HH:MM offsets are supported.
        let schedule = ReadableSchedule(
            vec!["09:30 +00:00", "11:15 +05:30", "23:00 +00:00"]
                .into_iter()
                .flat_map(ReadableOffsetTime::from_str)
                .collect::<Vec<_>>(),
        );

        let time_a = DateTime::parse_from_rfc3339("2023-10-27T09:30:57-00:00").unwrap();
        let time_b = DateTime::parse_from_rfc3339("2023-10-27T23:00:00-00:00").unwrap();
        let time_c = DateTime::parse_from_rfc3339("2024-05-29T05:45:00-00:00").unwrap();
        let time_d = DateTime::parse_from_rfc3339("2023-10-27T20:00:00-00:00").unwrap();

        assert!(schedule.is_scheduled_this_hour(&time_a));
        assert!(schedule.is_scheduled_this_hour(&time_b));
        assert!(schedule.is_scheduled_this_hour(&time_c));
        assert!(schedule.is_scheduled_this_hour_minute(&time_c));
        assert!(!schedule.is_scheduled_this_hour_minute(&time_d));
    }

    #[test]
    fn test_readable_schedule_parse() {
        let check_parse = |vec_strs: Vec<String>, strs| {
            let schedule = ReadableSchedule(
                vec_strs
                    .iter()
                    .flat_map(|s| ReadableOffsetTime::from_str(s.as_str()))
                    .collect::<Vec<_>>(),
            );

            let schedule2 = ReadableSchedule::from_str(strs).unwrap();
            assert_eq!(schedule, schedule2);

            let ConfigValue::Schedule(config_value) = ConfigValue::from(schedule) else {
                unreachable!()
            };
            assert_eq!(config_value, vec_strs);
            assert_eq!(
                ReadableSchedule::from(ConfigValue::Schedule(config_value)),
                schedule2
            );
        };

        check_parse(
            vec!["09:30 +00:00".to_owned(), "23:00 +00:00".to_owned()],
            "[\"09:30 +00:00\", \"23:00 +00:00\"]",
        );

        check_parse(
            vec!["11:30 +02:00".to_owned(), "13:00 +02:00".to_owned()],
            "[\"11:30 +02:00\", \"13:00 +02:00\"]",
        );
    }

    #[test]
    fn test_canonicalize_path() {
        let tmp = Builder::new()
            .prefix("test-canonicalize")
            .tempdir()
            .unwrap();
        let tmp_dir = tmp.path();

        let res_path1 = canonicalize_sub_path(tmp_dir.to_str().unwrap(), "test1.dump").unwrap();
        assert!(!Path::new(&res_path1).exists());
        assert_eq!(
            Path::new(&res_path1),
            tmp_dir.canonicalize().unwrap().join("test1.dump")
        );

        let cases = vec![".", "/../../", "./../"];
        for case in &cases {
            assert_eq!(
                Path::new(&canonicalize_non_existing_path(case).unwrap()),
                Path::new(case).canonicalize().unwrap(),
            );
        }

        // canonicalize a path containing symlink and non-existing nodes
        ensure_dir_exist(&format!("{}", tmp_dir.to_path_buf().join("dir").display())).unwrap();
        let nodes: &[&str] = if cfg!(target_os = "linux") {
            std::os::unix::fs::symlink(
                tmp_dir.to_path_buf().join("dir"),
                tmp_dir.to_path_buf().join("symlink"),
            )
            .unwrap();
            &["non_existing", "dir", "symlink"]
        } else {
            &["non_existing", "dir"]
        };
        for first in nodes {
            for second in nodes {
                let base_path = format!("{}/{}/..", tmp_dir.to_str().unwrap(), first,);
                let sub_path = format!("{}/non_existing", second);
                let full_path = format!("{}/{}", &base_path, &sub_path);
                let res_path1 = canonicalize_path(&full_path).unwrap();
                let res_path2 = canonicalize_sub_path(&base_path, &sub_path).unwrap();
                assert_eq!(Path::new(&res_path1), Path::new(&res_path2));
                // resolve to second/non_existing
                if *second == "non_existing" {
                    assert_eq!(
                        Path::new(&res_path1),
                        tmp_dir.to_path_buf().join("non_existing/non_existing")
                    );
                } else {
                    assert_eq!(
                        Path::new(&res_path1),
                        tmp_dir.to_path_buf().join("dir/non_existing")
                    );
                }
            }
        }

        // canonicalize a file
        let path2 = format!("{}", tmp_dir.to_path_buf().join("test2.dump").display());
        {
            File::create(&path2).unwrap();
        }
        canonicalize_path(&path2).unwrap_err();
        assert!(Path::new(&path2).exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_check_kernel() {
        use super::check_kernel::{Checker, check_kernel_params};

        // The range of vm.swappiness is from 0 to 100.
        let table: Vec<(&str, i64, Box<Checker>, bool)> = vec![
            (
                "/proc/sys/vm/swappiness",
                i64::MAX,
                Box::new(|got, expect| got == expect),
                false,
            ),
            (
                "/proc/sys/vm/swappiness",
                i64::MAX,
                Box::new(|got, expect| got < expect),
                true,
            ),
        ];

        for (path, expect, checker, is_ok) in table {
            assert_eq!(check_kernel_params(path, expect, checker).is_ok(), is_ok);
        }
    }

    #[test]
    fn test_check_addrs() {
        let table = vec![
            ("127.0.0.1:8080", true),
            ("[::1]:8080", true),
            ("localhost:8080", true),
            ("pingcap.com:8080", true),
            ("funnydomain:8080", true),
            ("127.0.0.1", false),
            ("[::1]", false),
            ("localhost", false),
            ("pingcap.com", false),
            ("funnydomain", false),
            ("funnydomain:", false),
            ("root@google.com:8080", false),
            ("http://google.com:8080", false),
            ("google.com:8080/path", false),
            ("http://google.com:8080/path", false),
            ("http://google.com:8080/path?lang=en", false),
            ("http://google.com:8080/path?lang=en#top", false),
            ("ftp://ftp.is.co.za/rfc/rfc1808.txt", false),
            ("http://www.ietf.org/rfc/rfc2396.txt", false),
            ("ldap://[2001:db8::7]/c=GB?objectClass?one", false),
            ("mailto:John.Doe@example.com", false),
            ("news:comp.infosystems.www.servers.unix", false),
            ("tel:+1-816-555-1212", false),
            ("telnet://192.0.2.16:80/", false),
            ("urn:oasis:names:specification:docbook:dtd:xml:4.1.2", false),
            (":8080", false),
            ("8080", false),
            ("8080:", false),
        ];

        for (addr, is_ok) in table {
            assert_eq!(check_addr(addr).is_ok(), is_ok);
        }

        let table = vec![
            ("0.0.0.0:8080", true),
            ("[::0]:8080", true),
            ("127.0.0.1:8080", false),
            ("[::1]:8080", false),
            ("localhost:8080", false),
            ("pingcap.com:8080", false),
            ("funnydomain:8080", false),
        ];

        for (addr, is_unspecified) in table {
            assert_eq!(check_addr(addr).unwrap(), is_unspecified);
        }
    }

    fn create_file(fpath: &str, buf: &[u8]) {
        let mut file = File::create(fpath).unwrap();
        file.write_all(buf).unwrap();
        file.flush().unwrap();
    }

    #[test]
    fn test_get_file_count() {
        let tmp_path = Builder::new()
            .prefix("test-get-file-count")
            .tempdir()
            .unwrap()
            .into_path();
        let count = get_file_count(tmp_path.to_str().unwrap(), "txt").unwrap();
        assert_eq!(count, 0);
        let tmp_file = format!("{}", tmp_path.join("test-get-file-count.txt").display());
        create_file(&tmp_file, b"");
        let count = get_file_count(tmp_path.to_str().unwrap(), "").unwrap();
        assert_eq!(count, 1);
        let count = get_file_count(tmp_path.to_str().unwrap(), "txt").unwrap();
        assert_eq!(count, 1);
        let count = get_file_count(tmp_path.to_str().unwrap(), "xt").unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn test_check_data_dir_empty() {
        // test invalid data_path
        check_data_dir_empty("/sys/invalid", "txt").unwrap();
        // test empty data_path
        let tmp_path = Builder::new()
            .prefix("test-get-file-count")
            .tempdir()
            .unwrap()
            .into_path();
        check_data_dir_empty(tmp_path.to_str().unwrap(), "txt").unwrap();
        // test non-empty data_path
        let tmp_file = format!("{}", tmp_path.join("test-get-file-count.txt").display());
        create_file(&tmp_file, b"");
        check_data_dir_empty(tmp_path.to_str().unwrap(), "").unwrap_err();
        check_data_dir_empty(tmp_path.to_str().unwrap(), "txt").unwrap_err();
        check_data_dir_empty(tmp_path.to_str().unwrap(), "xt").unwrap();
    }

    #[test]
    fn test_multi_tracker() {
        use std::sync::Arc;

        use super::*;

        #[derive(Debug, Default, PartialEq)]
        struct Value {
            v1: u64,
            v2: bool,
        }

        let count = 10;
        let vc = Arc::new(VersionTrack::new(Value::default()));
        let mut trackers = Vec::with_capacity(count);
        for _ in 0..count {
            trackers.push(vc.clone().tracker("test-tracker".to_owned()));
        }

        assert!(trackers.iter_mut().all(|tr| tr.any_new().is_none()));

        let _ = vc.update(|v| -> Result<(), ()> {
            v.v1 = 1000;
            v.v2 = true;
            Ok(())
        });
        for tr in trackers.iter_mut() {
            let incoming = tr.any_new();
            assert!(incoming.is_some());
            let incoming = incoming.unwrap();
            assert_eq!(incoming.v1, 1000);
            assert_eq!(incoming.v2, true);
        }

        assert!(trackers.iter_mut().all(|tr| tr.any_new().is_none()));
    }

    #[test]
    fn test_toml_writer() {
        let cfg = r#"
## commet1
log-level = "info"

[readpool.storage]
## commet2
normal-concurrency = 1
# high-concurrency = 1

## commet3
[readpool.coprocessor]
normal-concurrency = 1

[rocksdb.defaultcf]
compression-per-level = ["no", "no", "no", "no", "no", "no", "no"]

"#;
        let mut m = HashMap::new();
        m.insert("log-file".to_owned(), "log-file-name".to_owned());
        m.insert("readpool.storage.xxx".to_owned(), "zzz".to_owned());
        m.insert(
            "readpool.storage.high-concurrency".to_owned(),
            "345".to_owned(),
        );
        m.insert(
            "readpool.coprocessor.normal-concurrency".to_owned(),
            "123".to_owned(),
        );
        m.insert("not-in-file-config1.xxx.yyy".to_owned(), "100".to_owned());
        m.insert(
            "rocksdb.defaultcf.compression-per-level".to_owned(),
            "[\"no\", \"no\", \"lz4\", \"lz4\", \"lz4\", \"zstd\", \"zstd\"]".to_owned(),
        );

        let mut t = TomlWriter::new();
        t.write_change(cfg.to_owned(), m);
        let expect = r#"
## commet1
log-level = "info"

log-file = log-file-name
[readpool.storage]
## commet2
normal-concurrency = 1
high-concurrency = 345

## commet3
xxx = zzz
[readpool.coprocessor]
normal-concurrency = 123

[rocksdb.defaultcf]
compression-per-level = ["no", "no", "lz4", "lz4", "lz4", "zstd", "zstd"]

[not-in-file-config1.xxx]
yyy = 100

"#;
        assert_eq!(expect.as_bytes(), t.finish().as_slice());
    }

    #[test]
    fn test_update_empty_content() {
        // empty content
        let mut src = "".to_owned();

        src = {
            let mut m = HashMap::new();
            m.insert(
                "readpool.storage.high-concurrency".to_owned(),
                "1".to_owned(),
            );
            let mut t = TomlWriter::new();
            t.write_change(src.clone(), m);
            String::from_utf8_lossy(t.finish().as_slice()).to_string()
        };
        // src should have valid toml format
        let toml_value: toml::Value = toml::from_str(src.as_str()).unwrap();
        assert_eq!(
            toml_value["readpool"]["storage"]["high-concurrency"].as_integer(),
            Some(1)
        );

        src = {
            let mut m = HashMap::new();
            m.insert(
                "readpool.storage.normal-concurrency".to_owned(),
                "2".to_owned(),
            );
            let mut t = TomlWriter::new();
            t.write_change(src.clone(), m);
            String::from_utf8_lossy(t.finish().as_slice()).to_string()
        };
        // src should have valid toml format
        let toml_value: toml::Value = toml::from_str(src.as_str()).unwrap();
        assert_eq!(
            toml_value["readpool"]["storage"]["normal-concurrency"].as_integer(),
            Some(2)
        );
    }

    #[test]
    fn test_raft_engine_switch() {
        // default setting, raft-db and raft-engine are not in the same place, need
        // dump raft data from raft-db
        let dir = tempfile::Builder::new().tempdir().unwrap();
        let root = dir.path().join("root");
        let source = root.join("source");
        fs::create_dir_all(&source).unwrap();
        let raftdb_data = source.join("CURRENT");
        fs::File::create(raftdb_data).unwrap();
        let target = root.join("target");
        fs::create_dir_all(&target).unwrap();
        let mut state = RaftDataStateMachine::new(
            root.to_str().unwrap(),
            source.to_str().unwrap(),
            target.to_str().unwrap(),
        );
        state.validate(true).unwrap();
        let should_dump = state.before_open_target();
        assert!(should_dump);
        fs::remove_dir_all(&root).unwrap();

        // raft-db is eventually moved, can't dump from raft-db
        let dir = tempfile::Builder::new().tempdir().unwrap();
        let root = dir.path().join("root");
        let source = root.join("source");
        fs::create_dir_all(&source).unwrap();
        let target = root.join("target");
        fs::create_dir_all(&target).unwrap();
        state = RaftDataStateMachine::new(
            root.to_str().unwrap(),
            source.to_str().unwrap(),
            target.to_str().unwrap(),
        );
        state.validate(true).unwrap_err();
        fs::remove_dir_all(&root).unwrap();

        // when setting raft-db dir, raft-engine dir is not set, raft-engine dir
        // inherit from raft-db dir, need to dump raft data from raft-db
        let dir = tempfile::Builder::new().tempdir().unwrap();
        let root = dir.path().join("root");
        let source = root.join("source");
        fs::create_dir_all(&source).unwrap();
        let raftdb_data = source.join("CURRENT");
        fs::File::create(raftdb_data).unwrap();
        let target = source.join("target");
        fs::create_dir_all(&target).unwrap();
        state = RaftDataStateMachine::new(
            root.to_str().unwrap(),
            source.to_str().unwrap(),
            target.to_str().unwrap(),
        );
        state.validate(true).unwrap();
        let should_dump = state.before_open_target();
        assert!(should_dump);
        fs::remove_dir_all(&root).unwrap();

        // inherit scenario raft-db is eventually moved, can't dump from raft-db
        let dir = tempfile::Builder::new().tempdir().unwrap();
        let root = dir.path().join("root");
        let source = root.join("source");
        fs::create_dir_all(&source).unwrap();
        let target = source.join("target");
        fs::create_dir_all(&target).unwrap();
        state = RaftDataStateMachine::new(
            root.to_str().unwrap(),
            source.to_str().unwrap(),
            target.to_str().unwrap(),
        );
        state.validate(true).unwrap_err();
        fs::remove_dir_all(&root).unwrap();

        // raft-db dump from raft-engine
        let dir = tempfile::Builder::new().tempdir().unwrap();
        let root = dir.path().join("root");
        let source = root.join("source");
        fs::create_dir_all(&source).unwrap();
        let raftdb_data = source.join("CURRENT");
        fs::File::create(raftdb_data).unwrap();
        let target = source.join("target");
        fs::create_dir_all(&target).unwrap();
        let mut state = RaftDataStateMachine::new(
            root.to_str().unwrap(),
            source.to_str().unwrap(),
            target.to_str().unwrap(),
        );
        state.validate(true).unwrap();
        let should_dump = state.before_open_target();
        assert!(should_dump);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn test_raft_data_migration() {
        fn run_migration<F: Fn()>(root: &Path, source: &Path, target: &Path, check: F) {
            let mut state = RaftDataStateMachine::new(
                root.to_str().unwrap(),
                source.to_str().unwrap(),
                target.to_str().unwrap(),
            );
            state.validate(true).unwrap();
            check();
            // Dump to target.
            if state.before_open_target() {
                check();
                // Simulate partial writes.
                let marker = root.join("MIGRATING-RAFT");
                if marker.exists() {
                    let backup_marker = fs::read_to_string(&marker).unwrap();
                    fs::write(&marker, "").unwrap();
                    check();
                    fs::write(&marker, backup_marker).unwrap();
                }

                let mut source_file = source.join("CURRENT");
                let target_file = target.join("0000000000000001.raftlog");
                if !target.exists() {
                    fs::create_dir_all(target).unwrap();
                    check();
                }
                if !source_file.exists() {
                    source_file = source.join("0000000000000001.raftlog");
                }
                fs::copy(source_file, target_file).unwrap();
                check();
                state.after_dump_data_with_check(&check);
            }
            check();
        }

        fn copy_dir(src: &Path, dst: &Path) -> std::io::Result<()> {
            if dst.exists() {
                fs::remove_dir_all(dst)?;
            }
            fs::create_dir_all(dst)?;
            for entry in fs::read_dir(src)? {
                let entry = entry?;
                let ty = entry.file_type()?;
                if ty.is_dir() {
                    copy_dir(&entry.path(), &dst.join(entry.file_name()))?;
                } else {
                    fs::copy(entry.path(), dst.join(entry.file_name()))?;
                }
            }
            Ok(())
        }

        let dir = tempfile::Builder::new().tempdir().unwrap();
        let root = dir.path().join("root");
        let source = root.join("source");
        fs::create_dir_all(&source).unwrap();
        let target = root.join("target");
        fs::create_dir_all(&target).unwrap();
        // Write some data into source.
        let source_file = source.join("CURRENT");
        File::create(source_file).unwrap();

        let backup = dir.path().join("backup");

        run_migration(&root, &source, &target, || {
            copy_dir(&root, &backup).unwrap();

            // Simulate restart and migrate in halfway.
            run_migration(&root, &source, &target, || {});
            copy_dir(&backup, &root).unwrap();
            //
            run_migration(&root, &target, &source, || {});
            copy_dir(&backup, &root).unwrap();
        });
    }

    #[test]
    fn test_must_remove_except() {
        fn create_raftdb(path: &Path) {
            fs::create_dir(path).unwrap();
            // CURRENT file as the marker of raftdb.
            let raftdb_data = path.join("CURRENT");
            fs::File::create(raftdb_data).unwrap();
        }

        fn create_raftengine(path: &Path) {
            fs::create_dir(path).unwrap();
            let raftengine_data = path.join("raftengine_data");
            fs::File::create(raftengine_data).unwrap();
        }

        fn create_test_root(path: &Path) {
            fs::create_dir(path).unwrap();
        }

        fn raftengine_must_exist(path: &Path) {
            assert!(path.exists());
            let raftengine_data = path.join("raftengine_data");
            assert!(raftengine_data.exists());
        }

        fn raftdb_must_not_exist(path: &Path) {
            assert!(!path.exists());
            let raftdb_data = path.join("raftdb_data");
            assert!(!raftdb_data.exists());
        }
        let test_dir = tempfile::Builder::new()
            .tempdir()
            .unwrap()
            .into_path()
            .join("test_must_remove_except");

        // before:
        // test_must_remove_except
        // ├── raftdb
        // │   └── raftdb_data
        // └── raftengine
        //     └── raftengine_data
        //
        // after:
        // test_must_remove_except
        // └── raftengine
        //     └── raftengine_data
        create_test_root(&test_dir);
        let raftdb_dir = test_dir.join("raftdb");
        let raftengine_dir = test_dir.join("raftengine");
        create_raftdb(&raftdb_dir);
        create_raftengine(&raftengine_dir);
        RaftDataStateMachine::must_remove_except(&raftdb_dir, &raftengine_dir);
        raftengine_must_exist(&raftengine_dir);
        raftdb_must_not_exist(&raftdb_dir);
        fs::remove_dir_all(&test_dir).unwrap();

        // before:
        // test_must_remove_except/
        // └── raftdb
        //     ├── raftdb_data
        //     └── raftengine
        //         └── raftengine_data
        //
        // after:
        // test_must_remove_except/
        // └── raftdb
        //     └── raftengine
        //         └── raftengine_data
        create_test_root(&test_dir);
        let raftdb_dir = test_dir.join("raftdb");
        let raftengine_dir = raftdb_dir.join("raftengine");
        create_raftdb(&raftdb_dir);
        create_raftengine(&raftengine_dir);
        RaftDataStateMachine::must_remove_except(&raftdb_dir, &raftengine_dir);
        raftengine_must_exist(&raftengine_dir);
        assert!(!test_dir.join("raftdb/raftdb_data").exists());
        fs::remove_dir_all(&test_dir).unwrap();

        // before:
        // test_must_remove_except/
        // └── raftengine
        //     ├── raftdb
        //     │   └── raftdb_data
        //     └── raftengine_data
        //
        // after:
        // test_must_remove_except/
        // └── raftengine
        //     └── raftengine_data
        create_test_root(&test_dir);
        let raftengine_dir = test_dir.join("raftengine");
        let raftdb_dir = raftengine_dir.join("raftdb");
        create_raftengine(&raftengine_dir);
        create_raftdb(&raftdb_dir);
        RaftDataStateMachine::must_remove_except(&raftdb_dir, &raftengine_dir);
        raftengine_must_exist(&raftengine_dir);
        raftdb_must_not_exist(&raftdb_dir);
        fs::remove_dir_all(&test_dir).unwrap();

        // before:
        // test_must_remove_except/
        // ├── raftdb_data
        // └── raftengine
        //     └── raftengine_data
        //
        // after:
        // test_must_remove_except/
        // └── raftengine
        //     └── raftengine_data
        create_test_root(&test_dir);
        let raftdb_data = test_dir.join("raftdb_data");
        fs::File::create(raftdb_data).unwrap();
        let raftengine_dir = test_dir.join("raftengine");
        create_raftengine(&raftengine_dir);
        RaftDataStateMachine::must_remove_except(&test_dir, &raftengine_dir);
        raftengine_must_exist(&raftengine_dir);
        assert!(!test_dir.join("raftdb_data").exists());
        fs::remove_dir_all(&test_dir).unwrap();
    }

    #[test]
    fn test_raft_data_exist() {
        fn clear_dir(path: &PathBuf) {
            if path.exists() {
                fs::remove_dir_all(path).unwrap();
            }
            fs::create_dir(path).unwrap();
        }
        let test_dir = tempfile::Builder::new().tempdir().unwrap().into_path();

        clear_dir(&test_dir);
        fs::File::create(test_dir.join("0000000000000001.raftlog")).unwrap();
        assert!(RaftDataStateMachine::raftengine_exists(&test_dir));

        clear_dir(&test_dir);
        fs::File::create(test_dir.join("0000000000000001.raftlog")).unwrap();
        fs::File::create(test_dir.join("trash")).unwrap();
        assert!(RaftDataStateMachine::raftengine_exists(&test_dir));

        clear_dir(&test_dir);
        fs::File::create(test_dir.join("raftlog")).unwrap();
        assert!(!RaftDataStateMachine::raftengine_exists(&test_dir));

        clear_dir(&test_dir);
        assert!(!RaftDataStateMachine::raftengine_exists(&test_dir));

        clear_dir(&test_dir);
        fs::File::create(test_dir.join("CURRENT")).unwrap();
        assert!(RaftDataStateMachine::raftdb_exists(&test_dir));

        clear_dir(&test_dir);
        fs::File::create(test_dir.join("NOT_CURRENT")).unwrap();
        assert!(!RaftDataStateMachine::raftdb_exists(&test_dir));

        clear_dir(&test_dir);
        assert!(!RaftDataStateMachine::raftdb_exists(&test_dir));
    }
}
