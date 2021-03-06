use crate::trampoline::{
    generate_global_export, generate_memory_export, generate_table_export, StoreInstanceHandle,
};
use crate::values::{from_checked_anyfunc, into_checked_anyfunc, Val};
use crate::{
    ExternRef, ExternType, Func, GlobalType, MemoryType, Mutability, Store, TableType, Trap,
    ValType,
};
use anyhow::{anyhow, bail, Result};
use std::mem;
use std::ptr;
use std::slice;
use wasmtime_environ::wasm;
use wasmtime_runtime::{self as runtime, InstanceHandle};

// Externals

/// An external item to a WebAssembly module, or a list of what can possibly be
/// exported from a wasm module.
///
/// This is both returned from [`Instance::exports`](crate::Instance::exports)
/// as well as required by [`Instance::new`](crate::Instance::new). In other
/// words, this is the type of extracted values from an instantiated module, and
/// it's also used to provide imported values when instantiating a module.
#[derive(Clone)]
pub enum Extern {
    /// A WebAssembly `func` which can be called.
    Func(Func),
    /// A WebAssembly `global` which acts like a `Cell<T>` of sorts, supporting
    /// `get` and `set` operations.
    Global(Global),
    /// A WebAssembly `table` which is an array of `Val` types.
    Table(Table),
    /// A WebAssembly linear memory.
    Memory(Memory),
}

impl Extern {
    /// Returns the underlying `Func`, if this external is a function.
    ///
    /// Returns `None` if this is not a function.
    pub fn into_func(self) -> Option<Func> {
        match self {
            Extern::Func(func) => Some(func),
            _ => None,
        }
    }

    /// Returns the underlying `Global`, if this external is a global.
    ///
    /// Returns `None` if this is not a global.
    pub fn into_global(self) -> Option<Global> {
        match self {
            Extern::Global(global) => Some(global),
            _ => None,
        }
    }

    /// Returns the underlying `Table`, if this external is a table.
    ///
    /// Returns `None` if this is not a table.
    pub fn into_table(self) -> Option<Table> {
        match self {
            Extern::Table(table) => Some(table),
            _ => None,
        }
    }

    /// Returns the underlying `Memory`, if this external is a memory.
    ///
    /// Returns `None` if this is not a memory.
    pub fn into_memory(self) -> Option<Memory> {
        match self {
            Extern::Memory(memory) => Some(memory),
            _ => None,
        }
    }

    /// Returns the type associated with this `Extern`.
    pub fn ty(&self) -> ExternType {
        match self {
            Extern::Func(ft) => ExternType::Func(ft.ty()),
            Extern::Memory(ft) => ExternType::Memory(ft.ty()),
            Extern::Table(tt) => ExternType::Table(tt.ty()),
            Extern::Global(gt) => ExternType::Global(gt.ty()),
        }
    }

    pub(crate) fn get_wasmtime_export(&self) -> wasmtime_runtime::Export {
        match self {
            Extern::Func(f) => f.wasmtime_function().clone().into(),
            Extern::Global(g) => g.wasmtime_export.clone().into(),
            Extern::Memory(m) => m.wasmtime_export.clone().into(),
            Extern::Table(t) => t.wasmtime_export.clone().into(),
        }
    }

    pub(crate) fn from_wasmtime_export(
        wasmtime_export: wasmtime_runtime::Export,
        instance: StoreInstanceHandle,
    ) -> Extern {
        match wasmtime_export {
            wasmtime_runtime::Export::Function(f) => {
                Extern::Func(Func::from_wasmtime_function(f, instance))
            }
            wasmtime_runtime::Export::Memory(m) => {
                Extern::Memory(Memory::from_wasmtime_memory(m, instance))
            }
            wasmtime_runtime::Export::Global(g) => {
                Extern::Global(Global::from_wasmtime_global(g, instance))
            }
            wasmtime_runtime::Export::Table(t) => {
                Extern::Table(Table::from_wasmtime_table(t, instance))
            }
        }
    }

    pub(crate) fn comes_from_same_store(&self, store: &Store) -> bool {
        let my_store = match self {
            Extern::Func(f) => f.store(),
            Extern::Global(g) => &g.instance.store,
            Extern::Memory(m) => &m.instance.store,
            Extern::Table(t) => &t.instance.store,
        };
        Store::same(my_store, store)
    }
}

impl From<Func> for Extern {
    fn from(r: Func) -> Self {
        Extern::Func(r)
    }
}

impl From<Global> for Extern {
    fn from(r: Global) -> Self {
        Extern::Global(r)
    }
}

impl From<Memory> for Extern {
    fn from(r: Memory) -> Self {
        Extern::Memory(r)
    }
}

impl From<Table> for Extern {
    fn from(r: Table) -> Self {
        Extern::Table(r)
    }
}

