//! Minimal CoreFoundation + IOKit FFI (no extra crates). Only what the macOS sources need: owned CF
//! references with RAII release, dictionary/array/number/string/data accessors and IORegistry lookups.
//!
//! Every accessor returns `Option` and checks the CF type id before casting, so an unexpected registry
//! layout degrades to "missing" instead of undefined behavior.

#![allow(non_upper_case_globals, non_camel_case_types, clippy::upper_case_acronyms)]

use std::ffi::{c_char, c_void, CStr, CString};

pub type CFTypeRef = *const c_void;
pub type CFAllocatorRef = *const c_void;
pub type CFStringRef = *const c_void;
pub type CFDictionaryRef = *const c_void;
pub type CFMutableDictionaryRef = *mut c_void;
pub type CFArrayRef = *const c_void;
pub type CFNumberRef = *const c_void;
pub type CFDataRef = *const c_void;
pub type CFBooleanRef = *const c_void;
pub type CFTypeID = usize;
pub type CFIndex = isize;
pub type CFStringEncoding = u32;
pub type CFNumberType = CFIndex;

pub type mach_port_t = u32;
pub type io_object_t = mach_port_t;
pub type io_iterator_t = io_object_t;
pub type io_registry_entry_t = io_object_t;
pub type kern_return_t = i32;
pub type IOOptionBits = u32;

pub const kCFStringEncodingUTF8: CFStringEncoding = 0x0800_0100;
pub const kCFNumberSInt64Type: CFNumberType = 4;
pub const kCFNumberFloat64Type: CFNumberType = 6;
pub const kIOMainPortDefault: mach_port_t = 0;

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    pub static kCFAllocatorDefault: CFAllocatorRef;
    pub fn CFRelease(cf: CFTypeRef);
    pub fn CFGetTypeID(cf: CFTypeRef) -> CFTypeID;
    pub fn CFStringGetTypeID() -> CFTypeID;
    pub fn CFNumberGetTypeID() -> CFTypeID;
    pub fn CFBooleanGetTypeID() -> CFTypeID;
    pub fn CFDictionaryGetTypeID() -> CFTypeID;
    pub fn CFArrayGetTypeID() -> CFTypeID;
    pub fn CFDataGetTypeID() -> CFTypeID;
    pub fn CFStringCreateWithCString(
        alloc: CFAllocatorRef,
        c: *const c_char,
        enc: CFStringEncoding,
    ) -> CFStringRef;
    pub fn CFStringGetCString(s: CFStringRef, buf: *mut c_char, size: CFIndex, enc: CFStringEncoding) -> u8;
    pub fn CFStringGetLength(s: CFStringRef) -> CFIndex;
    pub fn CFStringGetMaximumSizeForEncoding(len: CFIndex, enc: CFStringEncoding) -> CFIndex;
    pub fn CFNumberGetValue(n: CFNumberRef, t: CFNumberType, out: *mut c_void) -> u8;
    pub fn CFBooleanGetValue(b: CFBooleanRef) -> u8;
    pub fn CFDictionaryGetValue(d: CFDictionaryRef, key: *const c_void) -> *const c_void;
    pub fn CFArrayGetCount(a: CFArrayRef) -> CFIndex;
    pub fn CFArrayGetValueAtIndex(a: CFArrayRef, i: CFIndex) -> *const c_void;
    pub fn CFDataGetLength(d: CFDataRef) -> CFIndex;
    pub fn CFDataGetBytePtr(d: CFDataRef) -> *const u8;
    pub fn CFDictionaryCreateMutableCopy(
        alloc: CFAllocatorRef,
        capacity: CFIndex,
        d: CFDictionaryRef,
    ) -> CFMutableDictionaryRef;
}

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    pub fn IOServiceMatching(name: *const c_char) -> CFMutableDictionaryRef;
    pub fn IOServiceNameMatching(name: *const c_char) -> CFMutableDictionaryRef;
    pub fn IOServiceGetMatchingServices(
        main: mach_port_t,
        matching: CFDictionaryRef,
        it: *mut io_iterator_t,
    ) -> kern_return_t;
    pub fn IOIteratorNext(it: io_iterator_t) -> io_object_t;
    pub fn IOObjectRelease(o: io_object_t) -> kern_return_t;
    pub fn IORegistryEntryCreateCFProperty(
        entry: io_registry_entry_t,
        key: CFStringRef,
        alloc: CFAllocatorRef,
        options: IOOptionBits,
    ) -> CFTypeRef;
    // Power sources (IOKit/ps/IOPowerSources.h).
    pub fn IOPSCopyPowerSourcesInfo() -> CFTypeRef;
    pub fn IOPSCopyPowerSourcesList(blob: CFTypeRef) -> CFArrayRef;
    pub fn IOPSGetPowerSourceDescription(blob: CFTypeRef, ps: CFTypeRef) -> CFDictionaryRef;
    pub fn IOPSGetProvidingPowerSourceType(blob: CFTypeRef) -> CFStringRef;
    pub fn IOPSCopyExternalPowerAdapterDetails() -> CFDictionaryRef;
}

