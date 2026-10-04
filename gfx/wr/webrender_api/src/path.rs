/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use crate::serde::{Serialize, Deserialize};
use crate::units::*;
use malloc_size_of::{MallocSizeOf, MallocSizeOfOps};
use std::hash::{Hash, Hasher};
use std::sync::Arc;

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
#[derive(Serialize, Deserialize)]
pub(crate) enum Verb {
    LineTo,
    QuadraticTo,
    CubicTo,
    Begin,
    Close,
    End,
}

/// Vector path geometry.
///
/// Paths are interned by the display list builder, so equality and hashing
/// compare the coordinates bit for bit.
#[derive(Clone, Serialize, Deserialize)]
pub struct Path {
    points: Arc<[LayoutPoint]>,
    verbs: Arc<[Verb]>,
    /// Bounding box of the endpoints and control points. Derived from the
    /// points, so it does not take part in equality and hashing.
    aabb: LayoutRect,
}

impl PartialEq for Path {
    fn eq(&self, other: &Self) -> bool {
        if Arc::ptr_eq(&self.points, &other.points) && Arc::ptr_eq(&self.verbs, &other.verbs) {
            return true;
        }

        self.verbs == other.verbs
            && self.points.len() == other.points.len()
            && self.points.iter().zip(other.points.iter()).all(|(a, b)| {
                a.x.to_bits() == b.x.to_bits() && a.y.to_bits() == b.y.to_bits()
            })
    }
}

impl Eq for Path {}

impl Hash for Path {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.verbs.hash(state);
        self.points.len().hash(state);
        for point in self.points.iter() {
            point.x.to_bits().hash(state);
            point.y.to_bits().hash(state);
        }
    }
}

impl MallocSizeOf for Path {
    fn size_of(&self, _ops: &mut MallocSizeOfOps) -> usize {
        // The buffers are shared between the content interner and every store
        // that resolved the path, so they are not attributed to any one owner.
        0
    }
}

impl Path {
    pub fn empty() -> Self {
        Path {
            points: Arc::new([]),
            verbs: Arc::new([]),
            aabb: LayoutRect::zero(),
        }
    }

    /// A conservative bounding box of the path, containing its control points.
    ///
    /// Not finite if any of the path's points is not finite.
    pub fn aabb(&self) -> LayoutRect {
        self.aabb
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

impl std::fmt::Debug for Path {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        for item in self.iter() {
            match item {
                PathEvent::Begin { at } => {
                    write!(f, "M {} {} ", at.x, at.y)?;
                }
                PathEvent::End { close, .. } => {
                    if close {
                        write!(f, "Z ")?;
                    }
                }
                PathEvent::Line { to, .. } => {
                    write!(f, "L {} {} ", to.x, to.y)?;
                }
                PathEvent::Quadratic { ctrl, to, .. } => {
                    write!(f, "Q {} {} {} {} ", ctrl.x, ctrl.y, to.x, to.y)?;
                }
                PathEvent::Cubic { ctrl1, ctrl2, to, .. } => {
                    write!(f, "C {} {} {} {} {} {} ", ctrl1.x, ctrl1.y, ctrl2.x, ctrl2.y, to.x, to.y)?;
                }
            }
        }

        Ok(())
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
        self.verbs.push(Verb::CubicTo);
    }

    pub fn build(&mut self) -> Path {
        self.validator.finish();
        let mut aabb = match self.points.first() {
            Some(first) => self.points.iter().fold(
                LayoutRect { min: *first, max: *first },
                |aabb, p| LayoutRect { min: aabb.min.min(*p), max: aabb.max.max(*p) },
            ),
            None => LayoutRect::zero(),
        };
        // min and max ignore NaNs.
        if self.points.iter().any(|p| !p.x.is_finite() || !p.y.is_finite()) {
            aabb.min.x = f32::NAN;
        }

        let path = Path {
            points: self.points.as_slice().into(),
            verbs: self.verbs.as_slice().into(),
            aabb,
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