/// A WebAssembly `global` value which can be read and written to.
///
/// A `global` in WebAssembly is sort of like a global variable within an
/// [`Instance`](crate::Instance). The `global.get` and `global.set`
/// instructions will modify and read global values in a wasm module. Globals
/// can either be imported or exported from wasm modules.
///
/// If you're familiar with Rust already you can think of a `Global` as a sort
/// of `Rc<Cell<Val>>`, more or less.
///
/// # `Global` and `Clone`
///
/// Globals are internally reference counted so you can `clone` a `Global`. The
/// cloning process only performs a shallow clone, so two cloned `Global`
/// instances are equivalent in their functionality.
#[derive(Clone)]
pub struct Global {
    instance: StoreInstanceHandle,
    wasmtime_export: wasmtime_runtime::ExportGlobal,
}

impl Global {
    /// Creates a new WebAssembly `global` value with the provide type `ty` and
    /// initial value `val`.
    ///
    /// The `store` argument provided is used as a general global cache for
    /// information, and otherwise the `ty` and `val` arguments are used to
    /// initialize the global.
    ///
    /// # Errors
    ///
    /// Returns an error if the `ty` provided does not match the type of the
    /// value `val`.
    pub fn new(store: &Store, ty: GlobalType, val: Val) -> Result<Global> {
        if !val.comes_from_same_store(store) {
            bail!("cross-`Store` globals are not supported");
        }
        if val.ty() != *ty.content() {
            bail!("value provided does not match the type of this global");
        }
        let (instance, wasmtime_export) = generate_global_export(store, &ty, val)?;
        Ok(Global {
            instance,
            wasmtime_export,
        })
    }

    /// Returns the underlying type of this `global`.
    pub fn ty(&self) -> GlobalType {
        // The original export is coming from wasmtime_runtime itself we should
        // support all the types coming out of it, so assert such here.
        GlobalType::from_wasmtime_global(&self.wasmtime_export.global)
    }

    /// Returns the value type of this `global`.
    pub fn val_type(&self) -> ValType {
        ValType::from_wasm_type(&self.wasmtime_export.global.wasm_ty)
    }

    /// Returns the underlying mutability of this `global`.
    pub fn mutability(&self) -> Mutability {
        if self.wasmtime_export.global.mutability {
            Mutability::Var
        } else {
            Mutability::Const
        }
    }

    /// Returns the current [`Val`] of this global.
    pub fn get(&self) -> Val {
        unsafe {
            let definition = &mut *self.wasmtime_export.definition;
            match self.val_type() {
                ValType::I32 => Val::from(*definition.as_i32()),
                ValType::I64 => Val::from(*definition.as_i64()),
                ValType::F32 => Val::F32(*definition.as_u32()),
                ValType::F64 => Val::F64(*definition.as_u64()),
                ValType::ExternRef => Val::ExternRef(
                    definition
                        .as_externref()
                        .clone()
                        .map(|inner| ExternRef { inner }),
                ),
                ValType::FuncRef => {
                    from_checked_anyfunc(definition.as_anyfunc() as *mut _, &self.instance.store)
                }
                ty => unimplemented!("Global::get for {:?}", ty),
            }
        }
    }

    /// Attempts to set the current value of this global to [`Val`].
    ///
    /// # Errors
    ///
    /// Returns an error if this global has a different type than `Val`, or if
    /// it's not a mutable global.
    pub fn set(&self, val: Val) -> Result<()> {
        if self.mutability() != Mutability::Var {
            bail!("immutable global cannot be set");
        }
        let ty = self.val_type();
        if val.ty() != ty {
            bail!("global of type {:?} cannot be set to {:?}", ty, val.ty());
        }
        if !val.comes_from_same_store(&self.instance.store) {
            bail!("cross-`Store` values are not supported");
        }
        unsafe {
            let definition = &mut *self.wasmtime_export.definition;
            match val {
                Val::I32(i) => *definition.as_i32_mut() = i,
                Val::I64(i) => *definition.as_i64_mut() = i,
                Val::F32(f) => *definition.as_u32_mut() = f,
                Val::F64(f) => *definition.as_u64_mut() = f,
                Val::FuncRef(f) => {
                    *definition.as_anyfunc_mut() = f.map_or(ptr::null(), |f| {
                        f.caller_checked_anyfunc().as_ptr() as *const _
                    });
                }
                Val::ExternRef(x) => {
                    // In case the old value's `Drop` implementation is
                    // re-entrant and tries to touch this global again, do a
                    // replace, and then drop. This way no one can observe a
                    // halfway-deinitialized value.
                    let old = mem::replace(definition.as_externref_mut(), x.map(|x| x.inner));
                    drop(old);
                }
                _ => unimplemented!("Global::set for {:?}", val.ty()),
            }
        }
        Ok(())
    }

    pub(crate) fn from_wasmtime_global(
        wasmtime_export: wasmtime_runtime::ExportGlobal,
        instance: StoreInstanceHandle,
    ) -> Global {
        Global {
            instance,
            wasmtime_export,
        }
    }
}

