use chrono::{NaiveDate, NaiveDateTime, NaiveTime, TimeZone, UTC};
use chrono::prelude::*;
use errors::*;
use futures::{Poll, Stream};
use serde::de::{Deserialize, Deserializer, Error as DeserializeError, SeqVisitor, Visitor};
use std::iter::Cloned;
use std::ops::Range;
use std::slice;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use util::{CarryingUpIterator, MergedIterator};

pub struct Schedule<'a, Tz: 'a + TimeZone> {
    upcoming: MergedIterator<Iter<'a>>,
    next: Option<DateTime<Tz>>,
    waiting: Arc<AtomicBool>,
    tz: &'a Tz,
}

#[derive(Debug, PartialEq, Eq)]
pub struct UnitSchedule {
    wdays: Vec<Wday>,
    hours: Vec<Hour>,
    mins: Vec<Min>,
}

pub struct Iter<'a> {
    inner: CarryingUpIterator<
        Cloned<slice::Iter<'a, Wday>>, Cloned<slice::Iter<'a, Hour>>, Cloned<slice::Iter<'a, Min>>
    >,
    date: NaiveDate,
}

type Wday = u32;
type Hour = u32;
type Min = u32;

impl<'a, Tz: 'a + TimeZone> Schedule<'a, Tz> {
    pub fn new<S>(sched: S, time_zone: &'a Tz) -> Self where S: IntoIterator<Item = &'a UnitSchedule> {
        let now = time_zone.from_utc_datetime(&UTC::now().naive_utc()).naive_local();

        Schedule {
            upcoming: MergedIterator::new(sched.into_iter().map(|us| us.iter_since(now))),
            next: None,
            waiting: Arc::new(AtomicBool::new(false)),
            tz: time_zone,
        }
    }

    fn next(&mut self, now: &DateTime<Tz>) -> DateTime<Tz> {
        let tz = self.tz;
        self.upcoming.by_ref()
            .filter_map(|tm| tz.from_local_datetime(&tm).latest())
            .find(|tm| tm > now)
            .unwrap()
    }

    fn set_timer(&mut self, dur: Duration) {
        use futures::task;
        use std::thread;

        let task = task::park();
        let waiting = self.waiting.clone();
        waiting.store(true, Ordering::Relaxed);

        thread::spawn(move || {
            thread::sleep(dur);
            task.unpark();
            waiting.store(false, Ordering::Release);
        });
    }
}

impl<'a, Tz: TimeZone> Stream for Schedule<'a, Tz> {
    type Item = ();
    type Error = Error;

    fn poll(&mut self) -> Poll<Option<()>, Error> {
        use futures::Async::*;

        let now = self.tz.from_utc_datetime(&UTC::now().naive_utc());

        let next = if let Some(ref next) = self.next {
            next.clone()
        } else {
            let next = self.next(&now);
            self.next = Some(next.clone());
            next
        };

        if let Ok(dur) = next.signed_duration_since(now.clone()).to_std() {
            if !self.waiting.load(Ordering::Acquire) {
                self.set_timer(dur);
            }
            Ok(NotReady)
        } else {
            let next = self.next(&now);
            self.next = Some(next.clone());
            self.set_timer(next.signed_duration_since(now).to_std().unwrap());
            Ok(Ready(Some(())))
        }
    }
}

impl UnitSchedule {
    pub fn new(mut wdays: Vec<Wday>, mut hours: Vec<Hour>, mut mins: Vec<Min>) -> Option<Self> {
        macro_rules! regularize {
            ($vec:ident, $range:expr) => {
                if $vec.is_empty() {
                    $vec.extend($range);
                } else {
                    $vec.sort();
                    $vec.dedup();
                    // `$vec` is monotonically increasing now.
                    if *$vec.first().unwrap() < $range.start || $range.end <= *$vec.last().unwrap() // Out of range
                    {
                        return None;
                    }
                }
            }
        }

        regularize!(wdays, 0..7);
        regularize!(hours, 0..24);
        regularize!(mins, 0..60);

        Some(UnitSchedule {
            wdays: wdays,
            hours: hours,
            mins: mins,
        })
    }

    pub fn iter_since(&self, since: NaiveDateTime) -> Iter {
        let mut since_date = since.date();
        let mut since_time = since.time();

        if (*self.hours.last().unwrap(), *self.mins.last().unwrap()) <= (since.hour(), since.minute()) {
            since_date = since_date.succ();
            since_time = NaiveTime::from_hms(0, 0, 0);
        }

        while !self.wdays.contains(&since_date.weekday().num_days_from_sunday()) {
            since_date = since_date.succ();
            since_time = NaiveTime::from_hms(0, 0, 0);
        }

        let mut inner = CarryingUpIterator::new(
            self.wdays.iter().cloned(), self.hours.iter().cloned(), self.mins.iter().cloned()
        ).unwrap();

        // Proceed `inner`'s iteration state to the first point after `since`:

        let since_whm = (since_date.weekday().num_days_from_sunday(), since_time.hour(), since_time.minute());
        let mut before_start = (*self.wdays.last().unwrap(), *self.hours.last().unwrap(), *self.mins.last().unwrap());

        for whm in inner.by_ref() {
            if whm > since_whm {
                break;
            }
            before_start = whm;
        }

        for whm in inner.by_ref() {
            if whm == before_start {
                break;
            }
        }

        Iter {
            inner: inner,
            date: since_date,
        }
    }
}

