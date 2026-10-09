#![cfg(allocative)]

use std::{
  borrow::Cow,
  cell::UnsafeCell,
  collections::HashMap,
  mem::{ManuallyDrop, MaybeUninit, size_of},
  sync::{
    Arc, LazyLock, Mutex,
    atomic::{AtomicBool, Ordering},
  },
};

use allocative::{Allocative, FlameGraphBuilder, Visit, Visitor};
use rspack_sources::{BoxSource, CachedSource, RawStringSource, SourceExt, SourceValue};
use smol_str::SmolStr;

fn capture(value: &dyn Visit) -> (usize, String, String) {
  capture_with(|visitor| value.visit_memory(visitor))
}

fn capture_with(visit: impl FnOnce(&mut Visitor<'_>)) -> (usize, String, String) {
  capture_in(FlameGraphBuilder::with_shared_ownership(), visit)
}

fn capture_in(
  mut builder: FlameGraphBuilder,
  visit: impl FnOnce(&mut Visitor<'_>),
) -> (usize, String, String) {
  let mut visitor = builder.root_visitor();
  visit(&mut visitor);
  visitor.exit();
  let output = builder.finish();
  let folded = output.flamegraph().write();
  let total = folded
    .lines()
    .map(|line| {
      line
        .rsplit_once(' ')
        .expect("folded row has a weight")
        .1
        .parse::<usize>()
        .expect("weight is an integer")
    })
    .sum();
  (total, folded, output.warnings())
}

#[test]
fn reserved_vector_capacity_is_counted_without_elements() {
  let value: Vec<u64> = Vec::with_capacity(128);
  let (total, _, warnings) = capture(&value);
  assert_eq!(
    total,
    size_of::<Vec<u64>>() + value.capacity() * size_of::<u64>()
  );
  assert!(warnings.is_empty(), "{warnings}");
}

#[test]
fn hash_map_accounts_for_capacity_and_nested_allocations() {
  let mut value = HashMap::with_capacity(64);
  let text = String::with_capacity(4096);
  let text_capacity = text.capacity();
  value.insert(1u64, text);
  let (total, _, warnings) = capture(&value);
  // Standard HashMap exposes usable capacity, not its private control-byte layout.
  assert_eq!(
    total,
    size_of_val(&value) + value.capacity() * size_of::<(u64, String)>() + text_capacity
  );
  assert!(warnings.is_empty(), "{warnings}");
}

// Deliberately test both the Arc allocation and its separately owned String buffer.
#[allow(clippy::rc_buffer)]
struct Owners {
  first: Arc<String>,
  second: Arc<String>,
}

#[test]
fn shared_allocations_stay_below_the_first_owner_and_are_counted_once() {
  let text = Arc::new("x".repeat(4096));
  let single = capture(&text).0;
  let value = Owners {
    first: text.clone(),
    second: text,
  };
  assert!(Arc::ptr_eq(&value.first, &value.second));
  let (total, folded, warnings) = capture(&value);
  assert_eq!(total, single + size_of::<Arc<String>>());
  assert!(
    folded
      .lines()
      .any(|line| line.contains("Owners;first;") && line.contains("capacity")),
    "{folded}"
  );
  assert!(
    !folded
      .lines()
      .any(|line| line.contains(";second;") && line.contains("capacity")),
    "{folded}"
  );
  assert!(warnings.is_empty(), "{warnings}");
}

struct Node {
  next: Mutex<Option<Arc<Node>>>,
  bytes: Vec<u8>,
}

#[test]
fn strong_cycles_terminate_and_deduplicate() {
  let first = Arc::new(Node {
    next: Mutex::new(None),
    bytes: vec![0; 4096],
  });
  let second = Arc::new(Node {
    next: Mutex::new(Some(first.clone())),
    bytes: vec![0; 8192],
  });
  *first.next.lock().unwrap() = Some(second.clone());
  assert_eq!(first.bytes.len() + second.bytes.len(), 4096 + 8192);
  let (total, _, warnings) = capture(&first);
  *first.next.lock().unwrap() = None;
  *second.next.lock().unwrap() = None;
  assert!(total >= 4096 + 8192);
  assert!(total < 4096 + 8192 + 1024);
  assert!(warnings.is_empty(), "{warnings}");
}

#[test]
fn shared_smol_strings_are_not_counted_twice() {
  let value = SmolStr::new("x".repeat(4096));
  assert!(value.is_heap_allocated());
  let single = capture(&value).0;
  let (total, _, warnings) = capture(&(value.clone(), value));
  assert_eq!(total, single + size_of::<SmolStr>());
  assert!(warnings.is_empty(), "{warnings}");
}

#[test]
fn profiling_does_not_initialize_lazy_values() {
  static INITIALIZED: AtomicBool = AtomicBool::new(false);
  let value: LazyLock<Vec<u8>> = LazyLock::new(|| {
    INITIALIZED.store(true, Ordering::Relaxed);
    vec![0; 4096]
  });
  let (total, _, warnings) = capture(&value);
  assert!(!INITIALIZED.load(Ordering::Relaxed));
  assert_eq!(total, size_of_val(&value));
  assert!(warnings.is_empty(), "{warnings}");
}

#[test]
fn cached_sources_follow_the_owned_source_without_rendering_it() {
  let raw: BoxSource = RawStringSource::from("x".repeat(4096)).boxed();
  let cache = CachedSource::new(raw.clone());
  let single = capture(&raw).0;
  let (total, _, warnings) = capture(&(raw, cache));
  assert!(total >= single);
  assert!(
    total < single + 2048,
    "source text was copied or counted twice: {total}"
  );
  assert!(warnings.is_empty(), "{warnings}");
}

#[test]
fn locked_values_report_missing_coverage_without_blocking() {
  let value = Mutex::new(vec![0u8; 4096]);
  let _guard = value.lock().expect("test lock");
  let (total, _, warnings) = capture(&value);
  assert_eq!(total, size_of_val(&value));
  assert!(
    warnings.contains("Unobserved nested allocations"),
    "{warnings}"
  );
}

#[test]
fn ordinary_nested_fields_arrays_and_static_references_need_no_derive() {
  struct Leaf {
    text: String,
  }
  struct Root<'a> {
    borrowed: &'a str,
    values: Vec<(Leaf, [Leaf; 2])>,
  }
  let borrowed = "borrowed storage is counted by its owner";
  let value = Root {
    borrowed,
    values: vec![(
      Leaf {
        text: String::with_capacity(31),
      },
      [
        Leaf {
          text: String::with_capacity(47),
        },
        Leaf {
          text: String::with_capacity(73),
        },
      ],
    )],
  };
  assert_eq!(value.borrowed, borrowed);
  let expected = size_of_val(&value)
    + value.values.capacity() * size_of::<(Leaf, [Leaf; 2])>()
    + value.values[0].0.text.capacity()
    + value.values[0]
      .1
      .iter()
      .map(|leaf| leaf.text.capacity())
      .sum::<usize>();
  let (total, folded, warnings) = capture(&value);
  assert_eq!(total, expected);
  assert!(folded.contains(";text;"), "{folded}");
  assert!(warnings.is_empty(), "{warnings}");
}

#[test]
fn trait_object_bridge_visits_the_unannotated_concrete_type() {
  trait Module: Visit {
    fn name(&self) -> &str;
  }
  struct TextModule {
    text: String,
  }
  impl Module for TextModule {
    fn name(&self) -> &str {
      &self.text
    }
  }
  let text = String::with_capacity(4096);
  let expected = size_of::<Box<dyn Module>>() + size_of::<TextModule>() + text.capacity();
  let value: Box<dyn Module> = Box::new(TextModule { text });
  assert_eq!(value.name(), "");
  let (total, folded, warnings) = capture(&value);
  assert_eq!(total, expected);
  assert!(folded.contains("TextModule;text;"), "{folded}");
  assert!(warnings.is_empty(), "{warnings}");
}

#[test]
fn generic_enum_adapter_accepts_unannotated_payloads() {
  #[derive(Allocative)]
  #[allocative(bound = "T: allocative::Visit + 'static")]
  enum Value<T> {
    Empty,
    Data(T),
  }
  struct Payload {
    text: String,
  }
  let payload = Payload {
    text: String::with_capacity(4096),
  };
  let expected = size_of::<Value<Payload>>() + payload.text.capacity();
  let (total, _, warnings) = capture(&Value::Data(payload));
  assert_eq!(total, expected);
  assert!(warnings.is_empty(), "{warnings}");
  assert_eq!(
    capture(&Value::<Payload>::Empty).0,
    size_of::<Value<Payload>>()
  );
}

#[test]
fn temporary_borrowed_sources_use_typed_adapters_without_following_borrows() {
  let text = String::from("temporary source");
  let borrowed = SourceValue::String(Cow::Borrowed(&text));
  let (total, _, warnings) = capture_with(|visitor| borrowed.visit(visitor));
  assert_eq!(total, size_of_val(&borrowed));
  assert!(warnings.is_empty(), "{warnings}");

  let owned = SourceValue::String(Cow::Owned(String::with_capacity(4096)));
  let (total, _, warnings) = capture_with(|visitor| owned.visit(visitor));
  assert_eq!(total, size_of_val(&owned) + 4096);
  assert!(warnings.is_empty(), "{warnings}");
}

#[test]
fn explicit_field_policy_takes_precedence_over_reflection() {
  #[derive(Allocative)]
  struct Value {
    text: String,
    #[allocative(visit = allocative::visit_opaque)]
    external: Vec<u8>,
  }
  let value = Value {
    text: String::with_capacity(1024),
    external: Vec::with_capacity(8192),
  };
  let (total, _, warnings) = capture(&value);
  assert_eq!(total, size_of_val(&value) + value.text.capacity());
  assert!(
    warnings.contains("Unobserved nested allocations"),
    "{warnings}"
  );
}

#[test]
fn unsupported_enum_reports_omitted_heap_instead_of_reading_inactive_fields() {
  enum Value {
    Empty,
    Data(String),
  }
  let value = Value::Data(String::with_capacity(4096));
  assert!(matches!(&value, Value::Data(text) if text.capacity() >= 4096));
  for value in [Value::Empty, value] {
    let (total, _, warnings) = capture(&value);
    assert_eq!(total, size_of_val(&value));
    assert!(
      warnings.contains("Unobserved nested allocations"),
      "{warnings}"
    );
  }
}

#[test]
fn unsynchronized_uninitialized_and_dropped_storage_is_not_read() {
  let cell = UnsafeCell::new(String::with_capacity(4096));
  let uninit = MaybeUninit::<String>::uninit();
  let mut dropped = ManuallyDrop::new(String::with_capacity(4096));
  // SAFETY: Drop once, without moving or reading the payload afterwards.
  unsafe { ManuallyDrop::drop(&mut dropped) };
  for (total, _, warnings) in [capture(&cell), capture(&uninit), capture(&dropped)] {
    assert_eq!(total, size_of::<String>());
    assert!(
      warnings.contains("Unobserved nested allocations"),
      "{warnings}"
    );
  }
}

#[test]
fn packed_fields_are_not_borrowed_at_an_invalid_alignment() {
  #[repr(C, packed)]
  struct Packed {
    byte: u8,
    text: String,
  }
  #[repr(C, align(16))]
  struct Root {
    byte: u8,
    packed: Packed,
  }
  let value = Root {
    byte: 0,
    packed: Packed {
      byte: 0,
      text: String::with_capacity(4096),
    },
  };
  assert_eq!(
    std::mem::offset_of!(Root, packed) + std::mem::offset_of!(Packed, text),
    2
  );
  let (total, _, warnings) = capture(&value);
  assert_eq!(total, size_of_val(&value));
  assert!(
    warnings.contains("Unobserved nested allocations"),
    "{warnings}"
  );
}

#[test]
fn array_lengths_with_const_expressions_keep_typed_field_adapters() {
  const COUNT: usize = 2;
  #[derive(Allocative)]
  struct Value {
    items: [String; COUNT + 1],
  }
  let value = Value {
    items: std::array::from_fn(|i| String::with_capacity(127 + i)),
  };
  let expected = size_of_val(&value) + value.items.iter().map(String::capacity).sum::<usize>();
  let (total, _, warnings) = capture(&value);
  assert_eq!(total, expected);
  assert!(warnings.is_empty(), "{warnings}");
}

#[test]
fn opaque_callbacks_keep_their_allocation_without_executing_or_following_captures() {
  let bytes = vec![0u8; 4096];
  let called = Arc::new(AtomicBool::new(false));
  let callback_called = Arc::clone(&called);
  let callback: Box<dyn Fn() -> usize + Send + Sync> = Box::new(move || {
    callback_called.store(true, Ordering::Relaxed);
    bytes.len()
  });
  let expected = size_of_val(&callback) + size_of_val(&*callback);
  let (total, _, warnings) = capture(&callback);
  assert_eq!(total, expected);
  assert!(
    warnings.contains("Unobserved nested allocations: dyn"),
    "{warnings}"
  );
  assert!(
    !warnings.contains("*const"),
    "callback payload was lost: {warnings}"
  );

  let callback: Arc<dyn Fn() -> usize + Send + Sync> = Arc::from(callback);
  let allocation = std::alloc::Layout::new::<[usize; 2]>()
    .extend(std::alloc::Layout::for_value(&*callback))
    .expect("callback layout")
    .0
    .pad_to_align()
    .size();
  let value = (callback.clone(), callback);
  assert_eq!(capture(&value).0, size_of_val(&value) + allocation);
  assert!(!called.load(Ordering::Relaxed));
}

#[test]
fn opaque_diagnostics_identify_each_field_without_changing_byte_weights() {
  #[derive(Allocative)]
  struct Fields {
    #[allocative(visit = allocative::visit_opaque)]
    first: Vec<u8>,
    #[allocative(visit = allocative::visit_opaque)]
    second: Vec<u8>,
  }
  let value = Fields {
    first: Vec::with_capacity(4096),
    second: Vec::with_capacity(8192),
  };
  let (total, folded, warnings) = capture(&value);
  assert_eq!(total, size_of_val(&value));
  let root = std::any::type_name::<Fields>();
  let name = std::any::type_name::<Vec<u8>>();
  assert_eq!(warnings.lines().count(), 2, "{warnings}");
  for field in ["first", "second"] {
    assert!(
      folded.contains(&format!(";{field};opaque;{name} {}", size_of::<Vec<u8>>())),
      "{folded}"
    );
    assert!(
      warnings.contains(&format!(
        "Unobserved nested allocations: {name} (path: {root};{field};opaque;{name})"
      )),
      "{warnings}"
    );
  }
}

#[test]
fn opaque_shared_callbacks_keep_the_owner_path_and_are_counted_once() {
  type Callback = dyn Fn(&str) -> usize + Send + Sync;
  struct Owners {
    first: Arc<Callback>,
    second: Arc<Callback>,
  }
  let bytes = vec![0u8; 4096];
  let callback: Arc<Callback> = Arc::new(move |text| bytes.len() + text.len());
  let allocation = std::alloc::Layout::new::<[usize; 2]>()
    .extend(std::alloc::Layout::for_value(&*callback))
    .expect("callback layout")
    .0
    .pad_to_align()
    .size();
  let value = Owners {
    first: callback.clone(),
    second: callback,
  };
  assert!(Arc::ptr_eq(&value.first, &value.second));
  for builder in [
    FlameGraphBuilder::with_shared_ownership(),
    FlameGraphBuilder::default(),
  ] {
    let (total, folded, warnings) = capture_in(builder, |visitor| value.visit_memory(visitor));
    assert_eq!(total, size_of_val(&value) + allocation);
    let name = std::any::type_name::<Callback>();
    assert!(folded.contains(&format!(";opaque;{name} ")), "{folded}");
    assert_eq!(warnings.lines().count(), 1, "{warnings}");
    assert!(
      warnings.contains(&format!(
        "(path: {};first;",
        std::any::type_name::<Owners>()
      )),
      "{warnings}"
    );
    assert!(!warnings.contains(";second;"), "{warnings}");
  }
}

#[test]
fn nested_optional_callbacks_count_slice_storage_and_deduplicate_opaque_payloads() {
  type Callback = dyn Fn(&str) -> usize + Send + Sync;
  let called = Arc::new(AtomicBool::new(false));
  let callback_called = called.clone();
  let bytes = vec![0u8; 4096];
  let callback: Arc<Callback> = Arc::new(move |text| {
    callback_called.store(true, Ordering::Relaxed);
    bytes.len() + text.len()
  });
  let allocation = std::alloc::Layout::new::<[usize; 2]>()
    .extend(std::alloc::Layout::for_value(&*callback))
    .expect("callback layout")
    .0
    .pad_to_align()
    .size();
  let value = Some(vec![None, Some(callback.clone()), Some(callback)].into_boxed_slice());
  let (total, folded, warnings) = capture(&value);
  assert_eq!(
    total,
    size_of_val(&value) + 3 * size_of::<Option<Arc<Callback>>>() + allocation
  );
  assert_eq!(warnings.lines().count(), 1, "{warnings}");
  assert!(folded.contains(";Some;"), "{folded}");
  assert!(
    warnings.contains(std::any::type_name::<Callback>()),
    "{warnings}"
  );
  assert!(!called.load(Ordering::Relaxed));
  let absent: Option<Box<[Option<Arc<Callback>>]>> = None;
  let (total, _, warnings) = capture(&absent);
  assert_eq!(total, size_of_val(&absent));
  assert!(warnings.is_empty(), "{warnings}");
}

#[test]
fn unknown_trait_objects_in_box_and_rc_use_the_generic_opaque_fallback() {
  use std::{any::Any, rc::Rc};

  let payload = String::with_capacity(4096);
  let boxed: Box<dyn Any> = Box::new(payload);
  let (total, _, warnings) = capture(&boxed);
  assert_eq!(total, size_of_val(&boxed) + size_of::<String>());
  assert!(
    warnings.contains(std::any::type_name::<dyn Any>()),
    "{warnings}"
  );

  let shared: Rc<dyn Any> = Rc::from(boxed);
  let allocation = std::alloc::Layout::new::<[usize; 2]>()
    .extend(std::alloc::Layout::for_value(&*shared))
    .expect("shared payload layout")
    .0
    .pad_to_align()
    .size();
  let owners = (shared.clone(), shared);
  let (total, _, warnings) = capture(&owners);
  assert_eq!(total, size_of_val(&owners) + allocation);
  assert_eq!(warnings.lines().count(), 1, "{warnings}");
}