/// A WebAssembly `table`, or an array of values.
///
/// Like [`Memory`] a table is an indexed array of values, but unlike [`Memory`]
/// it's an array of WebAssembly values rather than bytes. One of the most
/// common usages of a table is a function table for wasm modules, where each
/// element has the `Func` type.
///
/// Tables, like globals, are not threadsafe and can only be used on one thread.
/// Tables can be grown in size and each element can be read/written.
///
/// # `Table` and `Clone`
///
/// Tables are internally reference counted so you can `clone` a `Table`. The
/// cloning process only performs a shallow clone, so two cloned `Table`
/// instances are equivalent in their functionality.
#[derive(Clone)]
pub struct Table {
    instance: StoreInstanceHandle,
    wasmtime_export: wasmtime_runtime::ExportTable,
}

fn set_table_item(
    instance: &InstanceHandle,
    table_index: wasm::DefinedTableIndex,
    item_index: u32,
    item: runtime::TableElement,
) -> Result<()> {
    instance
        .table_set(table_index, item_index, item)
        .map_err(|()| anyhow!("table element index out of bounds"))
}

impl Table {
    /// Creates a new `Table` with the given parameters.
    ///
    /// * `store` - a global cache to store information in
    /// * `ty` - the type of this table, containing both the element type as
    ///   well as the initial size and maximum size, if any.
    /// * `init` - the initial value to fill all table entries with, if the
    ///   table starts with an initial size.
    ///
    /// # Errors
    ///
    /// Returns an error if `init` does not match the element type of the table.
    pub fn new(store: &Store, ty: TableType, init: Val) -> Result<Table> {
        let (instance, wasmtime_export) = generate_table_export(store, &ty)?;

        let init: runtime::TableElement = match ty.element() {
            ValType::FuncRef => into_checked_anyfunc(init, store)?.into(),
            ValType::ExternRef => init
                .externref()
                .ok_or_else(|| {
                    anyhow!("table initialization value does not have expected type `externref`")
                })?
                .map(|x| x.inner)
                .into(),
            ty => bail!("unsupported table element type: {:?}", ty),
        };

        // Initialize entries with the init value.
        let definition = unsafe { &*wasmtime_export.definition };
        let index = instance.table_index(definition);
        for i in 0..definition.current_elements {
            set_table_item(&instance, index, i, init.clone())?;
        }

        Ok(Table {
            instance,
            wasmtime_export,
        })
    }

    /// Returns the underlying type of this table, including its element type as
    /// well as the maximum/minimum lower bounds.
    pub fn ty(&self) -> TableType {
        TableType::from_wasmtime_table(&self.wasmtime_export.table.table)
    }

    fn wasmtime_table_index(&self) -> wasm::DefinedTableIndex {
        unsafe { self.instance.table_index(&*self.wasmtime_export.definition) }
    }

    /// Returns the table element value at `index`.
    ///
    /// Returns `None` if `index` is out of bounds.
    pub fn get(&self, index: u32) -> Option<Val> {
        let table_index = self.wasmtime_table_index();
        let item = self.instance.table_get(table_index, index)?;
        match item {
            runtime::TableElement::FuncRef(f) => {
                Some(unsafe { from_checked_anyfunc(f, &self.instance.store) })
            }
            runtime::TableElement::ExternRef(None) => Some(Val::ExternRef(None)),
            runtime::TableElement::ExternRef(Some(x)) => {
                Some(Val::ExternRef(Some(ExternRef { inner: x })))
            }
        }
    }

    /// Writes the `val` provided into `index` within this table.
    ///
    /// # Errors
    ///
    /// Returns an error if `index` is out of bounds or if `val` does not have
    /// the right type to be stored in this table.
    pub fn set(&self, index: u32, val: Val) -> Result<()> {
        if !val.comes_from_same_store(&self.instance.store) {
            bail!("cross-`Store` values are not supported in tables");
        }
        let table_index = self.wasmtime_table_index();
        set_table_item(
            &self.instance,
            table_index,
            index,
            val.into_table_element()?,
        )
    }

    /// Returns the current size of this table.
    pub fn size(&self) -> u32 {
        unsafe { (*self.wasmtime_export.definition).current_elements }
    }

    /// Grows the size of this table by `delta` more elements, initialization
    /// all new elements to `init`.
    ///
    /// Returns the previous size of this table if successful.
    ///
    /// # Errors
    ///
    /// Returns an error if the table cannot be grown by `delta`, for example
    /// if it would cause the table to exceed its maximum size. Also returns an
    /// error if `init` is not of the right type.
    pub fn grow(&self, delta: u32, init: Val) -> Result<u32> {
        let index = self.wasmtime_table_index();
        let orig_size = match self.ty().element() {
            ValType::FuncRef => {
                let init = into_checked_anyfunc(init, &self.instance.store)?;
                self.instance.defined_table_grow(index, delta, init.into())
            }
            ValType::ExternRef => {
                let init = match init {
                    Val::ExternRef(Some(x)) => Some(x.inner),
                    Val::ExternRef(None) => None,
                    _ => bail!("incorrect init value for growing table"),
                };
                self.instance.defined_table_grow(
                    index,
                    delta,
                    runtime::TableElement::ExternRef(init),
                )
            }
            _ => unreachable!("only `funcref` and `externref` tables are supported"),
        };
        if let Some(size) = orig_size {
            Ok(size)
        } else {
            bail!("failed to grow table by `{}`", delta)
        }
    }