/// An owned (+1) CF reference, released on drop.
pub struct Cf(CFTypeRef);

// SAFETY: CF objects used here are immutable snapshots; ownership is unique to this wrapper.
unsafe impl Send for Cf {}

impl Cf {
    /// Wraps a +1 reference from a `Create`/`Copy` function; `None` for null.
    pub fn owned(p: CFTypeRef) -> Option<Cf> {
        // Not `then_some(Cf(p))`: that builds the wrapper eagerly and drops it for a null pointer, i.e.
        // CFRelease(NULL), which traps (SIGTRAP). It crashed oomtop on battery power, where
        // `IOPSCopyExternalPowerAdapterDetails` returns null.
        if p.is_null() {
            None
        } else {
            Some(Cf(p))
        }
    }
    pub fn as_ptr(&self) -> CFTypeRef {
        self.0
    }
}

impl Drop for Cf {
    fn drop(&mut self) {
        // SAFETY: we hold one reference.
        unsafe { CFRelease(self.0) }
    }
}

/// Creates a CFString (owned).
pub fn cfstr(s: &str) -> Option<Cf> {
    let c = CString::new(s).ok()?;
    // SAFETY: valid C string.
    Cf::owned(unsafe { CFStringCreateWithCString(kCFAllocatorDefault, c.as_ptr(), kCFStringEncodingUTF8) })
}

fn type_is(p: CFTypeRef, id: CFTypeID) -> bool {
    // SAFETY: p is a non-null CF object.
    !p.is_null() && unsafe { CFGetTypeID(p) } == id
}

pub fn is_dict(p: CFTypeRef) -> bool {
    // SAFETY: plain type-id query.
    type_is(p, unsafe { CFDictionaryGetTypeID() })
}

pub fn is_array(p: CFTypeRef) -> bool {
    // SAFETY: plain type-id query.
    type_is(p, unsafe { CFArrayGetTypeID() })
}

/// CFString → Rust string (borrowed ref).
pub fn string(p: CFTypeRef) -> Option<String> {
    // SAFETY: type checked before use; buffer sized per CF's max-size query.
    unsafe {
        if !type_is(p, CFStringGetTypeID()) {
            return None;
        }
        let len = CFStringGetLength(p);
        let max = CFStringGetMaximumSizeForEncoding(len, kCFStringEncodingUTF8) + 1;
        let mut buf = vec![0 as c_char; max.max(1) as usize];
        if CFStringGetCString(p, buf.as_mut_ptr(), max, kCFStringEncodingUTF8) == 0 {
            return None;
        }
        Some(CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned())
    }
}

/// CFNumber / CFBoolean → i64 (borrowed ref). Floating numbers are truncated.
pub fn int(p: CFTypeRef) -> Option<i64> {
    // SAFETY: type checked; out pointers are valid.
    unsafe {
        if type_is(p, CFBooleanGetTypeID()) {
            return Some(CFBooleanGetValue(p) as i64);
        }
        if !type_is(p, CFNumberGetTypeID()) {
            return None;
        }
        let mut v: i64 = 0;
        if CFNumberGetValue(p, kCFNumberSInt64Type, &mut v as *mut i64 as *mut c_void) != 0 {
            return Some(v);
        }
        let mut f: f64 = 0.0;
        (CFNumberGetValue(p, kCFNumberFloat64Type, &mut f as *mut f64 as *mut c_void) != 0)
            .then_some(f as i64)
    }
}

