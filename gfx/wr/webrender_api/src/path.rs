/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use crate::serde::{Serialize, Deserialize};
use crate::units::*;
use std::sync::Arc;

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
#[derive(Serialize, Deserialize)]
pub enum FillRule {
    EventOdd,
    NonZero,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[derive(Serialize, Deserialize)]
pub(crate) enum Verb {
    LineTo,
    QuadraticTo,
    CubicTo,
    Begin,
    Close,
    End,
}

#[derive(Clone)]
pub struct Path {
    points: Arc<[LayoutPoint]>,
    verbs: Arc<[Verb]>,
}

impl Path {
    pub fn empty() -> Self {
        Path {
            points: Arc::new([]),
            verbs: Arc::new([]),
        }
    }

    pub fn iter(&self) -> PathIter {
        PathIter {
            points: self.points.iter(),
            verbs: self.verbs.iter(),
            current: LayoutPoint::zero(),
            first: LayoutPoint::zero(),
        }
    }
}

pub struct PathBuilder {
    points: Vec<LayoutPoint>,
    verbs: Vec<Verb>,
    validator: DebugValidator,
}

impl PathBuilder {
    pub fn new() -> Self {
        PathBuilder {
            points: Vec::new(),
            verbs: Vec::new(),
            validator: DebugValidator::new(),
        }
    }

    pub fn begin(&mut self, at: LayoutPoint) {
        self.validator.begin();
        self.points.push(at);
        self.verbs.push(Verb::Begin);
    }

    pub fn end(&mut self, close: bool) {
        self.validator.end();
        self.verbs.push(if close { Verb::Close } else { Verb::End });
    }

    pub fn line_to(&mut self, to: LayoutPoint) {
        self.validator.edge();
        self.points.push(to);
        self.verbs.push(Verb::LineTo);
    }

    pub fn quadratic_bezier_to(&mut self, ctrl: LayoutPoint, to: LayoutPoint) {
        self.validator.edge();
        self.points.push(ctrl);
        self.points.push(to);
        self.verbs.push(Verb::QuadraticTo);
    }

    pub fn cubic_bezier_to(&mut self, ctrl1: LayoutPoint, ctrl2: LayoutPoint, to: LayoutPoint) {
        self.validator.edge();
        self.points.push(ctrl1);
        self.points.push(ctrl2);
        self.points.push(to);
        self.verbs.push(Verb::QuadraticTo);
    }

    pub fn build(&mut self) -> Path {
        self.validator.finish();
        let path = Path {
            points: self.points.as_slice().into(),
            verbs: self.verbs.as_slice().into(),
        };

        self.points.clear();
        self.verbs.clear();
        self.validator = DebugValidator::new();

        path
    }
}

#[derive(Copy, Clone, Debug)]
pub enum PathEvent {
    Begin { at: LayoutPoint },
    End { last: LayoutPoint, first: LayoutPoint, close: bool },
    Line { from: LayoutPoint, to: LayoutPoint },
    Quadratic { from: LayoutPoint, ctrl: LayoutPoint, to: LayoutPoint },
    Cubic { from: LayoutPoint, ctrl1: LayoutPoint, ctrl2: LayoutPoint, to: LayoutPoint },
}

#[derive(Clone)]
pub struct PathIter<'l> {
    points: std::slice::Iter<'l, LayoutPoint>,
    verbs: std::slice::Iter<'l, Verb>,
    current: LayoutPoint,
    first: LayoutPoint,
}

impl<'l> Iterator for PathIter<'l> {
    type Item = PathEvent;

    fn next(&mut self) -> Option<PathEvent> {
        match self.verbs.next() {
            Some(&Verb::Begin) => {
                self.current = *self.points.next()?;
                self.first = self.current;
                Some(PathEvent::Begin { at: self.current })
            }
            Some(&Verb::LineTo) => {
                let from = self.current;
                self.current = *self.points.next()?;
                Some(PathEvent::Line {
                    from,
                    to: self.current,
                })
            }
            Some(&Verb::QuadraticTo) => {
                let from = self.current;
                let ctrl = *self.points.next()?;
                self.current = *self.points.next()?;
                Some(PathEvent::Quadratic {
                    from,
                    ctrl,
                    to: self.current,
                })
            }
            Some(&Verb::CubicTo) => {
                let from = self.current;
                let ctrl1 = *self.points.next()?;
                let ctrl2 = *self.points.next()?;
                self.current = *self.points.next()?;
                Some(PathEvent::Cubic {
                    from,
                    ctrl1,
                    ctrl2,
                    to: self.current,
                })
            }
            Some(&Verb::Close) => {
                let last = self.current;
                Some(PathEvent::End {
                    last,
                    first: self.first,
                    close: true,
                })
            }
            Some(&Verb::End) => {
                let last = self.current;
                self.current = self.first;
                Some(PathEvent::End {
                    last,
                    first: self.first,
                    close: false,
                })
            }
            None => None,
        }
    }
}



#[derive(Default, Copy, Clone, Debug, PartialEq)]
struct DebugValidator {
    #[cfg(debug_assertions)]
    in_subpath: bool,
}

impl DebugValidator {
    #[inline(always)]
    pub fn new() -> Self {
        Self::default()
    }

    #[inline(always)]
    fn begin(&mut self) {
        #[cfg(debug_assertions)]
        {
            assert!(!self.in_subpath, "multiple begin() calls without end()");
            self.in_subpath = true;
        }
    }

    #[inline(always)]
    fn end(&mut self) {
        #[cfg(debug_assertions)]
        {
            assert!(self.in_subpath, "end() called without begin()");
            self.in_subpath = false;
        }
    }

    #[inline(always)]
    fn edge(&self) {
        #[cfg(debug_assertions)]
        assert!(self.in_subpath, "edge operation is made before begin()");
    }

    #[inline(always)]
    fn finish(&self) {
        #[cfg(debug_assertions)]
        assert!(!self.in_subpath, "build() called before end()");
    }
}