    /// Copy `len` elements from `src_table[src_index..]` into
    /// `dst_table[dst_index..]`.
    ///
    /// # Errors
    ///
    /// Returns an error if the range is out of bounds of either the source or
    /// destination tables.
    pub fn copy(
        dst_table: &Table,
        dst_index: u32,
        src_table: &Table,
        src_index: u32,
        len: u32,
    ) -> Result<()> {
        if !Store::same(&dst_table.instance.store, &src_table.instance.store) {
            bail!("cross-`Store` table copies are not supported");
        }

        // NB: We must use the `dst_table`'s `wasmtime_handle` for the
        // `dst_table_index` and vice versa for `src_table` since each table can
        // come from different modules.

        let dst_table_index = dst_table.wasmtime_table_index();
        let dst_table = dst_table.instance.get_defined_table(dst_table_index);

        let src_table_index = src_table.wasmtime_table_index();
        let src_table = src_table.instance.get_defined_table(src_table_index);

        runtime::Table::copy(dst_table, src_table, dst_index, src_index, len)
            .map_err(Trap::from_runtime)?;
        Ok(())
    }

    /// Fill `table[dst..(dst + len)]` with the given value.
    ///
    /// # Errors
    ///
    /// Returns an error if
    ///
    /// * `val` is not of the same type as this table's
    ///   element type,
    ///
    /// * the region to be filled is out of bounds, or
    ///
    /// * `val` comes from a different `Store` from this table.
    pub fn fill(&self, dst: u32, val: Val, len: u32) -> Result<()> {
        if !val.comes_from_same_store(&self.instance.store) {
            bail!("cross-`Store` table fills are not supported");
        }

        let table_index = self.wasmtime_table_index();
        self.instance
            .handle
            .defined_table_fill(table_index, dst, val.into_table_element()?, len)
            .map_err(Trap::from_runtime)?;

        Ok(())
    }

    pub(crate) fn from_wasmtime_table(
        wasmtime_export: wasmtime_runtime::ExportTable,
        instance: StoreInstanceHandle,
    ) -> Table {
        Table {
            instance,
            wasmtime_export,
        }
    }
}

