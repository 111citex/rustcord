//! Rustcord: asks the system's fontconfig which font has a character. Loaded at run
//! time: without it, the search falls back to opening every font.

use alloc::string::String;
use core::ffi::{c_char, c_int, c_uchar, c_void};
use std::ffi::{CStr, CString};
use std::sync::OnceLock;

extern "C" {
    fn dlopen(file: *const c_char, mode: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, name: *const c_char) -> *mut c_void;
}

type Ptr = *mut c_void;

struct Lib {
    config: usize,
    pattern_create: unsafe extern "C" fn() -> Ptr,
    pattern_destroy: unsafe extern "C" fn(Ptr),
    charset_create: unsafe extern "C" fn() -> Ptr,
    charset_destroy: unsafe extern "C" fn(Ptr),
    charset_add: unsafe extern "C" fn(Ptr, u32) -> c_int,
    charset_has: unsafe extern "C" fn(Ptr, u32) -> c_int,
    add_charset: unsafe extern "C" fn(Ptr, *const c_char, Ptr) -> c_int,
    substitute: unsafe extern "C" fn(Ptr, Ptr, c_int) -> c_int,
    default_substitute: unsafe extern "C" fn(Ptr),
    font_match: unsafe extern "C" fn(Ptr, Ptr, *mut c_int) -> Ptr,
    get_string: unsafe extern "C" fn(Ptr, *const c_char, c_int, *mut *mut c_uchar) -> c_int,
    get_integer: unsafe extern "C" fn(Ptr, *const c_char, c_int, *mut c_int) -> c_int,
    get_charset: unsafe extern "C" fn(Ptr, *const c_char, c_int, *mut Ptr) -> c_int,
}

fn load() -> Option<Lib> {
    // SAFETY: each symbol is cast to fontconfig's documented signature for it.
    unsafe {
        let handle = dlopen(c"libfontconfig.so.1".as_ptr(), 2);
        if handle.is_null() {
            return None;
        }
        macro_rules! sym {
            ($name:expr) => {{
                let found = dlsym(handle, $name.as_ptr());
                if found.is_null() {
                    return None;
                }
                core::mem::transmute(found)
            }};
        }
        let init: unsafe extern "C" fn() -> Ptr = sym!(c"FcInitLoadConfigAndFonts");
        let config = init();
        if config.is_null() {
            return None;
        }
        Some(Lib {
            config: config as usize,
            pattern_create: sym!(c"FcPatternCreate"),
            pattern_destroy: sym!(c"FcPatternDestroy"),
            charset_create: sym!(c"FcCharSetCreate"),
            charset_destroy: sym!(c"FcCharSetDestroy"),
            charset_add: sym!(c"FcCharSetAddChar"),
            charset_has: sym!(c"FcCharSetHasChar"),
            add_charset: sym!(c"FcPatternAddCharSet"),
            substitute: sym!(c"FcConfigSubstitute"),
            default_substitute: sym!(c"FcDefaultSubstitute"),
            font_match: sym!(c"FcFontMatch"),
            get_string: sym!(c"FcPatternGetString"),
            get_integer: sym!(c"FcPatternGetInteger"),
            get_charset: sym!(c"FcPatternGetCharSet"),
        })
    }
}

fn lib() -> Option<&'static Lib> {
    static LIB: OnceLock<Option<Lib>> = OnceLock::new();
    LIB.get_or_init(load).as_ref()
}

pub(crate) fn available() -> bool {
    lib().is_some()
}

/// The file and face index of a font that has `ch`, if the system has one.
pub(crate) fn font_with_char(ch: char) -> Option<(String, u32)> {
    let lib = lib()?;
    let config = lib.config as Ptr;
    // SAFETY: everything created here is destroyed here; the file name is copied out
    // before the pattern that owns it goes.
    unsafe {
        let pattern = (lib.pattern_create)();
        let wanted = (lib.charset_create)();
        (lib.charset_add)(wanted, ch as u32);
        (lib.add_charset)(pattern, c"charset".as_ptr(), wanted);
        (lib.substitute)(config, pattern, 0);
        (lib.default_substitute)(pattern);
        let mut result = 0;
        let found = (lib.font_match)(config, pattern, &mut result);
        (lib.charset_destroy)(wanted);
        (lib.pattern_destroy)(pattern);
        if found.is_null() {
            return None;
        }
        let mut answer = None;
        let mut has: Ptr = core::ptr::null_mut();
        // The closest font isn't always one with the character.
        if (lib.get_charset)(found, c"charset".as_ptr(), 0, &mut has) == 0 && (lib.charset_has)(has, ch as u32) != 0 {
            let mut file: *mut c_uchar = core::ptr::null_mut();
            let mut index = 0;
            if (lib.get_string)(found, c"file".as_ptr(), 0, &mut file) == 0 && !file.is_null() {
                (lib.get_integer)(found, c"index".as_ptr(), 0, &mut index);
                answer = Some((CStr::from_ptr(file as *const c_char).to_string_lossy().into_owned(), index as u32 & 0xffff));
            }
        }
        (lib.pattern_destroy)(found);
        let _ = CString::default();
        answer
    }
}
