//! Keep the adapter crate checkable by the older Dylint toolchain, which does
//! not enable snapshots and predates the reflection API. Profiling builds use
//! the pinned workspace nightly and the reflected Visit implementation.

use crate::{Allocative, Visitor};

pub trait Visit {
  fn visit_memory<'a, 'b: 'a>(&self, visitor: &'a mut Visitor<'b>);
}

impl<T: Allocative + ?Sized> Visit for T {
  fn visit_memory<'a, 'b: 'a>(&self, visitor: &'a mut Visitor<'b>) {
    self.visit(visitor);
  }
}