/// A WebAssembly linear memory.
///
/// WebAssembly memories represent a contiguous array of bytes that have a size
/// that is always a multiple of the WebAssembly page size, currently 64
/// kilobytes.
///
/// WebAssembly memory is used for global data, statics in C/C++/Rust, shadow
/// stack memory, etc. Accessing wasm memory is generally quite fast!
///
/// # `Memory` and `Clone`
///
/// Memories are internally reference counted so you can `clone` a `Memory`. The
/// cloning process only performs a shallow clone, so two cloned `Memory`
/// instances are equivalent in their functionality.
///
/// # `Memory` and threads
///
/// It is intended that `Memory` is safe to share between threads. At this time
/// this is not implemented in `wasmtime`, however. This is planned to be
/// implemented though!
///
/// # `Memory` and Safety
///
/// Linear memory is a lynchpin of safety for WebAssembly, but it turns out
/// there are very few ways to safely inspect the contents of a memory from the
/// host (Rust). This is because memory safety is quite tricky when working with
/// a `Memory` and we're still working out the best idioms to encapsulate
/// everything safely where it's efficient and ergonomic. This section of
/// documentation, however, is intended to help educate a bit what is and isn't
/// safe when working with `Memory`.
///
/// For safety purposes you can think of a `Memory` as a glorified
/// `Rc<UnsafeCell<Vec<u8>>>`. There are a few consequences of this
/// interpretation:
///
/// * At any time someone else may have access to the memory (hence the `Rc`).
///   This could be a wasm instance, other host code, or a set of wasm instances
///   which all reference a `Memory`. When in doubt assume someone else has a
///   handle to your `Memory`.
///
/// * At any time, memory can be read from or written to (hence the
///   `UnsafeCell`). Anyone with a handle to a wasm memory can read/write to it.
///   Primarily other instances can execute the `load` and `store` family of
///   instructions, as well as any other which modifies or reads memory.
///
/// * At any time memory may grow (hence the `Vec<..>`). Growth may relocate the
///   base memory pointer (similar to how `vec.push(...)` can change the result
///   of `.as_ptr()`)
///
/// So given that we're working roughly with `Rc<UnsafeCell<Vec<u8>>>` that's a
/// lot to keep in mind! It's hopefully though sort of setting the stage as to
/// what you can safely do with memories.
///
/// Let's run through a few safe examples first of how you can use a `Memory`.
///
/// ```rust
/// use wasmtime::Memory;
///
/// fn safe_examples(mem: &Memory) {
///     // Just like wasm, it's safe to read memory almost at any time. The
///     // gotcha here is that we need to be sure to load from the correct base
///     // pointer and perform the bounds check correctly. So long as this is
///     // all self contained here (e.g. not arbitrary code in the middle) we're
///     // good to go.
///     let byte = unsafe { mem.data_unchecked()[0x123] };
///
///     // Short-lived borrows of memory are safe, but they must be scoped and
///     // not have code which modifies/etc `Memory` while the borrow is active.
///     // For example if you want to read a string from memory it is safe to do
///     // so:
///     let string_base = 0xdead;
///     let string_len = 0xbeef;
///     let string = unsafe {
///         let bytes = &mem.data_unchecked()[string_base..][..string_len];
///         match std::str::from_utf8(bytes) {
///             Ok(s) => s.to_string(), // copy out of wasm memory
///             Err(_) => panic!("not valid utf-8"),
///         }
///     };
///
///     // Additionally like wasm you can write to memory at any point in time,
///     // again making sure that after you get the unchecked slice you don't
///     // execute code which could read/write/modify `Memory`:
///     unsafe {
///         mem.data_unchecked_mut()[0x123] = 3;
///     }
///
///     // When working with *borrows* that point directly into wasm memory you
///     // need to be extremely careful. Any functionality that operates on a
///     // borrow into wasm memory needs to be thoroughly audited to effectively
///     // not touch the `Memory` at all
///     let data_base = 0xfeed;
///     let data_len = 0xface;
///     unsafe {
///         let data = &mem.data_unchecked()[data_base..][..data_len];
///         host_function_that_doesnt_touch_memory(data);
///
///         // effectively the same rules apply to mutable borrows
///         let data_mut = &mut mem.data_unchecked_mut()[data_base..][..data_len];
///         host_function_that_doesnt_touch_memory(data);
///     }
/// }
/// # fn host_function_that_doesnt_touch_memory(_: &[u8]){}
/// ```
///
/// It's worth also, however, covering some examples of **incorrect**,
/// **unsafe** usages of `Memory`. Do not do these things!
///
/// ```rust
/// # use anyhow::Result;
/// use wasmtime::Memory;
///
/// // NOTE: All code in this function is not safe to execute and may cause
/// // segfaults/undefined behavior at runtime. Do not copy/paste these examples
/// // into production code!
/// unsafe fn unsafe_examples(mem: &Memory) -> Result<()> {
///     // First and foremost, any borrow can be invalidated at any time via the
///     // `Memory::grow` function. This can relocate memory which causes any
///     // previous pointer to be possibly invalid now.
///     let pointer: &u8 = &mem.data_unchecked()[0x100];
///     mem.grow(1)?; // invalidates `pointer`!
///     // println!("{}", *pointer); // FATAL: use-after-free
///
///     // Note that the use-after-free also applies to slices, whether they're
///     // slices of bytes or strings.
///     let slice: &[u8] = &mem.data_unchecked()[0x100..0x102];
///     mem.grow(1)?; // invalidates `slice`!
///     // println!("{:?}", slice); // FATAL: use-after-free
///
///     // Due to the reference-counted nature of `Memory` note that literal
///     // calls to `Memory::grow` are not sufficient to audit for. You'll need
///     // to be careful that any mutation of `Memory` doesn't happen while
///     // you're holding an active borrow.
///     let slice: &[u8] = &mem.data_unchecked()[0x100..0x102];
///     some_other_function(); // may invalidate `slice` through another `mem` reference
///     // println!("{:?}", slice); // FATAL: maybe a use-after-free
///
///     // An especially subtle aspect of accessing a wasm instance's memory is
///     // that you need to be extremely careful about aliasing. Anyone at any
///     // time can call `data_unchecked()` or `data_unchecked_mut()`, which
///     // means you can easily have aliasing mutable references:
///     let ref1: &u8 = &mem.data_unchecked()[0x100];
///     let ref2: &mut u8 = &mut mem.data_unchecked_mut()[0x100];
///     // *ref2 = *ref1; // FATAL: violates Rust's aliasing rules
///
///     // Note that aliasing applies to strings as well, for example this is
///     // not valid because the slices overlap.
///     let slice1: &mut [u8] = &mut mem.data_unchecked_mut()[0x100..][..3];
///     let slice2: &mut [u8] = &mut mem.data_unchecked_mut()[0x102..][..4];
///     // println!("{:?} {:?}", slice1, slice2); // FATAL: aliasing mutable pointers
///
///     Ok(())
/// }
/// # fn some_other_function() {}
/// ```
///
/// Overall there's some general rules of thumb when working with `Memory` and
/// getting raw pointers inside of it:
///
/// * If you never have a "long lived" pointer into memory, you're likely in the
///   clear. Care still needs to be taken in threaded scenarios or when/where
///   data is read, but you'll be shielded from many classes of issues.
/// * Long-lived pointers must always respect Rust'a aliasing rules. It's ok for
///   shared borrows to overlap with each other, but mutable borrows must
///   overlap with nothing.
/// * Long-lived pointers are only valid if `Memory` isn't used in an unsafe way
///   while the pointer is valid. This includes both aliasing and growth.
///
/// At this point it's worth reiterating again that working with `Memory` is
/// pretty tricky and that's not great! Proposals such as [interface types] are
/// intended to prevent wasm modules from even needing to import/export memory
/// in the first place, which obviates the need for all of these safety caveats!
/// Additionally over time we're still working out the best idioms to expose in
/// `wasmtime`, so if you've got ideas or questions please feel free to [open an
/// issue]!
///
/// ## `Memory` Safety and Threads
///
/// Currently the `wasmtime` crate does not implement the wasm threads proposal,
/// but it is planned to do so. It's additionally worthwhile discussing how this
/// affects memory safety and what was previously just discussed as well.
///
/// Once threads are added into the mix, all of the above rules still apply.
/// There's an additional, rule, however, that all reads and writes can
/// happen *concurrently*. This effectively means that long-lived borrows into
/// wasm memory are virtually never safe to have.
///
/// Mutable pointers are fundamentally unsafe to have in a concurrent scenario
/// in the face of arbitrary wasm code. Only if you dynamically know for sure
/// that wasm won't access a region would it be safe to construct a mutable
/// pointer. Additionally even shared pointers are largely unsafe because their
/// underlying contents may change, so unless `UnsafeCell` in one form or
/// another is used everywhere there's no safety.
///
/// One important point about concurrency is that `Memory::grow` can indeed
/// happen concurrently. This, however, will never relocate the base pointer.
/// Shared memories must always have a maximum size and they will be
/// preallocated such that growth will never relocate the base pointer. The
/// maximum length of the memory, however, will change over time.
///
/// Overall the general rule of thumb for shared memories is that you must
/// atomically read and write everything. Nothing can be borrowed and everything
/// must be eagerly copied out.
///
/// [interface types]: https://github.com/webassembly/interface-types
/// [open an issue]: https://github.com/bytecodealliance/wasmtime/issues/new
#[derive(Clone)]
pub struct Memory {
    instance: StoreInstanceHandle,
    wasmtime_export: wasmtime_runtime::ExportMemory,
}