impl Deserialize for UnitSchedule {
    fn deserialize<D: Deserializer>(d: D) -> ::std::result::Result<Self, D::Error> {
        use std::result::Result;

        fn deserialize_nums<D: Deserializer>(d: D) -> Result<Vec<u32>, D::Error> {
            use std::fmt;
            use std::u32;

            struct NumsVisitor;

            macro_rules! visit_as_u32 {
                ($name:ident, $t:ty, $u:ty, $map:expr) => {
                    #[allow(unused_comparisons)]
                    fn $name<E: DeserializeError>(self, n: $t) -> Result<$u, E> {
                        if 0 <= n && n as u64 <= ::std::u32::MAX as u64 {
                            Ok($map(n as u32))
                        } else {
                            Err(E::custom(format!("u32 out of range: {}", n)))
                        }
                    }
                }
            }

            impl Visitor for NumsVisitor {
                type Value = Vec<u32>;

                fn visit_seq<V: SeqVisitor>(self, mut v: V) -> Result<Vec<u32>, V::Error> {
                    enum NumOrRange {
                        Num(u32),
                        Range(Range<u32>),
                    }

                    impl Deserialize for NumOrRange {
                        fn deserialize<D: Deserializer>(d: D) -> Result<Self, D::Error> {
                            struct NumRangeVisitor;

                            impl Visitor for NumRangeVisitor {
                                type Value = NumOrRange;

                                fn visit_str<E: DeserializeError>(self, s: &str) -> Result<NumOrRange, E> {
                                    parse_range(s).map(NumOrRange::Range).map_err(|_| E::custom("expected a number"))
                                }

                                fn visit_string<E: DeserializeError>(self, s: String) -> Result<NumOrRange, E> {
                                    self.visit_str(&s)
                                }

                                fn visit_u32<E>(self, n: u32) -> Result<NumOrRange, E> {
                                    Ok(NumOrRange::Num(n))
                                }

                                visit_as_u32!(visit_u64, u64, NumOrRange, NumOrRange::Num);
                                visit_as_u32!(visit_u16, u16, NumOrRange, NumOrRange::Num);
                                visit_as_u32!(visit_u8, u8, NumOrRange, NumOrRange::Num);
                                visit_as_u32!(visit_i64, i64, NumOrRange, NumOrRange::Num);
                                visit_as_u32!(visit_i32, i32, NumOrRange, NumOrRange::Num);
                                visit_as_u32!(visit_i16, i16, NumOrRange, NumOrRange::Num);
                                visit_as_u32!(visit_i8, i8, NumOrRange, NumOrRange::Num);

                                fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                                    write!(f, "an unsigned integer or a string")
                                }
                            }

                            d.deserialize_u32(NumRangeVisitor)
                        }
                    }

                    let mut ret = Vec::with_capacity(v.size_hint().0);

                    while let Some(num_or_range) = v.visit()? {
                        match num_or_range {
                            NumOrRange::Num(n) => ret.push(n),
                            NumOrRange::Range(r) => ret.extend(r),
                        }
                    }

                    Ok(ret)
                }

                fn visit_str<E: DeserializeError>(self, s: &str) -> Result<Vec<u32>, E> {
                    parse_range(s).map(Iterator::collect).map_err(|_| E::custom("expected a pair of numbers"))
                }

                fn visit_string<E: DeserializeError>(self, s: String) -> Result<Vec<u32>, E> {
                    self.visit_str(&s)
                }

                fn visit_u32<E>(self, n: u32) -> Result<Vec<u32>, E> {
                    Ok(vec![n])
                }

                visit_as_u32!(visit_u64, u64, Vec<u32>, |n| vec![n]);
                visit_as_u32!(visit_u16, u16, Vec<u32>, |n| vec![n]);
                visit_as_u32!(visit_u8, u8, Vec<u32>, |n| vec![n]);
                visit_as_u32!(visit_i64, i64, Vec<u32>, |n| vec![n]);
                visit_as_u32!(visit_i32, i32, Vec<u32>, |n| vec![n]);
                visit_as_u32!(visit_i16, i16, Vec<u32>, |n| vec![n]);
                visit_as_u32!(visit_i8, i8, Vec<u32>, |n| vec![n]);

                fn visit_unit<E>(self) -> Result<Vec<u32>, E> {
                    Ok(Vec::new())
                }

                fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                    write!(f, "an unsigned integer, an array of unsigned integers or a string")
                }
            }

