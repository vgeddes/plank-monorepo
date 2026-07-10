use hashbrown::{DefaultHashBuilder, HashTable, hash_table::Entry};
use plank_core::{IndexVec, chunked_arena::ChunkedArena, newtype_index};
use std::{cell::RefCell, hash::BuildHasher};

newtype_index! {
    /// Index into the combined backing store. Kept private so that `StrId` /
    /// `BytesId` can only be minted inside this module.
    struct InternIdx;
}

/// Index of an interned string.
///
/// Sealed: outside this module it can only be obtained from
/// [`Interner::intern_str`] (or the built-in known name consts, which are validated
/// against the interner on session construction). This guarantees the indexed
/// content is valid UTF-8 as long as the id is resolved against the interner
/// that minted it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StrId(InternIdx);

/// Index of an interned byte string (arbitrary, not necessarily UTF-8).
///
/// Sealed: outside this module it can only be obtained from
/// [`Interner::intern_bytes`] or by converting a [`StrId`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BytesId(InternIdx);

/// Id of the empty byte string, validated on [`Interner`] construction.
pub const EMPTY_BYTES: BytesId = BytesId(InternIdx::new(0));

impl StrId {
    /// Mints a `StrId` without interning anything.
    ///
    /// Only sound for the built-in known name table: `inject_builtins` asserts that
    /// every const produced this way matches the id the session's interner
    /// actually assigns to the corresponding name.
    pub(crate) const fn from_builtin_index(raw: u32) -> Self {
        Self(InternIdx::new(raw))
    }
}

impl From<StrId> for BytesId {
    fn from(id: StrId) -> Self {
        Self(id.0)
    }
}

pub struct Interner {
    arena: ChunkedArena<1>,
    inner: RefCell<InternerInner>,
}

struct InternerInner {
    entries: IndexVec<InternIdx, ArenaEntry>,
    map: HashTable<InternIdx>,
    hasher: DefaultHashBuilder,
}

#[derive(Clone, Copy)]
struct ArenaEntry {
    offset: u32,
    len: u32,
}

impl Interner {
    pub fn new() -> Self {
        let this = Self {
            arena: ChunkedArena::new(),
            inner: RefCell::new(InternerInner {
                entries: IndexVec::new(),
                map: HashTable::new(),
                hasher: DefaultHashBuilder::default(),
            }),
        };
        assert_eq!(this.intern_bytes(b""), EMPTY_BYTES);
        this
    }

    pub fn intern_str(&self, string: &str) -> StrId {
        StrId(self.intern(string.as_bytes()))
    }

    pub fn intern_bytes(&self, bytes: &[u8]) -> BytesId {
        BytesId(self.intern(bytes))
    }

    fn intern(&self, bytes: &[u8]) -> InternIdx {
        let mut inner = self.inner.borrow_mut();
        let InternerInner { entries, map, hasher } = &mut *inner;
        let entry = map.entry(
            hasher.hash_one(bytes),
            |&id| Self::entry_bytes(&self.arena, entries[id]) == bytes,
            |&id| hasher.hash_one(Self::entry_bytes(&self.arena, entries[id])),
        );

        match entry {
            Entry::Occupied(occupied) => *occupied.get(),
            Entry::Vacant(vacant) => {
                let len = u32::try_from(bytes.len()).expect("interned byte string too large");
                // Allocate at least one byte so even the empty entry has a non-null pointer
                // that can be passed to `slice::from_raw_parts` during lookup.
                let (offset, dst) = unsafe { self.arena.alloc_append(bytes.len().max(1)) };
                // Safety: `alloc_append` returned storage for at least `bytes.len()` bytes. The
                // new allocation does not overlap the input slice and has not been published yet.
                unsafe { dst.copy_from_nonoverlapping(bytes.as_ptr(), bytes.len()) };

                let id = entries.push(ArenaEntry { offset, len });
                vacant.insert(id);
                id
            }
        }
    }

    fn entry_bytes(arena: &ChunkedArena<1>, entry: ArenaEntry) -> &[u8] {
        // Safety: Every `ArenaEntry` is created from an allocation in `arena`; `entry.len` bytes
        // are initialized before the entry is published. Entries and their bytes are append-only,
        // the arena is never reset, and chunk addresses remain stable. The returned slice's
        // lifetime is tied to the arena borrow.
        unsafe { std::slice::from_raw_parts(arena.get(entry.offset), entry.len as usize) }
    }

    /// # Safety
    ///
    /// `id` must originate from this interner. A `StrId` minted by a
    /// *different* interner may index content that is not valid UTF-8.
    pub unsafe fn lookup_str(&self, id: StrId) -> &str {
        let entry = self.inner.borrow().entries[id.0];
        let bytes = Self::entry_bytes(&self.arena, entry);
        unsafe { std::str::from_utf8_unchecked(bytes) }
    }

    pub fn lookup_bytes(&self, id: BytesId) -> &[u8] {
        let entry = self.inner.borrow().entries[id.0];
        Self::entry_bytes(&self.arena, entry)
    }
}

impl Default for Interner {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_interning_and_retrieval() {
        let interner = Interner::new();

        let id1 = interner.intern_bytes(b"hello");
        let id2 = interner.intern_bytes(b"world");
        let id3 = interner.intern_bytes(b"");

        assert_eq!(interner.lookup_bytes(id1), b"hello");
        assert_eq!(interner.lookup_bytes(id2), b"world");
        assert_eq!(interner.lookup_bytes(id3), b"");
    }

    #[test]
    fn duplicate_contents_return_same_id() {
        let interner = Interner::new();

        let id1 = interner.intern_bytes(b"duplicate");
        let id2 = interner.intern_bytes(b"other");
        let id3 = interner.intern_bytes(b"duplicate");

        assert_eq!(id1, id3);
        assert_ne!(id1, id2);
        assert_eq!(interner.lookup_bytes(id1), interner.lookup_bytes(id3));
    }

    #[test]
    fn lookup_references_survive_arena_growth() {
        let interner = Interner::new();
        let hello_id = interner.intern_str("hello");
        let hello = unsafe { interner.lookup_str(hello_id) };

        for i in 0..1_000 {
            interner.intern_str(&format!("a distinct interned string number {i}"));
        }

        assert_eq!(hello, "hello");
        assert_eq!(unsafe { interner.lookup_str(hello_id) }, hello);
    }
}