impl Memory {
    /// Creates a new WebAssembly memory given the configuration of `ty`.
    ///
    /// The `store` argument is a general location for cache information, and
    /// otherwise the memory will immediately be allocated according to the
    /// type's configuration. All WebAssembly memory is initialized to zero.
    ///
    /// # Examples
    ///
    /// ```
    /// # use wasmtime::*;
    /// # fn main() -> anyhow::Result<()> {
    /// let engine = Engine::default();
    /// let store = Store::new(&engine);
    ///
    /// let memory_ty = MemoryType::new(Limits::new(1, None));
    /// let memory = Memory::new(&store, memory_ty);
    ///
    /// let module = Module::new(&engine, "(module (memory (import \"\" \"\") 1))")?;
    /// let instance = Instance::new(&store, &module, &[memory.into()])?;
    /// // ...
    /// # Ok(())
    /// # }
    /// ```
    pub fn new(store: &Store, ty: MemoryType) -> Memory {
        let (instance, wasmtime_export) =
            generate_memory_export(store, &ty).expect("generated memory");
        Memory {
            instance,
            wasmtime_export,
        }
    }

    /// Returns the underlying type of this memory.
    ///
    /// # Examples
    ///
    /// ```
    /// # use wasmtime::*;
    /// # fn main() -> anyhow::Result<()> {
    /// let engine = Engine::default();
    /// let store = Store::new(&engine);
    /// let module = Module::new(&engine, "(module (memory (export \"mem\") 1))")?;
    /// let instance = Instance::new(&store, &module, &[])?;
    /// let memory = instance.get_memory("mem").unwrap();
    /// let ty = memory.ty();
    /// assert_eq!(ty.limits().min(), 1);
    /// # Ok(())
    /// # }
    /// ```
    pub fn ty(&self) -> MemoryType {
        MemoryType::from_wasmtime_memory(&self.wasmtime_export.memory.memory)
    }

    /// Returns this memory as a slice view that can be read natively in Rust.
    ///
    /// # Safety
    ///
    /// This is an unsafe operation because there is no guarantee that the
    /// following operations do not happen concurrently while the slice is in
    /// use:
    ///
    /// * Data could be modified by calling into a wasm module.
    /// * Memory could be relocated through growth by calling into a wasm
    ///   module.
    /// * When threads are supported, non-atomic reads will race with other
    ///   writes.
    ///
    /// Extreme care need be taken when the data of a `Memory` is read. The
    /// above invariants all need to be upheld at a bare minimum, and in
    /// general you'll need to ensure that while you're looking at slice you're
    /// the only one who can possibly look at the slice and read/write it.
    ///
    /// Be sure to keep in mind that `Memory` is reference counted, meaning
    /// that there may be other users of this `Memory` instance elsewhere in
    /// your program. Additionally `Memory` can be shared and used in any number
    /// of wasm instances, so calling any wasm code should be considered
    /// dangerous while you're holding a slice of memory.
    ///
    /// For more information and examples see the documentation on the
    /// [`Memory`] type.
    pub unsafe fn data_unchecked(&self) -> &[u8] {
        self.data_unchecked_mut()
    }