            d.deserialize_seq(NumsVisitor)
        }

        #[derive(Deserialize)]
        struct Unit(
            #[serde(deserialize_with = "deserialize_nums")] Vec<Wday>,
            #[serde(deserialize_with = "deserialize_nums")] Vec<Hour>,
            #[serde(deserialize_with = "deserialize_nums")] Vec<Min>,
        );

        let ret = Unit::deserialize(d)?;

        UnitSchedule::new(ret.0, ret.1, ret.2)
            .ok_or_else(|| D::Error::custom("invalid number"))
    }
}

impl<'a> Iterator for Iter<'a> {
    type Item = NaiveDateTime;

    fn next(&mut self) -> Option<NaiveDateTime> {
        let (wday, hr, min) = self.inner.next().unwrap();

        while self.date.weekday().num_days_from_sunday() != wday {
            self.date = self.date.succ();
        }

        Some(self.date.and_hms(hr, min, 0))
    }
}

/// Parses a string representing a range. e.g. "1-5" = [1, 5] (inclusive).
fn parse_range(s: &str) -> ::std::result::Result<Range<u32>, ()> {
    if let Some(i) = s.find('-') {
        let (s, e) = s.split_at(i);
        let e = &e[1..];

        let s = s.parse().map_err(|_| ())?;
        let e = 1 + e.parse::<u32>().map_err(|_| ())?;

        Ok(s..e)
    } else {
        Err(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iter() {
        fn ymdhm(y: i32, mon: u32, d: u32, h: u32, min: u32) -> NaiveDateTime {
            NaiveDate::from_ymd(y, mon, d).and_hms(h, min, 0)
        }
        let us = UnitSchedule::new(vec![1, 0], vec![8, 12], vec![20, 40]).unwrap();

        let mut iter = us.iter_since(ymdhm(2017, 2, 13, 8, 20)); // wday == 1
        assert_eq!(Some(ymdhm(2017, 2, 13,  8, 40)), iter.next());
        assert_eq!(Some(ymdhm(2017, 2, 13, 12, 20)), iter.next());
        assert_eq!(Some(ymdhm(2017, 2, 13, 12, 40)), iter.next());
        assert_eq!(Some(ymdhm(2017, 2, 19,  8, 20)), iter.next());
        assert_eq!(Some(ymdhm(2017, 2, 19,  8, 40)), iter.next());
        assert_eq!(Some(ymdhm(2017, 2, 19, 12, 20)), iter.next());
        assert_eq!(Some(ymdhm(2017, 2, 19, 12, 40)), iter.next());
        assert_eq!(Some(ymdhm(2017, 2, 20,  8, 20)), iter.next());

        let mut iter = us.iter_since(ymdhm(2017, 2, 16, 13, 50)); // wday == 4
        assert_eq!(Some(ymdhm(2017, 2, 19, 8, 20)), iter.next());
        assert_eq!(Some(ymdhm(2017, 2, 19, 8, 40)), iter.next());

        let mut iter = us.iter_since(ymdhm(2017, 2, 28, 12, 0)); // wday == 2
        assert_eq!(Some(ymdhm(2017, 3, 5, 8, 20)), iter.next());
        assert_eq!(Some(ymdhm(2017, 3, 5, 8, 40)), iter.next());
    }

    #[test]
    fn new() {
        assert_eq!(
            UnitSchedule::new(vec![1, 4, 0, 6], vec![8, 11, 23, 0], vec![0, 59]).unwrap(),
            UnitSchedule::new(vec![0, 1, 4, 6], vec![0, 8, 11, 23], vec![0, 59]).unwrap()
        );

        assert_eq!(
            UnitSchedule::new(vec![                   ], vec![0], vec![0]).unwrap(),
            UnitSchedule::new(vec![0, 1, 2, 3, 4, 5, 6], vec![0], vec![0]).unwrap()
        );

        assert!(UnitSchedule::new(vec![7], vec![ 0], vec![ 0]).is_none());
        assert!(UnitSchedule::new(vec![0], vec![24], vec![ 0]).is_none());
        assert!(UnitSchedule::new(vec![0], vec![ 0], vec![60]).is_none());
    }

    #[test]
    fn range() {
        assert_eq!(Ok(1..7), parse_range("1-6"));
        assert_eq!(Ok(0..12), parse_range("0-11"));

        assert!(parse_range("").is_err());
        assert!(parse_range("0").is_err());
        assert!(parse_range("1-").is_err());
        assert!(parse_range("2- 3").is_err());
        assert!(parse_range("3-4-4").is_err());
    }
}