/// CFData → bytes (borrowed ref).
pub fn data(p: CFTypeRef) -> Option<Vec<u8>> {
    // SAFETY: type checked; CF guarantees `len` readable bytes at the pointer.
    unsafe {
        if !type_is(p, CFDataGetTypeID()) {
            return None;
        }
        let len = CFDataGetLength(p).max(0) as usize;
        let ptr = CFDataGetBytePtr(p);
        if ptr.is_null() {
            return Some(Vec::new());
        }
        Some(std::slice::from_raw_parts(ptr, len).to_vec())
    }
}

/// `dict[key]` (borrowed, +0). `None` when `dict` is not a dictionary or the key is absent.
pub fn dict_get(dict: CFTypeRef, key: &str) -> Option<CFTypeRef> {
    if !is_dict(dict) {
        return None;
    }
    let k = cfstr(key)?;
    // SAFETY: dict is a CFDictionary, k a CFString.
    let v = unsafe { CFDictionaryGetValue(dict, k.as_ptr()) };
    (!v.is_null()).then_some(v)
}

pub fn dict_int(dict: CFTypeRef, key: &str) -> Option<i64> {
    dict_get(dict, key).and_then(int)
}

pub fn dict_string(dict: CFTypeRef, key: &str) -> Option<String> {
    dict_get(dict, key).and_then(string)
}

/// Elements of a CFArray (borrowed refs).
pub fn array_items(arr: CFTypeRef) -> Vec<CFTypeRef> {
    if !is_array(arr) {
        return Vec::new();
    }
    // SAFETY: arr is a CFArray; indices are in range.
    unsafe {
        let n = CFArrayGetCount(arr).max(0);
        (0..n).map(|i| CFArrayGetValueAtIndex(arr, i)).collect()
    }
}

/// An owned IOKit object, released on drop.
pub struct IoObject(pub io_object_t);

impl Drop for IoObject {
    fn drop(&mut self) {
        if self.0 != 0 {
            // SAFETY: we own this object reference.
            unsafe { IOObjectRelease(self.0) };
        }
    }
}

/// All services matching `IOServiceMatching(class)` (or `IOServiceNameMatching(name)` when `by_name`).
pub fn services(class_or_name: &str, by_name: bool) -> Vec<IoObject> {
    let Ok(c) = CString::new(class_or_name) else {
        return Vec::new();
    };
    // SAFETY: matching dictionary is consumed by IOServiceGetMatchingServices (it releases it).
    unsafe {
        let m = if by_name {
            IOServiceNameMatching(c.as_ptr())
        } else {
            IOServiceMatching(c.as_ptr())
        };
        if m.is_null() {
            return Vec::new();
        }
        let mut it: io_iterator_t = 0;
        if IOServiceGetMatchingServices(kIOMainPortDefault, m as CFDictionaryRef, &mut it) != 0 || it == 0 {
            return Vec::new();
        }
        let it = IoObject(it);
        let mut out = Vec::new();
        loop {
            let o = IOIteratorNext(it.0);
            if o == 0 {
                break;
            }
            out.push(IoObject(o));
            if out.len() > 64 {
                break;
            }
        }
        out
    }
}

/// One registry property (owned).
pub fn registry_property(entry: &IoObject, key: &str) -> Option<Cf> {
    let k = cfstr(key)?;
    // SAFETY: valid entry and key; returns +1 or null.
    Cf::owned(unsafe { IORegistryEntryCreateCFProperty(entry.0, k.as_ptr(), kCFAllocatorDefault, 0) })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A null "Copy" result is `None` and is never released (CFRelease(NULL) traps: oomtop crashed with
    /// SIGTRAP on battery power, where the power-adapter details are null).
    #[test]
    fn null_copy_results_are_none_and_never_released() {
        assert!(Cf::owned(std::ptr::null()).is_none());
        // SAFETY: returns +1 or null (null on battery power).
        let ad = Cf::owned(unsafe { IOPSCopyExternalPowerAdapterDetails() as CFTypeRef });
        drop(ad);
    }

    #[test]
    fn strings_and_type_checks() {
        let s = cfstr("héllo").unwrap();
        assert_eq!(string(s.as_ptr()).as_deref(), Some("héllo"));
        assert_eq!(int(s.as_ptr()), None);
        assert!(!is_dict(s.as_ptr()));
        assert!(array_items(s.as_ptr()).is_empty());
        assert!(dict_get(s.as_ptr(), "x").is_none());
    }

    #[test]
    fn registry_lookup_degrades() {
        assert!(services("NoSuchIOKitClassForOomtop", false).is_empty());
    }
}