    /// Returns this memory as a slice view that can be read and written
    /// natively in Rust.
    ///
    /// # Safety
    ///
    /// All of the same safety caveats of [`Memory::data_unchecked`] apply
    /// here, doubly so because this is returning a mutable slice! As a
    /// double-extra reminder, remember that `Memory` is reference counted, so
    /// you can very easily acquire two mutable slices by simply calling this
    /// function twice. Extreme caution should be used when using this method,
    /// and in general you probably want to result to unsafe accessors and the
    /// `data` methods below.
    ///
    /// For more information and examples see the documentation on the
    /// [`Memory`] type.
    pub unsafe fn data_unchecked_mut(&self) -> &mut [u8] {
        let definition = &*self.wasmtime_export.definition;
        slice::from_raw_parts_mut(definition.base, definition.current_length)
    }

    /// Returns the base pointer, in the host's address space, that the memory
    /// is located at.
    ///
    /// When reading and manipulating memory be sure to read up on the caveats
    /// of [`Memory::data_unchecked`] to make sure that you can safely
    /// read/write the memory.
    ///
    /// For more information and examples see the documentation on the
    /// [`Memory`] type.
    pub fn data_ptr(&self) -> *mut u8 {
        unsafe { (*self.wasmtime_export.definition).base }
    }

    /// Returns the byte length of this memory.
    ///
    /// The returned value will be a multiple of the wasm page size, 64k.
    ///
    /// For more information and examples see the documentation on the
    /// [`Memory`] type.
    pub fn data_size(&self) -> usize {
        unsafe { (*self.wasmtime_export.definition).current_length }
    }

    /// Returns the size, in pages, of this wasm memory.
    pub fn size(&self) -> u32 {
        (self.data_size() / wasmtime_environ::WASM_PAGE_SIZE as usize) as u32
    }

    /// Grows this WebAssembly memory by `delta` pages.
    ///
    /// This will attempt to add `delta` more pages of memory on to the end of
    /// this `Memory` instance. If successful this may relocate the memory and
    /// cause [`Memory::data_ptr`] to return a new value. Additionally previous
    /// slices into this memory may no longer be valid.
    ///
    /// On success returns the number of pages this memory previously had
    /// before the growth succeeded.
    ///
    /// # Errors
    ///
    /// Returns an error if memory could not be grown, for example if it exceeds
    /// the maximum limits of this memory.
    ///
    /// # Examples
    ///
    /// ```
    /// # use wasmtime::*;
    /// # fn main() -> anyhow::Result<()> {
    /// let engine = Engine::default();
    /// let store = Store::new(&engine);
    /// let module = Module::new(&engine, "(module (memory (export \"mem\") 1 2))")?;
    /// let instance = Instance::new(&store, &module, &[])?;
    /// let memory = instance.get_memory("mem").unwrap();
    ///
    /// assert_eq!(memory.size(), 1);
    /// assert_eq!(memory.grow(1)?, 1);
    /// assert_eq!(memory.size(), 2);
    /// assert!(memory.grow(1).is_err());
    /// assert_eq!(memory.size(), 2);
    /// assert_eq!(memory.grow(0)?, 2);
    /// # Ok(())
    /// # }
    /// ```
    pub fn grow(&self, delta: u32) -> Result<u32> {
        let index = self
            .instance
            .memory_index(unsafe { &*self.wasmtime_export.definition });
        self.instance
            .memory_grow(index, delta)
            .ok_or_else(|| anyhow!("failed to grow memory"))
    }

    pub(crate) fn from_wasmtime_memory(
        wasmtime_export: wasmtime_runtime::ExportMemory,
        instance: StoreInstanceHandle,
    ) -> Memory {
        Memory {
            instance,
            wasmtime_export,
        }
    }
}

/// A linear memory. This trait provides an interface for raw memory buffers which are used
/// by wasmtime, e.g. inside ['Memory']. Such buffers are in principle not thread safe.
/// By implementing this trait together with MemoryCreator,
/// one can supply wasmtime with custom allocated host managed memory.
///
/// # Safety
/// The memory should be page aligned and a multiple of page size.
/// To prevent possible silent overflows, the memory should be protected by a guard page.
/// Additionally the safety concerns explained in ['Memory'], for accessing the memory
/// apply here as well.
///
/// Note that this is a relatively new and experimental feature and it is recommended
/// to be familiar with wasmtime runtime code to use it.
pub unsafe trait LinearMemory {
    /// Returns the number of allocated wasm pages.
    fn size(&self) -> u32;

    /// Grow memory by the specified amount of wasm pages.
    ///
    /// Returns `None` if memory can't be grown by the specified amount
    /// of wasm pages.
    fn grow(&self, delta: u32) -> Option<u32>;

    /// Return the allocated memory as a mutable pointer to u8.
    fn as_ptr(&self) -> *mut u8;
}

/// A memory creator. Can be used to provide a memory creator
/// to wasmtime which supplies host managed memory.
///
/// # Safety
/// This trait is unsafe, as the memory safety depends on proper implementation of
/// memory management. Memories created by the MemoryCreator should always be treated
/// as owned by wasmtime instance, and any modification of them outside of wasmtime
/// invoked routines is unsafe and may lead to corruption.
///
/// Note that this is a relatively new and experimental feature and it is recommended
/// to be familiar with wasmtime runtime code to use it.
pub unsafe trait MemoryCreator: Send + Sync {
    /// Create a new `LinearMemory` object from the specified parameters.
    ///
    /// The type of memory being created is specified by `ty` which indicates
    /// both the minimum and maximum size, in wasm pages.
    ///
    /// The `reserved_size_in_bytes` value indicates the expected size of the
    /// reservation that is to be made for this memory. If this value is `None`
    /// than the implementation is free to allocate memory as it sees fit. If
    /// the value is `Some`, however, then the implementation is expected to
    /// reserve that many bytes for the memory's allocation, plus the guard
    /// size at the end. Note that this reservation need only be a virtual
    /// memory reservation, physical memory does not need to be allocated
    /// immediately. In this case `grow` should never move the base pointer and
    /// the maximum size of `ty` is guaranteed to fit within `reserved_size_in_bytes`.
    ///
    /// The `guard_size_in_bytes` parameter indicates how many bytes of space, after the
    /// memory allocation, is expected to be unmapped. JIT code will elide
    /// bounds checks based on the `guard_size_in_bytes` provided, so for JIT code to
    /// work correctly the memory returned will need to be properly guarded with
    /// `guard_size_in_bytes` bytes left unmapped after the base allocation.
    ///
    /// Note that the `reserved_size_in_bytes` and `guard_size_in_bytes` options are tuned from
    /// the various [`Config`](crate::Config) methods about memory
    /// sizes/guards. Additionally these two values are guaranteed to be
    /// multiples of the system page size.
    fn new_memory(
        &self,
        ty: MemoryType,
        reserved_size_in_bytes: Option<u64>,
        guard_size_in_bytes: u64,
    ) -> Result<Box<dyn LinearMemory>, String>;
}

#[cfg(test)]
mod tests {
    use crate::*;

    // Assert that creating a memory via `Memory::new` respects the limits/tunables
    // in `Config`.
    #[test]
    fn respect_tunables() {
        let mut cfg = Config::new();
        cfg.static_memory_maximum_size(0)
            .dynamic_memory_guard_size(0);
        let store = Store::new(&Engine::new(&cfg));
        let ty = MemoryType::new(Limits::new(1, None));
        let mem = Memory::new(&store, ty);
        assert_eq!(mem.wasmtime_export.memory.offset_guard_size, 0);
        match mem.wasmtime_export.memory.style {
            wasmtime_environ::MemoryStyle::Dynamic => {}
            other => panic!("unexpected style {:?}", other),
        }
    }
}

// Exports

/// An exported WebAssembly value.
///
/// This type is primarily accessed from the
/// [`Instance::exports`](crate::Instance::exports) accessor and describes what
/// names and items are exported from a wasm instance.
#[derive(Clone)]
pub struct Export<'instance> {
    /// The name of the export.
    name: &'instance str,

    /// The definition of the export.
    definition: Extern,
}

impl<'instance> Export<'instance> {
    /// Creates a new export which is exported with the given `name` and has the
    /// given `definition`.
    pub(crate) fn new(name: &'instance str, definition: Extern) -> Export<'instance> {
        Export { name, definition }
    }

    /// Returns the name by which this export is known.
    pub fn name(&self) -> &'instance str {
        self.name
    }

    /// Return the `ExternType` of this export.
    pub fn ty(&self) -> ExternType {
        self.definition.ty()
    }

    /// Consume this `Export` and return the contained `Extern`.
    pub fn into_extern(self) -> Extern {
        self.definition
    }

    /// Consume this `Export` and return the contained `Func`, if it's a function,
    /// or `None` otherwise.
    pub fn into_func(self) -> Option<Func> {
        self.definition.into_func()
    }

    /// Consume this `Export` and return the contained `Table`, if it's a table,
    /// or `None` otherwise.
    pub fn into_table(self) -> Option<Table> {
        self.definition.into_table()
    }

    /// Consume this `Export` and return the contained `Memory`, if it's a memory,
    /// or `None` otherwise.
    pub fn into_memory(self) -> Option<Memory> {
        self.definition.into_memory()
    }

    /// Consume this `Export` and return the contained `Global`, if it's a global,
    /// or `None` otherwise.
    pub fn into_global(self) -> Option<Global> {
        self.definition.into_global()
    }
}
